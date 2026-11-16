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
}
