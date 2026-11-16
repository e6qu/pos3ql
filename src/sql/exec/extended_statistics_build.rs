//! PostgreSQL's extended-statistics builders over ANALYZE's sample:
//! `statext_ndistinct_build` (`mvdistinct.c`), `statext_dependencies_build`
//! (`dependencies.c`) and `statext_mcv_build` (`mcv.c`) of PostgreSQL 18.6.
//! Each sorts the sample's rows with PostgreSQL's quicksort and the key
//! types' default orderings (NULLs last), so equal counts keep PostgreSQL's
//! order.

use crate::mem::arena::Arena;
use crate::sql::ast::Collation;
use crate::sql::eval::SqlError;
use crate::sql::types::Datum;
use crate::storage::{
    MAX_EXTENDED_STATISTICS_DEPENDENCIES, MAX_EXTENDED_STATISTICS_KEYS,
    MAX_EXTENDED_STATISTICS_NDISTINCT, Storage,
};
use core::cmp::Ordering;

/// ANALYZE's sampled rows of a statistics object's keys.
pub(super) struct Sample<'s, 'a> {
    pub(super) rows: usize,
    pub(super) keys: usize,
    /// `rows * keys` values, row by row.
    pub(super) values: &'s [Datum<'a>],
    /// Per value: a variable-width value wider than `WIDTH_THRESHOLD`, which
    /// `build_sorted_items` leaves out with its row.
    pub(super) too_wide: &'s [bool],
    pub(super) collations: [Collation; MAX_EXTENDED_STATISTICS_KEYS],
}

impl Sample<'_, '_> {
    fn value(&self, row: u32, key: u8) -> &Datum<'_> {
        &self.values[row as usize * self.keys + usize::from(key)]
    }

    /// `multi_sort_compare` over `dimensions`, NULLs last.
    fn compare(
        &self,
        storage: &Storage,
        txid: u32,
        dimensions: &[u8],
        left: u32,
        right: u32,
    ) -> Result<Ordering, SqlError> {
        for &key in dimensions {
            let order = self.compare_key(storage, txid, key, left, right)?;
            if order != Ordering::Equal {
                return Ok(order);
            }
        }
        Ok(Ordering::Equal)
    }

    fn compare_key(
        &self,
        storage: &Storage,
        txid: u32,
        key: u8,
        left: u32,
        right: u32,
    ) -> Result<Ordering, SqlError> {
        let (left, right) = (self.value(left, key), self.value(right, key));
        Ok(match (left.is_null(), right.is_null()) {
            (true, true) => Ordering::Equal,
            (true, false) => Ordering::Greater,
            (false, true) => Ordering::Less,
            (false, false) => crate::sql::eval::compare_datums_collated(
                storage,
                txid,
                self.collations[usize::from(key)],
                left,
                right,
            )?,
        })
    }

    /// `build_sorted_items`: the rows without a too-wide value among
    /// `dimensions`, sorted by them; empty when every row has one.
    fn sorted_items<'r>(
        &self,
        storage: &Storage,
        txid: u32,
        dimensions: &[u8],
        arena: &'r Arena,
    ) -> Result<&'r mut [u32], SqlError> {
        let items = arena
            .alloc_slice_with(self.rows, |_| 0u32)
            .map_err(|_| crate::sql::eval::arena_full())?;
        let mut count = 0usize;
        for row in 0..self.rows {
            if dimensions
                .iter()
                .any(|&key| self.too_wide[row * self.keys + usize::from(key)])
            {
                continue;
            }
            items[count] = row as u32;
            count += 1;
        }
        let items = &mut items[..count];
        crate::sql::pg_qsort::pg_qsort(items, &mut |left, right| {
            self.compare(storage, txid, dimensions, left, right)
        })?;
        Ok(items)
    }
}

/// `estimate_ndistinct`: the Haas-Stokes estimate of the distinct values of
/// a relation of `total_rows` rows from `rows` sampled ones holding `d`
/// distinct values, `f1` of them once.
fn estimate_ndistinct(total_rows: f64, rows: usize, d: usize, f1: usize) -> f64 {
    let numer = rows as f64 * d as f64;
    let denom = (rows - f1) as f64 + f1 as f64 * rows as f64 / total_rows;
    let mut ndistinct = numer / denom;
    if ndistinct < d as f64 {
        ndistinct = d as f64;
    }
    if ndistinct > total_rows {
        ndistinct = total_rows;
    }
    (ndistinct + 0.5).floor()
}

/// `statext_ndistinct_build`: each combination's estimate, in
/// `extended_statistics_combinations` order.
pub(super) fn ndistinct(
    storage: &Storage,
    txid: u32,
    sample: &Sample<'_, '_>,
    total_rows: f64,
    arena: &Arena,
) -> Result<[u64; MAX_EXTENDED_STATISTICS_NDISTINCT], SqlError> {
    let mut result = [0u64; MAX_EXTENDED_STATISTICS_NDISTINCT];
    let mut position = 0usize;
    let mut failure = None;
    crate::storage::extended_statistics_combinations(sample.keys, &mut |combination| {
        if failure.is_some() {
            return;
        }
        let mark = arena.mark();
        let estimate = (|| {
            let items = arena
                .alloc_slice_with(sample.rows, |row| row as u32)
                .map_err(|_| crate::sql::eval::arena_full())?;
            crate::sql::pg_qsort::pg_qsort(items, &mut |left, right| {
                sample.compare(storage, txid, combination, left, right)
            })?;
            let (mut f1, mut count, mut d) = (0usize, 1usize, 1usize);
            for at in 1..items.len() {
                if sample
                    .compare(storage, txid, combination, items[at], items[at - 1])?
                    .is_ne()
                {
                    if count == 1 {
                        f1 += 1;
                    }
                    d += 1;
                    count = 0;
                }
                count += 1;
            }
            if count == 1 {
                f1 += 1;
            }
            Ok::<_, SqlError>(estimate_ndistinct(total_rows, sample.rows, d, f1))
        })();
        // SAFETY: the sorted items were consumed above.
        unsafe { arena.rewind_to(mark) };
        match estimate {
            Ok(estimate) => result[position] = estimate as u64,
            Err(error) => failure = Some(error),
        }
        position += 1;
    });
    match failure {
        Some(error) => Err(error),
        None => Ok(result),
    }
}

/// `statext_dependencies_build`: each dependency's degree (its bits), in
/// `extended_statistics_dependency_order` order.
pub(super) fn dependencies(
    storage: &Storage,
    txid: u32,
    sample: &Sample<'_, '_>,
    arena: &Arena,
) -> Result<[u64; MAX_EXTENDED_STATISTICS_DEPENDENCIES], SqlError> {
    let mut result = [0u64; MAX_EXTENDED_STATISTICS_DEPENDENCIES];
    let mut position = 0usize;
    let mut failure = None;
    crate::storage::extended_statistics_dependency_order(sample.keys, &mut |dependency| {
        if failure.is_some() {
            return;
        }
        let mark = arena.mark();
        let degree = (|| {
            let items = sample.sorted_items(storage, txid, dependency, arena)?;
            let k = dependency.len();
            let determinants = &dependency[..k - 1];
            let implied = dependency[k - 1];
            let (mut group_size, mut violations, mut supporting) = (1usize, 0usize, 0usize);
            for at in 1..=items.len() {
                if at == items.len()
                    || sample
                        .compare(storage, txid, determinants, items[at - 1], items[at])?
                        .is_ne()
                {
                    if violations == 0 {
                        supporting += group_size;
                    }
                    violations = 0;
                    group_size = 1;
                    continue;
                }
                if sample
                    .compare_key(storage, txid, implied, items[at - 1], items[at])?
                    .is_ne()
                {
                    violations += 1;
                }
                group_size += 1;
            }
            // An empty list of sorted items supports nothing.
            if items.is_empty() {
                supporting = 0;
            }
            Ok::<_, SqlError>(supporting as f64 / sample.rows as f64)
        })();
        // SAFETY: the sorted items were consumed above.
        unsafe { arena.rewind_to(mark) };
        match degree {
            Ok(degree) => result[position] = degree.to_bits(),
            Err(error) => failure = Some(error),
        }
        position += 1;
    });
    match failure {
        Some(error) => Err(error),
        None => Ok(result),
    }
}

/// One item `statext_mcv_build` keeps: a representative sampled row of the
/// group, how many rows hold it, and its base frequency.
#[derive(Clone, Copy)]
pub(super) struct McvItem {
    pub(super) row: u32,
    pub(super) count: u64,
    pub(super) base_frequency: f64,
}

/// `get_mincount_for_mcv_list`.
fn mincount(sample_rows: usize, total_rows: f64) -> f64 {
    let n = sample_rows as f64;
    let numer = n * (total_rows - n);
    let denom = total_rows - n + 0.04 * n * (total_rows - 1.0);
    if denom == 0.0 { 0.0 } else { numer / denom }
}

/// `statext_mcv_build`: the most common combinations of the keys, at most
/// `target`, most frequent first; `items` receives them and the count is
/// returned.
pub(super) fn mcv(
    storage: &Storage,
    txid: u32,
    sample: &Sample<'_, '_>,
    total_rows: f64,
    target: usize,
    items_out: &mut [McvItem],
    arena: &Arena,
) -> Result<usize, SqlError> {
    let all = [0u8, 1, 2, 3, 4, 5, 6, 7];
    let dimensions = &all[..sample.keys];
    let mark = arena.mark();
    let result = (|| {
        let items = sample.sorted_items(storage, txid, dimensions, arena)?;
        if items.is_empty() {
            return Ok(0);
        }
        // `build_distinct_groups`: each group's first row and count.
        let rows = arena
            .alloc_slice_with(items.len(), |_| 0u32)
            .map_err(|_| crate::sql::eval::arena_full())?;
        let counts = arena
            .alloc_slice_with(items.len(), |_| 0u64)
            .map_err(|_| crate::sql::eval::arena_full())?;
        let mut groups = 0usize;
        rows[0] = items[0];
        counts[0] = 1;
        for at in 1..items.len() {
            if sample
                .compare(storage, txid, dimensions, items[at], items[at - 1])?
                .is_ne()
            {
                groups += 1;
                rows[groups] = items[at];
                counts[groups] = 0;
            }
            counts[groups] += 1;
        }
        let groups = groups + 1;
        // Sorted by count, most frequent first (`compare_sort_item_count`),
        // moving each group's row and count together.
        let order = arena
            .alloc_slice_with(groups, |group| group as u32)
            .map_err(|_| crate::sql::eval::arena_full())?;
        crate::sql::pg_qsort::pg_qsort(order, &mut |left, right| {
            Ok(counts[right as usize].cmp(&counts[left as usize]))
        })?;
        let mut kept = target.min(groups);
        let threshold = mincount(sample.rows, total_rows);
        for (position, &group) in order[..kept].iter().enumerate() {
            if (counts[group as usize] as f64) < threshold {
                kept = position;
                break;
            }
        }
        if kept > items_out.len() {
            return Err(crate::sql_err!(
                crate::sql::eval::sqlstate::PROGRAM_LIMIT_EXCEEDED,
                "an extended-statistics MCV list of {} items exceeds its {} items",
                kept,
                items_out.len()
            ));
        }
        if kept == 0 {
            return Ok(0);
        }
        // `build_column_frequencies`: each key value's count over every
        // group, found for each kept item.
        let by_value = arena
            .alloc_slice_with(groups, |group| group as u32)
            .map_err(|_| crate::sql::eval::arena_full())?;
        for (position, &group) in order[..kept].iter().enumerate() {
            items_out[position] = McvItem {
                row: rows[group as usize],
                count: counts[group as usize],
                base_frequency: 1.0,
            };
        }
        for key in 0..sample.keys as u8 {
            for (group, slot) in by_value.iter_mut().enumerate() {
                *slot = group as u32;
            }
            crate::sql::pg_qsort::pg_qsort(by_value, &mut |left, right| {
                sample.compare_key(
                    storage,
                    txid,
                    key,
                    rows[left as usize],
                    rows[right as usize],
                )
            })?;
            for item in items_out[..kept].iter_mut() {
                // The run of groups holding the item's value of `key`.
                let (mut low, mut high) = (0usize, groups);
                while low < high {
                    let middle = (low + high) / 2;
                    if sample
                        .compare_key(
                            storage,
                            txid,
                            key,
                            rows[by_value[middle] as usize],
                            item.row,
                        )?
                        .is_lt()
                    {
                        low = middle + 1;
                    } else {
                        high = middle;
                    }
                }
                let mut count = 0u64;
                let mut at = low;
                while at < groups
                    && sample
                        .compare_key(storage, txid, key, rows[by_value[at] as usize], item.row)?
                        .is_eq()
                {
                    count += counts[by_value[at] as usize];
                    at += 1;
                }
                item.base_frequency *= count as f64 / sample.rows as f64;
            }
        }
        Ok(kept)
    })();
    // SAFETY: the kept items were copied to `items_out`.
    unsafe { arena.rewind_to(mark) };
    result
}
