//! The duplicate a unique B-tree index build reports. PostgreSQL feeds the
//! heap's live rows, in heap order, to a tuplesort whose comparator raises
//! the unique violation for the first pair of equal keys it compares
//! (`comparetup_index_btree_tiebreak`). Which pair that is depends on the
//! in-memory quicksort's exact comparison order (`sort_template.h`), ported
//! here. A build PostgreSQL would sort in parallel or across runs on disk
//! compares other pairs, so its duplicate is refused rather than misnamed.
//! Rows enter in a sequential scan's physical order, pos3ql's heap order.

use core::cmp::Ordering;

use crate::mem::arena::Arena;
use crate::sql::ast::{Collation, Expr};
use crate::sql::eval::{SqlError, compare_datums_collated, sqlstate};
use crate::sql::types::Datum;
use crate::sql_err;
use crate::storage::{IndexOperatorClass, MAX_INDEX_COLS, SqlName, Storage};

use super::{DuplicateKey, RowKey};

/// PostgreSQL's default `maintenance_work_mem` (64 MB), which bounds an index
/// build's sort; pos3ql has no such setting.
const MAINTENANCE_WORK_MEM: i64 = 64 * 1024 * 1024;
/// `INITIAL_MEMTUPSIZE` and `sizeof(SortTuple)` on a 64-bit build.
const INITIAL_MEMTUPSIZE: i64 = 1024;
const SORT_TUPLE_BYTES: i64 = 24;
/// The header of an allocation-set chunk large enough for its own block.
const CHUNK_HEADER_BYTES: i64 = 8;
/// `min_parallel_table_scan_size`'s default in heap pages: from that heap size
/// PostgreSQL's default settings build an index with a parallel worker.
const PARALLEL_BUILD_PAGES: u64 = 1024;
/// A heap page's space for tuples, after its header.
const HEAP_PAGE_BYTES: u64 = 8192 - 24;
/// The largest key and covering values one entry copies.
const ENTRY_SCRATCH_BYTES: usize = 64 * 1024;

const fn maxalign(bytes: u64) -> u64 {
    (bytes + 7) & !7
}

/// How one key column orders its values.
#[derive(Clone, Copy)]
enum KeyOrder {
    Collated(Collation),
    /// A pattern operator class compares bytes, whatever the collation.
    Bytes,
    /// A catalog operator class's support function 1.
    Function(i32),
}

/// One key column: its order, direction and NULL placement.
#[derive(Clone, Copy)]
struct KeyColumn {
    order: KeyOrder,
    descending: bool,
    nulls_first: bool,
}

/// The unique key whose index a build sorts.
struct BuildKey<'a> {
    name: SqlName,
    columns: [u16; MAX_INDEX_COLS],
    expressions: [Option<&'a Expr<'a>>; MAX_INDEX_COLS],
    expression_texts: [Option<&'a str>; MAX_INDEX_COLS],
    order: [KeyColumn; MAX_INDEX_COLS],
    n_cols: usize,
    include_columns: [u16; MAX_INDEX_COLS],
    n_include_cols: usize,
    nulls_not_distinct: bool,
    predicate: Option<&'a Expr<'a>>,
}

impl<'a> BuildKey<'a> {
    fn plain(name: SqlName, columns: &[u16], collations: &[Collation], distinct: bool) -> Self {
        let mut key = Self {
            name,
            columns: [0; MAX_INDEX_COLS],
            expressions: [None; MAX_INDEX_COLS],
            expression_texts: [None; MAX_INDEX_COLS],
            order: [KeyColumn {
                order: KeyOrder::Bytes,
                descending: false,
                nulls_first: false,
            }; MAX_INDEX_COLS],
            n_cols: columns.len(),
            include_columns: [0; MAX_INDEX_COLS],
            n_include_cols: 0,
            nulls_not_distinct: !distinct,
            predicate: None,
        };
        key.columns[..columns.len()].copy_from_slice(columns);
        for (position, collation) in collations.iter().enumerate() {
            key.order[position].order = KeyOrder::Collated(*collation);
        }
        key
    }

    fn index(
        storage: &Storage,
        txid: u32,
        index: &crate::storage::IndexDef,
        arena: &'a Arena,
    ) -> Result<Self, SqlError> {
        let mut key = Self::plain(
            index.name_for(txid),
            &index.columns[..index.n_cols],
            &index.collations[..index.n_cols],
            !index.nulls_not_distinct,
        );
        let classes = super::resolve_index_operator_classes(index)?;
        for position in 0..index.n_cols {
            if let Some(source) = &index.expressions[position] {
                let source = arena
                    .alloc_str(source.as_str())
                    .map_err(|_| crate::sql::eval::arena_full())?;
                key.expression_texts[position] = Some(source);
                key.expressions[position] =
                    Some(super::super::expression_bindings::bound_index_expression(
                        source,
                        storage,
                        index,
                        crate::storage::ExpressionOwner::IndexKey(position as u8),
                        txid,
                        arena,
                    )?);
            }
            key.order[position] = KeyColumn {
                order: match classes[position].expect("index operator class resolved") {
                    IndexOperatorClass::Btree(class) if class.is_pattern() => KeyOrder::Bytes,
                    IndexOperatorClass::Btree(_) => KeyOrder::Collated(index.collations[position]),
                    IndexOperatorClass::Catalog(oid) => {
                        let slot =
                            storage
                                .operator_class_slot_by_oid(oid, txid)
                                .ok_or_else(|| {
                                    sql_err!(
                                        sqlstate::UNDEFINED_OBJECT,
                                        "index operator class does not exist"
                                    )
                                })?;
                        KeyOrder::Function(
                            storage
                                .operator_class_for(slot, txid)
                                .comparison_function()
                                .ok_or_else(|| {
                                    sql_err!(
                                        sqlstate::UNDEFINED_OBJECT,
                                        "missing support function 1 for index operator class"
                                    )
                                })?
                                .function,
                        )
                    }
                    _ => {
                        return Err(sql_err!(
                            sqlstate::INTERNAL_ERROR,
                            "unique index key has no B-tree operator class"
                        ));
                    }
                },
                descending: index.descending[position],
                nulls_first: index.nulls_first[position],
            };
        }
        key.include_columns[..index.n_include_cols]
            .copy_from_slice(&index.include_columns[..index.n_include_cols]);
        key.n_include_cols = index.n_include_cols;
        key.predicate = index
            .predicate
            .map(|source| {
                super::super::expression_bindings::bound_index_expression(
                    source.as_str(),
                    storage,
                    index,
                    crate::storage::ExpressionOwner::IndexPredicate,
                    txid,
                    arena,
                )
            })
            .transpose()?;
        Ok(key)
    }
}

/// One live row's index entry: its key values and the bytes PostgreSQL's
/// sort accounts for its index tuple, at most. Entries are in heap order.
#[derive(Clone, Copy)]
struct Entry<'a> {
    keys: &'a [Datum<'a>],
    tuple_bytes: u64,
}

/// Visits one live row the build indexes: its row identifier, its values and
/// its stored size.
pub(crate) type BuildRowVisit<'v> =
    dyn FnMut(u64, &[Datum<'_>], usize) -> Result<(), SqlError> + 'v;

/// Visits each live row the build indexes, in heap order.
type BuildRows<'r> = dyn FnMut(&mut BuildRowVisit<'_>) -> Result<(), SqlError> + 'r;

/// The error PostgreSQL raises building the unique index or key of `def`
/// whose index has OID `oid` over `rows`, when its sort finds a duplicate.
/// `None` when no unique B-tree key has that OID.
pub(crate) fn sorted_build_violation<B: crate::storage::DefinitionBacking>(
    storage: &Storage,
    table_index: usize,
    txid: u32,
    def: &crate::storage::TableDefinition<B>,
    oid: i32,
    rows: &mut BuildRows<'_>,
    arena: &Arena,
) -> Result<Option<SqlError>, SqlError> {
    let mut found = None;
    super::for_each_row_key(storage, def, txid, &mut |key| {
        if found.is_some() {
            return Ok(());
        }
        let identity_oid =
            |identity: Option<crate::storage::RelationIdentity>| identity.map(|i| i.oid());
        let key = match key {
            RowKey::Column(column)
                if identity_oid(def.column_index_identity(column)) == Some(oid) =>
            {
                let name = def.column_key_name(column).ok_or_else(|| {
                    sql_err!(
                        sqlstate::INTERNAL_ERROR,
                        "inline unique key has no admitted name"
                    )
                })?;
                BuildKey::plain(
                    name,
                    &[column as u16],
                    &[def.columns()[column].collation],
                    true,
                )
            }
            RowKey::Table(key) if identity_oid(key.index_identity) == Some(oid) => {
                let mut collations = [Collation::None; MAX_INDEX_COLS];
                for (position, column) in key.columns().iter().enumerate() {
                    collations[position] = def.columns()[usize::from(*column)].collation;
                }
                BuildKey::plain(
                    key.name,
                    key.columns(),
                    &collations[..key.columns().len()],
                    !key.kind.nulls_not_distinct(),
                )
            }
            RowKey::Index(index) if crate::sql::catalog::explicit_index_oid(index) == oid => {
                BuildKey::index(storage, txid, index, arena)?
            }
            _ => return Ok(()),
        };
        found = Some(report(storage, table_index, txid, def, &key, rows, arena)?);
        Ok(())
    })?;
    Ok(found)
}

/// Collects `key`'s entries in heap order and sorts them as PostgreSQL does,
/// returning the error its comparator raises.
fn report<B: crate::storage::DefinitionBacking>(
    storage: &Storage,
    table_index: usize,
    txid: u32,
    def: &crate::storage::TableDefinition<B>,
    key: &BuildKey<'_>,
    rows: &mut BuildRows<'_>,
    arena: &Arena,
) -> Result<SqlError, SqlError> {
    let full = crate::sql::eval::arena_full;
    let indexed = |values: &[Datum], arena: &Arena| -> Result<bool, SqlError> {
        match key.predicate {
            Some(predicate) => {
                super::index_predicate_matches(storage, txid, def, values, predicate, arena)
            }
            None => Ok(true),
        }
    };
    // The heap PostgreSQL scans holds every live row; the sort only those the
    // predicate admits.
    let mut count = 0usize;
    let mut heap_bytes = 0u64;
    rows(&mut |_, values, stored| {
        heap_bytes = heap_bytes.saturating_add(maxalign(24 + stored as u64) + 4);
        let mark = arena.mark();
        let admitted = indexed(values, arena);
        // SAFETY: only the predicate's verdict leaves this row.
        unsafe { arena.rewind_to(mark) };
        count += usize::from(admitted?);
        Ok(())
    })?;
    let entries = arena
        .alloc_slice_with(count, |_| Entry {
            keys: &[],
            tuple_bytes: 0,
        })
        .map_err(|_| full())?;
    let scratch = arena
        .alloc_slice_with(ENTRY_SCRATCH_BYTES, |_| 0u8)
        .map_err(|_| full())?;
    let n_cols = key.n_cols;
    let mut filled = 0usize;
    rows(&mut |_, values, _| {
        // Evaluation scratch is discarded; the entry keeps an encoded copy.
        let encoded = arena
            .with_remaining_region(|region| -> Result<Option<(usize, u64)>, SqlError> {
                if !indexed(values, region)? {
                    return Ok(None);
                }
                let keys = super::index_key_values(
                    storage,
                    txid,
                    def,
                    values,
                    &key.columns[..n_cols],
                    &key.expressions[..n_cols],
                    region,
                )?;
                let mut tuple = [Datum::Null; 2 * MAX_INDEX_COLS];
                tuple[..n_cols].copy_from_slice(&keys[..n_cols]);
                for (position, column) in
                    key.include_columns[..key.n_include_cols].iter().enumerate()
                {
                    tuple[n_cols + position] = values[usize::from(*column)];
                }
                let width = n_cols + key.n_include_cols;
                // An upper bound of the index tuple PostgreSQL forms: its
                // header with a NULL bitmap, and each value with a varlena
                // header and alignment padding.
                let tuple_bytes = maxalign(
                    16 + crate::sql::exec::projected_row_len(&tuple[..width])? as u64
                        + 8 * width as u64,
                );
                let written = crate::sql::exec::encode_projected_into(&keys[..n_cols], scratch)?;
                Ok(Some((written, tuple_bytes)))
            })
            .map_err(|_| full())??;
        let Some((written, tuple_bytes)) = encoded else {
            return Ok(());
        };
        let entry = entries.get_mut(filled).ok_or_else(|| {
            sql_err!(
                sqlstate::SERIALIZATION_FAILURE,
                "unique index build rows changed while sorting"
            )
        })?;
        let bytes = arena
            .alloc_slice_copy(&scratch[..written])
            .map_err(|_| full())?;
        let keys = arena
            .alloc_slice_with(n_cols, |_| Datum::Null)
            .map_err(|_| full())?;
        crate::sql::exec::decode_projected_row_into(bytes, arena, keys)?;
        *entry = Entry { keys, tuple_bytes };
        filled += 1;
        Ok(())
    })?;
    if filled != count {
        return Err(sql_err!(
            sqlstate::SERIALIZATION_FAILURE,
            "unique index build rows changed while sorting"
        ));
    }
    // pos3ql's row encoding stands in for PostgreSQL's heap tuples; doubling
    // the estimate keeps every build PostgreSQL may run in parallel refused.
    let parallel = def.persistence != crate::storage::RelationPersistence::Temporary
        && heap_bytes.div_ceil(HEAP_PAGE_BYTES).saturating_mul(2) >= PARALLEL_BUILD_PAGES;
    if parallel || !sorts_in_memory(entries.iter().map(|entry| entry.tuple_bytes as i64)) {
        return Ok(sql_err!(
            sqlstate::FEATURE_NOT_SUPPORTED,
            "unique index \"{}\" has duplicate keys PostgreSQL would find sorting in parallel or beyond maintenance_work_mem, which is not supported",
            key.name.as_str()
        ));
    }
    let order = arena
        .alloc_slice_with(count, |position| position as u32)
        .map_err(|_| full())?;
    let mut compare = |left_position: u32, right_position: u32| -> Result<Ordering, SqlError> {
        let left = &entries[left_position as usize];
        let right = &entries[right_position as usize];
        let mut has_null = false;
        for column in 0..n_cols {
            let (a, b) = (left.keys[column], right.keys[column]);
            let KeyColumn {
                order,
                descending,
                nulls_first,
            } = key.order[column];
            let ordering = match (a.is_null(), b.is_null()) {
                (true, true) => {
                    has_null = true;
                    Ordering::Equal
                }
                (true, false) if nulls_first => Ordering::Less,
                (true, false) => Ordering::Greater,
                (false, true) if nulls_first => Ordering::Greater,
                (false, true) => Ordering::Less,
                (false, false) => {
                    let ordering = match order {
                        KeyOrder::Collated(collation) => {
                            compare_datums_collated(storage, txid, collation, &a, &b)?
                        }
                        KeyOrder::Bytes => {
                            compare_datums_collated(storage, txid, Collation::C, &a, &b)?
                        }
                        KeyOrder::Function(function) => {
                            crate::sql::query::execute_index_comparison(
                                storage, txid, function, a, b, arena,
                            )?
                        }
                    };
                    if descending {
                        ordering.reverse()
                    } else {
                        ordering
                    }
                }
            };
            if ordering != Ordering::Equal {
                return Ok(ordering);
            }
        }
        if has_null && !key.nulls_not_distinct {
            // Equal keys sort by heap position.
            return Ok(left_position.cmp(&right_position));
        }
        Err(super::index_key_violation(
            storage,
            table_index,
            txid,
            def,
            key.name,
            &key.columns[..n_cols],
            &key.expressions[..n_cols],
            &key.expression_texts[..n_cols],
            left.keys,
            DuplicateKey::IndexBuild,
            arena,
        ))
    };
    match pg_qsort(order, &mut compare) {
        Err(error) => Ok(error),
        Ok(()) => Err(sql_err!(
            sqlstate::INTERNAL_ERROR,
            "unique index \"{}\" sort compared no duplicate keys",
            key.name.as_str()
        )),
    }
}

/// Whether PostgreSQL's tuplesort keeps tuples of these sizes in memory under
/// `maintenance_work_mem`: `tuplesort_puttuple_common` charges each tuple,
/// then `grow_memtuples` grows the slot array, and the sort goes to disk once
/// memory or slots run out.
fn sorts_in_memory(tuples: impl Iterator<Item = i64>) -> bool {
    let allowed = MAINTENANCE_WORK_MEM;
    let space = |slots: i64| slots * SORT_TUPLE_BYTES + CHUNK_HEADER_BYTES;
    let mut slots = INITIAL_MEMTUPSIZE;
    let mut available = allowed - space(slots);
    let mut grows = true;
    let mut count = 0i64;
    for tuple in tuples {
        available -= tuple;
        if count >= slots - 1 && grows {
            let used = allowed - available;
            let wanted = if used <= available {
                slots * 2
            } else {
                grows = false;
                (slots as f64 * (allowed as f64 / used as f64)) as i64
            };
            if wanted <= slots || available < (wanted - slots) * SORT_TUPLE_BYTES {
                grows = false;
            } else {
                available += space(slots);
                slots = wanted;
                available -= space(slots);
            }
        }
        count += 1;
        if count >= slots || available < 0 {
            return false;
        }
    }
    true
}

/// PostgreSQL's `pg_qsort` (`sort_template.h`): insertion sort below seven
/// elements, a presorted check, Bentley-McIlroy three-way partitioning around
/// a median of three (of nine above forty elements), recursing into the
/// smaller side. The comparator sees the same pairs, in the same argument
/// order, as PostgreSQL's.
fn pg_qsort(
    data: &mut [u32],
    compare: &mut dyn FnMut(u32, u32) -> Result<Ordering, SqlError>,
) -> Result<(), SqlError> {
    let mut a = 0usize;
    let mut n = data.len();
    loop {
        if n < 7 {
            for pm in a + 1..a + n {
                let mut pl = pm;
                while pl > a && compare(data[pl - 1], data[pl])?.is_gt() {
                    data.swap(pl, pl - 1);
                    pl -= 1;
                }
            }
            return Ok(());
        }
        let mut presorted = true;
        for pm in a + 1..a + n {
            if compare(data[pm - 1], data[pm])?.is_gt() {
                presorted = false;
                break;
            }
        }
        if presorted {
            return Ok(());
        }
        let mut pm = a + n / 2;
        if n > 7 {
            let mut pl = a;
            let mut pn = a + n - 1;
            if n > 40 {
                let d = n / 8;
                pl = med3(data, pl, pl + d, pl + 2 * d, compare)?;
                pm = med3(data, pm - d, pm, pm + d, compare)?;
                pn = med3(data, pn - 2 * d, pn - d, pn, compare)?;
            }
            pm = med3(data, pl, pm, pn, compare)?;
        }
        data.swap(a, pm);
        let (mut pa, mut pb) = (a + 1, a + 1);
        let (mut pc, mut pd) = (a + n - 1, a + n - 1);
        loop {
            while pb <= pc {
                let r = compare(data[pb], data[a])?;
                if r.is_gt() {
                    break;
                }
                if r.is_eq() {
                    data.swap(pa, pb);
                    pa += 1;
                }
                pb += 1;
            }
            while pb <= pc {
                let r = compare(data[pc], data[a])?;
                if r.is_lt() {
                    break;
                }
                if r.is_eq() {
                    data.swap(pc, pd);
                    pd -= 1;
                }
                pc -= 1;
            }
            if pb > pc {
                break;
            }
            data.swap(pb, pc);
            pb += 1;
            pc -= 1;
        }
        let pn = a + n;
        let d1 = (pa - a).min(pb - pa);
        swap_runs(data, a, pb - d1, d1);
        let d1 = (pd - pc).min(pn - pd - 1);
        swap_runs(data, pb, pn - d1, d1);
        let d1 = pb - pa;
        let d2 = pd - pc;
        if d1 <= d2 {
            if d1 > 1 {
                pg_qsort(&mut data[a..a + d1], compare)?;
            }
            if d2 <= 1 {
                return Ok(());
            }
            a = pn - d2;
            n = d2;
        } else {
            if d2 > 1 {
                pg_qsort(&mut data[pn - d2..pn], compare)?;
            }
            if d1 <= 1 {
                return Ok(());
            }
            n = d1;
        }
    }
}

fn med3(
    data: &[u32],
    a: usize,
    b: usize,
    c: usize,
    compare: &mut dyn FnMut(u32, u32) -> Result<Ordering, SqlError>,
) -> Result<usize, SqlError> {
    Ok(if compare(data[a], data[b])?.is_lt() {
        if compare(data[b], data[c])?.is_lt() {
            b
        } else if compare(data[a], data[c])?.is_lt() {
            c
        } else {
            a
        }
    } else if compare(data[b], data[c])?.is_gt() {
        b
    } else if compare(data[a], data[c])?.is_lt() {
        a
    } else {
        c
    })
}

fn swap_runs(data: &mut [u32], a: usize, b: usize, n: usize) {
    for offset in 0..n {
        data.swap(a + offset, b + offset);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The comparisons `pg_qsort` makes, recorded as value pairs.
    fn trace(values: &[i64]) -> (Vec<u32>, Vec<(i64, i64)>) {
        let mut order: Vec<u32> = (0..values.len() as u32).collect();
        let mut pairs = Vec::new();
        pg_qsort(&mut order, &mut |a, b| {
            pairs.push((values[a as usize], values[b as usize]));
            Ok(values[a as usize].cmp(&values[b as usize]))
        })
        .unwrap();
        (order, pairs)
    }

    #[test]
    fn sorts_like_pg_qsort_and_meets_duplicates_in_its_order() {
        for values in [
            vec![5, 3, 9, 1, 7, 2, 8, 6, 4, 0],
            (0..100).map(|v| (v * 37) % 101).collect(),
            (0..60).rev().collect(),
            vec![1, 2, 3, 4, 5, 6, 7, 8],
        ] {
            let (order, _) = trace(&values);
            let sorted: Vec<i64> = order.iter().map(|i| values[*i as usize]).collect();
            let mut expected = values.clone();
            expected.sort_unstable();
            assert_eq!(sorted, expected);
        }
        // Eight elements, not presorted: the median of the first, middle and
        // last is compared before any partitioning.
        let (_, pairs) = trace(&[3, 1, 2, 4, 5, 6, 7, 0]);
        assert_eq!(pairs[..4], [(3, 1), (3, 5), (5, 0), (3, 0)]);
    }

    #[test]
    fn spills_once_maintenance_work_mem_is_spent() {
        assert!(sorts_in_memory([16i64; 1000].into_iter()));
        assert!(sorts_in_memory(core::iter::repeat_n(16, 1_000_000)));
        assert!(!sorts_in_memory(core::iter::repeat_n(16, 2_000_000)));
        assert!(!sorts_in_memory(core::iter::repeat_n(1 << 20, 64)));
    }
}
