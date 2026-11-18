//! PostgreSQL's in-memory sort (`src/include/lib/sort_template.h`), whose
//! order of equal elements and sequence of comparisons callers reproduce.

use crate::sql::eval::SqlError;
use core::cmp::Ordering;

/// PostgreSQL's `pg_qsort` (`sort_template.h`): insertion sort below seven
/// elements, a presorted check, Bentley-McIlroy three-way partitioning around
/// a median of three (of nine above forty elements), recursing into the
/// smaller side. The comparator sees the same pairs, in the same argument
/// order, as PostgreSQL's.
pub(crate) fn pg_qsort(
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

/// PostgreSQL's in-memory tuplesort of `data` in arrival order: a sort
/// bounded to its `bound` least elements keeps them in a heap once more
/// than twice the bound arrive (`make_bounded_heap`, `tuplesort_heap_*`
/// with the sort reversed) and returns them sorted (`sort_bounded_heap`);
/// otherwise `pg_qsort`. Returns how many leading elements are the sorted
/// What [`pg_incremental_sort`] tells of each batch it sorts: whether it is
/// a presorted-prefix batch, its rows, and the bound it sorts under.
pub(crate) type SortedBatch<'f> =
    dyn FnMut(bool, &[u32], Option<usize>) -> Result<(), SqlError> + 'f;

/// result.
pub(crate) fn pg_tuplesort(
    data: &mut [u32],
    bound: Option<usize>,
    compare: &mut dyn FnMut(u32, u32) -> Result<Ordering, SqlError>,
) -> Result<usize, SqlError> {
    let Some(bound) = bound.filter(|&bound| data.len() > bound * 2) else {
        pg_qsort(data, compare)?;
        return Ok(data.len());
    };
    if bound == 0 {
        return Ok(0);
    }
    // The reversed order puts the greatest element at the heap's top.
    let mut reversed = |a: u32, b: u32| compare(b, a);
    let mut count = 0usize;
    for at in 0..data.len() {
        let tuple = data[at];
        if count < bound {
            // `tuplesort_heap_insert`: sift up.
            let mut j = count;
            count += 1;
            while j > 0 {
                let i = (j - 1) >> 1;
                if reversed(tuple, data[i])?.is_ge() {
                    break;
                }
                data[j] = data[i];
                j = i;
            }
            data[j] = tuple;
        } else if reversed(tuple, data[0])?.is_le() {
            // Not less than the greatest kept: discarded.
        } else {
            heap_replace_top(&mut data[..count], tuple, &mut reversed)?;
        }
    }
    // `sort_bounded_heap`: each greatest element moves past the heap.
    let kept = count;
    while count > 1 {
        let top = data[0];
        count -= 1;
        let last = data[count];
        heap_replace_top(&mut data[..count], last, &mut reversed)?;
        data[count] = top;
    }
    Ok(kept)
}

/// Where PostgreSQL's incremental sort is in its cycle
/// (`IncrementalSortExecutionStatus`).
#[derive(Clone, Copy, PartialEq, Eq)]
enum Incremental {
    LoadFull,
    LoadPrefix,
    ReadFull,
    ReadPrefix,
}

/// PostgreSQL's `ExecIncrementalSort` over `input`, already sorted by the
/// first keys, which `same_group` compares: batches of at least 32 rows
/// (`DEFAULT_MIN_GROUP_SIZE`) extended to the end of their group are
/// sorted by every key, and past 64 rows a large group's rest is sorted
/// alone by the other keys (`switchToPresortedPrefixMode`). A `bound` from
/// a limit above stops it once it returned that many rows and bounds its
/// sorts as `tuplesort_set_bound` does. Writes the rows it returns to
/// `out`, using `scratch` of twice the input's length, and returns how
/// many and how many input rows it read. `sorted` sees each batch before
/// it is sorted: whether it is a
/// presorted prefix group's, its rows in arrival order and its sort's
/// bound (`instrumentSortedGroup`).
pub(crate) fn pg_incremental_sort(
    input: &[u32],
    scratch: &mut [u32],
    out: &mut [u32],
    bound: Option<usize>,
    same_group: &mut dyn FnMut(u32, u32) -> Result<bool, SqlError>,
    compare: &mut dyn FnMut(u32, u32) -> Result<Ordering, SqlError>,
    sorted: &mut SortedBatch<'_>,
) -> Result<(usize, usize), SqlError> {
    const MIN_GROUP: usize = 32;
    const MAX_FULL_SORT_GROUP: usize = 2 * MIN_GROUP;
    let (full, prefix) = scratch.split_at_mut(input.len());
    let mut state = IncrementalState {
        status: Incremental::LoadFull,
        pivot: None,
        transfer: None,
        remaining: 0,
        bound_done: 0,
        full_read: 0,
        prefix_len: 0,
        prefix_bound: None,
    };
    let (mut full_kept, mut prefix_kept) = (0usize, 0usize);
    let (mut position, mut emitted, mut outer_done) = (0usize, 0usize, false);
    loop {
        if bound.is_some_and(|bound| emitted >= bound) {
            break;
        }
        let mut tuples = 0usize;
        if matches!(
            state.status,
            Incremental::ReadFull | Incremental::ReadPrefix
        ) {
            if outer_done {
                break;
            }
            if state.remaining > 0 {
                prefix_kept =
                    state.switch_to_prefix(full, prefix, bound, same_group, compare, sorted)?;
            } else {
                state.status = Incremental::LoadFull;
            }
        }
        if state.status == Incremental::LoadFull {
            let mut full_len = 0;
            state.full_read = 0;
            let current = bound.map(|bound| bound - state.bound_done);
            let sort_bound = current.filter(|&current| current < MIN_GROUP);
            let min_group = current.map_or(MIN_GROUP, |current| current.min(MIN_GROUP));
            if let Some(pivot) = state.pivot {
                full[full_len] = pivot;
                full_len += 1;
                tuples += 1;
                if tuples != min_group {
                    state.pivot = None;
                }
            }
            loop {
                let Some(&slot) = input.get(position) else {
                    outer_done = true;
                    sorted(false, &full[..full_len], sort_bound)?;
                    full_kept = pg_tuplesort(&mut full[..full_len], sort_bound, compare)?;
                    state.status = Incremental::ReadFull;
                    break;
                };
                position += 1;
                if tuples < min_group {
                    full[full_len] = slot;
                    full_len += 1;
                    tuples += 1;
                    if tuples == min_group {
                        state.pivot = Some(slot);
                    }
                } else if same_group(state.pivot.expect("a group's first row"), slot)? {
                    full[full_len] = slot;
                    full_len += 1;
                    tuples += 1;
                } else {
                    state.pivot = Some(slot);
                    if let Some(bound) = bound {
                        state.bound_done = bound.min(state.bound_done + tuples);
                    }
                    sorted(false, &full[..full_len], sort_bound)?;
                    full_kept = pg_tuplesort(&mut full[..full_len], sort_bound, compare)?;
                    state.status = Incremental::ReadFull;
                    break;
                }
                if tuples > MAX_FULL_SORT_GROUP {
                    state.pivot = None;
                    sorted(false, &full[..full_len], sort_bound)?;
                    full_kept = pg_tuplesort(&mut full[..full_len], sort_bound, compare)?;
                    if let (Some(sort_bound), Some(current)) = (sort_bound, current)
                        && full_len > sort_bound * 2
                    {
                        tuples = tuples.min(current);
                    }
                    state.remaining = tuples;
                    prefix_kept =
                        state.switch_to_prefix(full, prefix, bound, same_group, compare, sorted)?;
                    break;
                }
            }
        }
        if state.status == Incremental::LoadPrefix {
            loop {
                let Some(&slot) = input.get(position) else {
                    outer_done = true;
                    break;
                };
                position += 1;
                if same_group(state.pivot.expect("a group's first row"), slot)? {
                    prefix[state.prefix_len] = slot;
                    state.prefix_len += 1;
                    tuples += 1;
                } else {
                    state.pivot = Some(slot);
                    break;
                }
            }
            sorted(true, &prefix[..state.prefix_len], state.prefix_bound)?;
            prefix_kept =
                pg_tuplesort(&mut prefix[..state.prefix_len], state.prefix_bound, compare)?;
            state.status = Incremental::ReadPrefix;
            if let Some(bound) = bound {
                state.bound_done = bound.min(state.bound_done + tuples);
            }
        }
        let batch = if state.status == Incremental::ReadFull {
            &full[..full_kept]
        } else {
            &prefix[..prefix_kept]
        };
        for &row in batch {
            if bound.is_some_and(|bound| emitted >= bound) {
                break;
            }
            out[emitted] = row;
            emitted += 1;
        }
    }
    Ok((emitted, position))
}

/// The incremental sort's state between its batches.
struct IncrementalState {
    status: Incremental,
    /// The first row of the group being loaded (`group_pivot`).
    pivot: Option<u32>,
    /// The full sort's row that began the next group (`transfer_tuple`).
    transfer: Option<u32>,
    /// The full sort's rows not yet returned (`n_fullsort_remaining`).
    remaining: usize,
    bound_done: usize,
    /// The full sort's rows read back.
    full_read: usize,
    prefix_len: usize,
    prefix_bound: Option<usize>,
}

impl IncrementalState {
    /// `switchToPresortedPrefixMode`: moves the sorted full batch's rows of
    /// its first group to the prefix sort; returns the prefix sort's
    /// sorted rows once the group ends within them.
    fn switch_to_prefix(
        &mut self,
        full: &[u32],
        prefix: &mut [u32],
        bound: Option<usize>,
        same_group: &mut dyn FnMut(u32, u32) -> Result<bool, SqlError>,
        compare: &mut dyn FnMut(u32, u32) -> Result<Ordering, SqlError>,
        sorted: &mut SortedBatch<'_>,
    ) -> Result<usize, SqlError> {
        self.prefix_len = 0;
        self.prefix_bound = bound.map(|bound| bound - self.bound_done);
        let mut tuples = 0;
        while tuples < self.remaining {
            if tuples == 0
                && let Some(transfer) = self.transfer
            {
                prefix[self.prefix_len] = transfer;
                self.prefix_len += 1;
                self.pivot = Some(transfer);
            } else {
                let row = full[self.full_read];
                self.full_read += 1;
                self.transfer = Some(row);
                let pivot = *self.pivot.get_or_insert(row);
                if same_group(pivot, row)? {
                    prefix[self.prefix_len] = row;
                    self.prefix_len += 1;
                } else {
                    self.pivot = None;
                    break;
                }
            }
            tuples += 1;
        }
        self.remaining -= tuples;
        if self.remaining == 0 {
            self.pivot = self.transfer.take();
            self.status = Incremental::LoadPrefix;
            Ok(0)
        } else {
            sorted(true, &prefix[..self.prefix_len], self.prefix_bound)?;
            let kept = pg_tuplesort(&mut prefix[..self.prefix_len], self.prefix_bound, compare)?;
            if let Some(bound) = bound {
                self.bound_done = bound.min(self.bound_done + tuples);
            }
            self.status = Incremental::ReadPrefix;
            Ok(kept)
        }
    }
}

/// `tuplesort_heap_replace_top`: `tuple` replaces the heap's top and sifts
/// down.
fn heap_replace_top(
    heap: &mut [u32],
    tuple: u32,
    reversed: &mut dyn FnMut(u32, u32) -> Result<Ordering, SqlError>,
) -> Result<(), SqlError> {
    let n = heap.len();
    let mut i = 0usize;
    loop {
        let mut j = 2 * i + 1;
        if j >= n {
            break;
        }
        if j + 1 < n && reversed(heap[j], heap[j + 1])?.is_gt() {
            j += 1;
        }
        if reversed(tuple, heap[j])?.is_le() {
            break;
        }
        heap[i] = heap[j];
        i = j;
    }
    heap[i] = tuple;
    Ok(())
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

#[cfg(all(test, not(feature = "sql-fixture-suite")))]
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
    fn bounded_and_incremental_sorts_return_the_least_rows_in_order() {
        let values = [9i64, 4, 7, 1, 8, 2, 6, 3, 5, 0, 4, 1];
        let mut compare = |a: u32, b: u32| Ok(values[a as usize].cmp(&values[b as usize]));
        let mut order: Vec<u32> = (0..values.len() as u32).collect();
        let kept = pg_tuplesort(&mut order, Some(3), &mut compare).unwrap();
        let least: Vec<i64> = order[..kept]
            .iter()
            .map(|&at| values[at as usize])
            .collect();
        assert_eq!(least, [0, 1, 1]);
        // Rows already grouped by their tens are sorted within each batch.
        let grouped: Vec<i64> = (0..100).map(|at| (at / 40) * 100 + (99 - at) % 7).collect();
        let input: Vec<u32> = (0..grouped.len() as u32).collect();
        let mut scratch = vec![0u32; grouped.len() * 2];
        let mut out = vec![0u32; grouped.len()];
        let mut batches = Vec::new();
        let (returned, _) = pg_incremental_sort(
            &input,
            &mut scratch,
            &mut out,
            None,
            &mut |a, b| Ok(grouped[a as usize] / 100 == grouped[b as usize] / 100),
            &mut |a, b| Ok(grouped[a as usize].cmp(&grouped[b as usize])),
            &mut |prefix, rows, _| {
                batches.push((prefix, rows.len()));
                Ok(())
            },
        )
        .unwrap();
        // Groups of 40, 40 and 20 rows: two full sorts of one group each,
        // the second extended past 32 rows to its group's end.
        assert_eq!(batches, [(false, 40), (false, 40), (false, 20)]);
        let sorted: Vec<i64> = out[..returned]
            .iter()
            .map(|&at| grouped[at as usize])
            .collect();
        let mut expected = grouped.clone();
        expected.sort_unstable();
        assert_eq!(sorted, expected);
    }
}
