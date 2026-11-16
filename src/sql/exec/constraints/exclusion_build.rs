//! Exclusion constraint builds. PostgreSQL builds the constraint's index,
//! then checks each live row in heap order against the index
//! (`IndexCheckExclusion`), so the error names the first row in heap order
//! that conflicts with any other, and a row it conflicts with. Comparing
//! every pair of rows finds that row in O(n²); here equality elements group
//! the rows by sorting and a range-overlap element is swept in lower-bound
//! order, so the build makes O(n log n) comparisons. Other operators keep
//! the pairwise check. The partner reported is the first conflicting row in
//! heap order, which is PostgreSQL's B-tree index order among equal keys.

use core::cell::Cell;
use core::cmp::Ordering;

use crate::mem::arena::Arena;
use crate::sql::ast::BinaryOp;
use crate::sql::eval::{SqlError, compare_datums_collated, sqlstate};
use crate::sql::types::Datum;
use crate::sql_err;
use crate::storage::{MAX_INDEX_COLS, Storage};

use super::build_sort::BuildRows;
use super::{ExclusionKeys, ExclusionScan};

/// One checked row's encoded element values in the build's key buffer.
#[derive(Clone, Copy)]
struct Entry {
    offset: u32,
    length: u32,
}

/// How the build finds conflicting rows.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Strategy {
    /// Equality elements, and at most one range-overlap element.
    Sorted { overlap: Option<usize> },
    /// Another operator: every pair of rows.
    Pairwise,
}

/// Checks the exclusion constraint at `position` of `def` over `rows`, as
/// building its index does, raising PostgreSQL's 23P01 for the first row in
/// heap order that conflicts with another.
#[allow(clippy::too_many_arguments)]
pub(crate) fn check_exclusion_build<B: crate::storage::DefinitionBacking>(
    storage: &Storage,
    table_index: usize,
    txid: u32,
    def: &crate::storage::TableDefinition<B>,
    position: usize,
    rows: &mut BuildRows<'_>,
    arena: &Arena,
) -> Result<(), SqlError> {
    let exclusion = &def.exclusions()[position];
    let scan = ExclusionScan::new(exclusion, storage, txid, def, arena)?;
    let predicate = super::exclusion_predicate(storage, txid, def, exclusion, arena)?;
    let n = exclusion.len();
    let full = || {
        sql_err!(
            sqlstate::PROGRAM_LIMIT_EXCEEDED,
            "the rows of exclusion constraint \"{}\" exceed the statement arena",
            exclusion.name.as_str()
        )
    };
    let changed = || {
        sql_err!(
            sqlstate::SERIALIZATION_FAILURE,
            "exclusion constraint build rows changed while checking"
        )
    };
    // The rows the constraint covers, in heap order, and their encoded
    // element values.
    let mut count = 0usize;
    let mut key_bytes = 0usize;
    rows(true, &mut |_, values, _| {
        let length = arena
            .with_remaining_region(|region| -> Result<Option<usize>, SqlError> {
                if let Some(predicate) = predicate
                    && !super::index_predicate_matches(
                        storage, txid, def, values, predicate, region,
                    )?
                {
                    return Ok(None);
                }
                let keys = super::index_key_values(
                    storage,
                    txid,
                    def,
                    values,
                    &scan.columns[..n],
                    &scan.expressions[..n],
                    region,
                )?;
                Ok(Some(crate::sql::exec::projected_row_len(&keys[..n])?))
            })
            .map_err(|_| full())??;
        if let Some(length) = length {
            count += 1;
            key_bytes = key_bytes.checked_add(length).ok_or_else(full)?;
        }
        Ok(())
    })?;
    if count < 2 {
        return Ok(());
    }
    let entries = arena
        .alloc_slice_with(count, |_| Entry {
            offset: 0,
            length: 0,
        })
        .map_err(|_| full())?;
    let buffer = arena
        .alloc_slice_with(key_bytes, |_| 0u8)
        .map_err(|_| full())?;
    let mut filled = 0usize;
    let mut written = 0usize;
    let mark = arena.mark();
    let filling = rows(true, &mut |_, values, _| {
        let entry = arena
            .with_remaining_region(|region| -> Result<Option<Entry>, SqlError> {
                if let Some(predicate) = predicate
                    && !super::index_predicate_matches(
                        storage, txid, def, values, predicate, region,
                    )?
                {
                    return Ok(None);
                }
                let keys = super::index_key_values(
                    storage,
                    txid,
                    def,
                    values,
                    &scan.columns[..n],
                    &scan.expressions[..n],
                    region,
                )?;
                let length = crate::sql::exec::encode_projected_into(
                    &keys[..n],
                    buffer.get_mut(written..).ok_or_else(changed)?,
                )?;
                Ok(Some(Entry {
                    offset: u32::try_from(written).map_err(|_| full())?,
                    length: u32::try_from(length).map_err(|_| full())?,
                }))
            })
            .map_err(|_| full())??;
        let Some(entry) = entry else {
            return Ok(());
        };
        *entries.get_mut(filled).ok_or_else(changed)? = entry;
        filled += 1;
        written += entry.length as usize;
        Ok(())
    });
    // SAFETY: the row source's scan state is consumed by the pass; entries
    // and keys precede the mark.
    unsafe { arena.rewind_to(mark) };
    filling?;
    if filled != count {
        return Err(changed());
    }
    let buffer: &[u8] = buffer;
    let entries: &[Entry] = entries;
    // Runs `with` over row `row`'s element values, decoded into scratch the
    // arena drops afterwards.
    let with_keys = |row: usize,
                     with: &mut dyn FnMut(&ExclusionKeys<'_>) -> Result<(), SqlError>|
     -> Result<(), SqlError> {
        let mark = arena.mark();
        let result = (|| {
            let entry = entries[row];
            let bytes = &buffer[entry.offset as usize..][..entry.length as usize];
            let mut keys: ExclusionKeys<'_> = [Datum::Null; MAX_INDEX_COLS];
            crate::sql::exec::decode_projected_row_into(bytes, arena, &mut keys[..n])?;
            with(&keys)
        })();
        // SAFETY: the decoded values do not outlive `with`.
        unsafe { arena.rewind_to(mark) };
        result
    };
    let conflicts = |a: usize, b: usize| -> Result<bool, SqlError> {
        let mut conflict = false;
        with_keys(a, &mut |left| {
            with_keys(b, &mut |right| {
                let left: ExclusionKeys<'_> = *left;
                let right: ExclusionKeys<'_> = *right;
                let mark = arena.mark();
                let result = (|| {
                    // The operators read values the arena already holds.
                    let left = detached(&left[..n], arena)?;
                    let right = detached(&right[..n], arena)?;
                    scan.conflicts(storage, txid, &left, &right, arena)
                })();
                // SAFETY: only the verdict leaves.
                unsafe { arena.rewind_to(mark) };
                conflict = result?;
                Ok(())
            })
        })?;
        Ok(conflict)
    };
    // The first row in heap order that conflicts with another.
    let first = match strategy(&scan, n) {
        Strategy::Pairwise => {
            let mut first = None;
            'rows: for a in 0..count {
                for b in 0..count {
                    if a != b && conflicts(a, b)? {
                        first = Some(a);
                        break 'rows;
                    }
                }
            }
            first
        }
        Strategy::Sorted { overlap } => sorted_first(
            storage, txid, &scan, n, overlap, count, &with_keys, &conflicts, arena,
        )?,
    };
    let Some(first) = first else {
        return Ok(());
    };
    let mut partner = None;
    for other in 0..count {
        if other != first && conflicts(first, other)? {
            partner = Some(other);
            break;
        }
    }
    let partner = partner.ok_or_else(|| {
        sql_err!(
            sqlstate::INTERNAL_ERROR,
            "exclusion constraint build found a conflict without a partner"
        )
    })?;
    let mut error = None;
    with_keys(first, &mut |values| {
        with_keys(partner, &mut |other| {
            let values: ExclusionKeys<'_> = *values;
            let other: ExclusionKeys<'_> = *other;
            let values = detached(&values[..n], arena)?;
            let other = detached(&other[..n], arena)?;
            error = Some(super::index_build_exclusion_violation(
                &scan,
                storage,
                txid,
                table_index,
                def,
                &values,
                &other,
                arena,
            ));
            Ok(())
        })
    })?;
    Err(error.expect("the conflict is described"))
}

/// Element values copied into the arena with its lifetime.
fn detached<'a>(values: &[Datum<'_>], arena: &'a Arena) -> Result<ExclusionKeys<'a>, SqlError> {
    let mut keys: ExclusionKeys<'a> = [Datum::Null; MAX_INDEX_COLS];
    for (slot, value) in keys.iter_mut().zip(values) {
        *slot = crate::sql::exec::detach_routine_datum(*value, arena)?;
    }
    Ok(keys)
}

/// A range element's value as its text and kind.
fn range_of<'v>(value: &Datum<'v>) -> Result<(&'v str, crate::sql::types::RangeKind), SqlError> {
    match *value {
        Datum::Range { text, kind } => Ok((text, kind)),
        _ => Err(sql_err!(
            sqlstate::DATATYPE_MISMATCH,
            "an overlap exclusion element is not a range"
        )),
    }
}

/// Runs a visit over one checked row's element values.
type WithKeys<'w> = dyn Fn(usize, &mut dyn FnMut(&ExclusionKeys<'_>) -> Result<(), SqlError>) -> Result<(), SqlError>
    + 'w;

/// How the elements' operators let the build find conflicts.
fn strategy(scan: &ExclusionScan<'_, '_>, n: usize) -> Strategy {
    let mut overlap = None;
    for (position, operator) in scan.exclusion.operators()[..n].iter().enumerate() {
        match operator.binary() {
            // A catalog collation's equality is the collation's.
            Some(BinaryOp::Eq) => {}
            Some(BinaryOp::Overlaps) if overlap.is_none() => overlap = Some(position),
            _ => return Strategy::Pairwise,
        }
    }
    Strategy::Sorted { overlap }
}

/// The first row in heap order with a conflict, found by sorting the rows
/// by their equality elements, then by the overlap element's lower bound:
/// equal rows conflict, and within a run of equal rows a range overlaps
/// another exactly when it overlaps the next one or the earlier one that
/// reaches furthest.
#[allow(clippy::too_many_arguments)]
fn sorted_first(
    storage: &Storage,
    txid: u32,
    scan: &ExclusionScan<'_, '_>,
    n: usize,
    overlap: Option<usize>,
    count: usize,
    with_keys: &WithKeys<'_>,
    conflicts: &dyn Fn(usize, usize) -> Result<bool, SqlError>,
    arena: &Arena,
) -> Result<Option<usize>, SqlError> {
    let full = || crate::sql::eval::arena_full();
    // Rows that can conflict at all: no NULL element, no empty range.
    let mut candidates = 0usize;
    let eligible = arena
        .alloc_slice_with(count, |_| false)
        .map_err(|_| full())?;
    for (row, slot) in eligible.iter_mut().enumerate() {
        with_keys(row, &mut |keys| {
            *slot = keys[..n]
                .iter()
                .enumerate()
                .all(|(position, value)| match value {
                    Datum::Null => false,
                    Datum::Range { text, .. } if Some(position) == overlap => {
                        !crate::sql::range::is_empty(text)
                    }
                    _ => true,
                });
            Ok(())
        })?;
        candidates += usize::from(*slot);
    }
    let order = arena
        .alloc_slice_with(candidates, |_| 0u32)
        .map_err(|_| full())?;
    let mut at = 0usize;
    for (row, eligible) in eligible.iter().enumerate() {
        if *eligible {
            order[at] = row as u32;
            at += 1;
        }
    }
    let compare_rows = |a: usize, b: usize, ranges: bool| -> Result<Ordering, SqlError> {
        let mut ordering = Ordering::Equal;
        with_keys(a, &mut |left| {
            with_keys(b, &mut |right| {
                for position in 0..n {
                    if Some(position) == overlap {
                        continue;
                    }
                    ordering = compare_datums_collated(
                        storage,
                        txid,
                        scan.collations[position],
                        &left[position],
                        &right[position],
                    )?;
                    if ordering != Ordering::Equal {
                        return Ok(());
                    }
                }
                if ranges && let Some(position) = overlap {
                    let (left, kind) = range_of(&left[position])?;
                    let (right, _) = range_of(&right[position])?;
                    ordering = crate::sql::range::cmp_ranges(left, right, kind)?;
                }
                Ok(())
            })
        })?;
        Ok(ordering)
    };
    let failed: Cell<Option<SqlError>> = Cell::new(None);
    order.sort_unstable_by(|&a, &b| {
        match compare_rows(a as usize, b as usize, true) {
            // Ties keep heap order, which the run sweep does not need but
            // makes the comparison total.
            Ok(Ordering::Equal) => a.cmp(&b),
            Ok(ordering) => ordering,
            Err(error) => {
                failed.set(Some(error));
                Ordering::Equal
            }
        }
    });
    if let Some(error) = failed.take() {
        return Err(error);
    }
    let flagged = arena
        .alloc_slice_with(count, |_| false)
        .map_err(|_| full())?;
    let mut start = 0usize;
    while start < order.len() {
        let mut end = start + 1;
        while end < order.len()
            && compare_rows(order[start] as usize, order[end] as usize, false)? == Ordering::Equal
        {
            end += 1;
        }
        let run = &order[start..end];
        match overlap {
            None if run.len() > 1 => {
                for &row in run {
                    flagged[row as usize] = true;
                }
            }
            None => {}
            Some(position) => {
                // The earlier row of the run whose range reaches furthest.
                let mut reach: Option<u32> = None;
                for (index, &row) in run.iter().enumerate() {
                    let row_index = row as usize;
                    if let Some(&next) = run.get(index + 1)
                        && conflicts(row_index, next as usize)?
                    {
                        flagged[row_index] = true;
                        flagged[next as usize] = true;
                    }
                    if let Some(furthest) = reach
                        && conflicts(row_index, furthest as usize)?
                    {
                        flagged[row_index] = true;
                        flagged[furthest as usize] = true;
                    }
                    let further = match reach {
                        None => true,
                        Some(furthest) => {
                            let mut ordering = Ordering::Equal;
                            with_keys(row_index, &mut |left| {
                                with_keys(furthest as usize, &mut |right| {
                                    let (left, kind) = range_of(&left[position])?;
                                    let (right, _) = range_of(&right[position])?;
                                    ordering =
                                        crate::sql::range::cmp_upper_bounds(left, right, kind)?;
                                    Ok(())
                                })
                            })?;
                            ordering == Ordering::Greater
                        }
                    };
                    if further {
                        reach = Some(row);
                    }
                }
            }
        }
        start = end;
    }
    Ok(flagged.iter().position(|&flag| flag))
}

#[cfg(all(test, not(feature = "sql-fixture-suite")))]
mod tests {
    use super::*;

    #[test]
    fn upper_bounds_order_an_infinite_bound_last() {
        let kind = crate::sql::types::RangeKind::Int4;
        assert_eq!(
            crate::sql::range::cmp_upper_bounds("[1,5)", "[2,4)", kind).unwrap(),
            Ordering::Greater
        );
        assert_eq!(
            crate::sql::range::cmp_upper_bounds("[1,)", "[2,4)", kind).unwrap(),
            Ordering::Greater
        );
        assert_eq!(
            crate::sql::range::cmp_upper_bounds("[1,4]", "[2,4)", kind).unwrap(),
            Ordering::Greater
        );
    }
}
