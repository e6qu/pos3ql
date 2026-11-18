//! PostgreSQL's `compute_array_stats` (`array_typanalyze.c`, REL_18_6): the
//! most common elements of an array column with their frequencies
//! (`STATISTIC_KIND_MCELEM`) and the histogram of the arrays' distinct
//! element counts (`STATISTIC_KIND_DECHIST`), found by lossy counting over
//! the ANALYZE sample's arrays in sample order.

use core::cmp::Ordering;

use crate::mem::arena::Arena;
use crate::sql::eval::SqlError;
use crate::sql::types::{ArrElem, Datum};

/// An array wider than this joins no element statistics
/// (`ARRAY_WIDTH_THRESHOLD`).
const ARRAY_WIDTH_THRESHOLD: usize = 0x10000;

/// The element statistics of one column.
#[derive(Clone, Copy)]
pub(crate) struct ElementSet<'a> {
    /// The tracked elements in the element type's order.
    pub(crate) values: &'a [Datum<'a>],
    /// Each element's frequency among the non-NULL arrays, then the least
    /// and greatest tracked frequencies and the frequency of arrays holding
    /// a NULL element; empty when no element is tracked.
    pub(crate) frequencies: &'a [f32],
    /// The distinct-element-count histogram followed by the average count;
    /// empty when no array was analyzed.
    pub(crate) counts: &'a [f32],
}

#[derive(Clone, Copy)]
struct Tracked<'a> {
    value: Datum<'a>,
    frequency: u32,
    delta: u32,
    last_container: u32,
    live: bool,
}

/// Computes the element statistics of the sample's `sampled` rows, `value_at`
/// returning row `index`'s array (NULL for none), for statistics target
/// `target`; `None` when no array was analyzed.
pub(crate) fn compute<'a>(
    value_at: &mut dyn FnMut(usize) -> Result<Datum<'a>, SqlError>,
    sampled: usize,
    target: usize,
    element: ArrElem,
    compare: &mut dyn FnMut(&Datum<'a>, &Datum<'a>) -> Result<Ordering, SqlError>,
    hash: &mut dyn FnMut(&Datum<'a>) -> Result<u64, SqlError>,
    arena: &'a Arena,
) -> Result<Option<ElementSet<'a>>, SqlError> {
    let arena_full = crate::sql::eval::arena_full;
    // `num_mcelem` and the lossy-counting bucket width.
    let mut num_mcelem = target * 10;
    let bucket_width = (num_mcelem * 1000 / 7).max(1);
    // The most element occurrences the sample can add.
    let mut capacity = 0usize;
    for row in 0..sampled {
        if let Datum::Array { raw, .. } = value_at(row)?
            && raw.len() <= ARRAY_WIDTH_THRESHOLD
        {
            capacity += crate::sql::array::len(raw);
        }
    }
    let tracked = arena
        .alloc_slice_with(capacity.max(1), |_| Tracked {
            value: Datum::Null,
            frequency: 0,
            delta: 0,
            last_container: u32::MAX,
            live: false,
        })
        .map_err(|_| arena_full())?;
    let table_size = (capacity.max(1) * 2).next_power_of_two();
    let table = arena
        .alloc_slice_with(table_size, |_| 0u32)
        .map_err(|_| arena_full())?;
    // The distinct-element counts of the analyzed arrays: (count, arrays).
    let counts = arena
        .alloc_slice_with(sampled.max(1), |_| (0u32, 0u32))
        .map_err(|_| arena_full())?;
    let mut count_items = 0usize;
    let mut used = 0usize;
    let mut b_current = 1u32;
    let mut element_no = 0usize;
    let mut analyzed_rows = 0usize;
    let mut null_elem_cnt = 0usize;
    for row in 0..sampled {
        crate::sql::query::check_timeout()?;
        let Datum::Array { raw, .. } = value_at(row)? else {
            continue;
        };
        if raw.len() > ARRAY_WIDTH_THRESHOLD {
            continue;
        }
        analyzed_rows += 1;
        let mut null_present = false;
        let mut distinct_count = 0u32;
        for index in 0..crate::sql::array::len(raw) {
            let value = crate::sql::array::get(raw, element, index).unwrap_or(Datum::Null);
            if value.is_null() {
                null_present = true;
                continue;
            }
            let mut slot = (hash(&value)? as usize) & (table_size - 1);
            let found = loop {
                let held = table[slot];
                if held == 0 {
                    break None;
                }
                let entry = &tracked[held as usize - 1];
                if entry.live && compare(&entry.value, &value)? == Ordering::Equal {
                    break Some(held as usize - 1);
                }
                slot = (slot + 1) & (table_size - 1);
            };
            match found {
                Some(at) => {
                    let entry = &mut tracked[at];
                    // An element counts once per array.
                    if entry.last_container == row as u32 {
                        continue;
                    }
                    entry.frequency += 1;
                    entry.last_container = row as u32;
                }
                None => {
                    tracked[used] = Tracked {
                        value,
                        frequency: 1,
                        delta: b_current - 1,
                        last_container: row as u32,
                        live: true,
                    };
                    used += 1;
                    table[slot] = used as u32;
                }
            }
            distinct_count += 1;
            element_no += 1;
            // Prune after each bucket (`prune_element_hashtable`).
            if element_no.is_multiple_of(bucket_width) {
                for entry in &mut tracked[..used] {
                    if entry.live && entry.frequency + entry.delta <= b_current {
                        entry.live = false;
                    }
                }
                table.fill(0);
                for (at, entry) in tracked[..used].iter().enumerate() {
                    if !entry.live {
                        continue;
                    }
                    let mut slot = (hash(&entry.value)? as usize) & (table_size - 1);
                    while table[slot] != 0 {
                        slot = (slot + 1) & (table_size - 1);
                    }
                    table[slot] = at as u32 + 1;
                }
                b_current += 1;
            }
        }
        if null_present {
            null_elem_cnt += 1;
        }
        match counts[..count_items]
            .iter_mut()
            .find(|(count, _)| *count == distinct_count)
        {
            Some((_, arrays)) => *arrays += 1,
            None => {
                counts[count_items] = (distinct_count, 1);
                count_items += 1;
            }
        }
    }
    if analyzed_rows == 0 {
        return Ok(None);
    }
    let nonnull = analyzed_rows as f64;
    // The elements whose frequency exceeds the cutoff `(s - epsilon) * N`.
    let cutoff = 9 * element_no / bucket_width;
    let mut keep = 0usize;
    let (mut minfreq, mut maxfreq) = (element_no as u32, 0u32);
    let order = arena
        .alloc_slice_with(used.max(1), |_| 0usize)
        .map_err(|_| arena_full())?;
    for (at, entry) in tracked[..used].iter().enumerate() {
        if entry.live && entry.frequency as usize > cutoff {
            order[keep] = at;
            keep += 1;
            minfreq = minfreq.min(entry.frequency);
            maxfreq = maxfreq.max(entry.frequency);
        }
    }
    if num_mcelem < keep {
        // The most frequent `num_mcelem`, most frequent first.
        order[..keep]
            .sort_by(|&left, &right| tracked[right].frequency.cmp(&tracked[left].frequency));
        minfreq = tracked[order[num_mcelem - 1]].frequency;
    } else {
        num_mcelem = keep;
    }
    let mut values: &[Datum<'a>] = &[];
    let mut frequencies: &[f32] = &[];
    if num_mcelem > 0 {
        let chosen = &mut order[..num_mcelem];
        let mut failure = None;
        chosen.sort_by(|&left, &right| {
            if failure.is_some() {
                return Ordering::Equal;
            }
            compare(&tracked[left].value, &tracked[right].value).unwrap_or_else(|error| {
                failure = Some(error);
                Ordering::Equal
            })
        });
        if let Some(error) = failure {
            return Err(error);
        }
        let held = arena
            .alloc_slice_with(num_mcelem, |at| tracked[chosen[at]].value)
            .map_err(|_| arena_full())?;
        let numbers = arena
            .alloc_slice_with(num_mcelem + 3, |at| {
                let count = if at < num_mcelem {
                    tracked[chosen[at]].frequency as usize
                } else if at == num_mcelem {
                    minfreq as usize
                } else if at == num_mcelem + 1 {
                    maxfreq as usize
                } else {
                    null_elem_cnt
                };
                (count as f64 / nonnull) as f32
            })
            .map_err(|_| arena_full())?;
        values = held;
        frequencies = numbers;
    }
    // The histogram of distinct-element counts, then their average.
    let num_hist = target.max(2);
    counts[..count_items].sort_unstable_by_key(|&(count, _)| count);
    let histogram = arena
        .alloc_slice_with(num_hist + 1, |_| 0f32)
        .map_err(|_| arena_full())?;
    histogram[num_hist] = (element_no as f64 / nonnull) as f32;
    let delta = analyzed_rows as i64 - 1;
    let mut at = 0usize;
    let mut frac = i64::from(counts[0].1) * (num_hist as i64 - 1);
    for slot in histogram.iter_mut().take(num_hist) {
        while frac <= 0 {
            at += 1;
            frac += i64::from(counts[at].1) * (num_hist as i64 - 1);
        }
        *slot = counts[at].0 as f32;
        frac -= delta;
    }
    Ok(Some(ElementSet {
        values,
        frequencies,
        counts: histogram,
    }))
}
