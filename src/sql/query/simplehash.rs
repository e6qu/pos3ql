//! The order in which `ExecAgg` returns a hash aggregate's groups: its
//! `simplehash` table (`lib/simplehash.h`) of the groups' combined hashes
//! with robin hood insertion, doubling when 90% full or when a run grows
//! too long, walked by `SH_START_ITERATE` backwards from the first empty
//! bucket.
//! <https://github.com/postgres/postgres/blob/REL_18_6/src/include/lib/simplehash.h>

use crate::mem::arena::Arena;
use crate::sql::eval::SqlError;

/// `SH_FILLFACTOR`, `SH_GROW_MAX_DIB`, `SH_GROW_MAX_MOVE` and
/// `SH_GROW_MIN_FILLFACTOR`.
const FILL: f64 = 0.9;
const MAX_DIB: u64 = 25;
const MAX_MOVE: u64 = 150;
const MIN_FILL: f64 = 0.1;

/// `murmurhash32`, which `TupleHashTableHash` applies to the combined
/// hash of a row's keys.
pub(crate) fn murmur32(mut h: u32) -> u32 {
    h ^= h >> 16;
    h = h.wrapping_mul(0x85eb_ca6b);
    h ^= h >> 13;
    h = h.wrapping_mul(0xc2b2_ae35);
    h ^ (h >> 16)
}

/// The groups of `rows` input rows inserted in arrival order into a table
/// created for `buckets` entries (`SH_CREATE`), `hash` giving each row's
/// table hash and `same` whether two rows (earlier, later) belong to one
/// group. Returns each group's first row, in the order the table's
/// iteration returns them.
pub(crate) fn iteration_order<'a>(
    rows: usize,
    buckets: usize,
    arena: &'a Arena,
    hash: &mut dyn FnMut(usize) -> Result<u32, SqlError>,
    same: &mut dyn FnMut(usize, usize) -> Result<bool, SqlError>,
) -> Result<&'a [u32], SqlError> {
    let full = || crate::sql::eval::arena_full();
    let sized = |elements: f64| ((elements as u64).max(2)).next_power_of_two();
    let mut size = sized(buckets as f64 / FILL);
    // Each bucket: the group's hash and its first row, or `None`.
    let mut table = arena
        .alloc_slice_with(size as usize, |_| None::<(u32, u32)>)
        .map_err(|_| full())?;
    let mut members = 0u64;
    let mut threshold = (size as f64 * FILL) as u64;
    for row in 0..rows {
        let hashed = hash(row)?;
        'restart: loop {
            if members >= threshold {
                // `SH_GROW`: each entry moved, from the first bucket that is
                // empty or holds an entry at its optimal place.
                let old = &*table;
                let old_size = size;
                size *= 2;
                let mask = size - 1;
                let grown = arena
                    .alloc_slice_with(size as usize, |_| None::<(u32, u32)>)
                    .map_err(|_| full())?;
                let old_mask = old_size - 1;
                let start = (0..old_size)
                    .find(|&at| {
                        old[at as usize].is_none_or(|(hash, _)| u64::from(hash) & old_mask == at)
                    })
                    .expect("an empty bucket");
                for step in 0..old_size {
                    let at = (start + step) % old_size;
                    if let Some(entry) = old[at as usize] {
                        let mut place = u64::from(entry.0) & mask;
                        while grown[place as usize].is_some() {
                            place = (place + 1) & mask;
                        }
                        grown[place as usize] = Some(entry);
                    }
                }
                table = grown;
                threshold = (size as f64 * FILL) as u64;
            }
            let mask = size - 1;
            let mut place = u64::from(hashed) & mask;
            let mut distance = 0u64;
            loop {
                let Some((held_hash, held_row)) = table[place as usize] else {
                    table[place as usize] = Some((hashed, row as u32));
                    members += 1;
                    break 'restart;
                };
                if held_hash == hashed && same(held_row as usize, row)? {
                    break 'restart;
                }
                let optimal = u64::from(held_hash) & mask;
                let held_distance = (place + size - optimal) & mask;
                if distance > held_distance {
                    // Robin hood: shift the run forward to the next empty
                    // bucket and take this one.
                    let mut empty = place;
                    let mut moved = 0u64;
                    loop {
                        empty = (empty + 1) & mask;
                        if table[empty as usize].is_none() {
                            break;
                        }
                        moved += 1;
                        if moved > MAX_MOVE && members as f64 / size as f64 >= MIN_FILL {
                            threshold = 0;
                            continue 'restart;
                        }
                    }
                    let mut at = empty;
                    while at != place {
                        let previous = (at + size - 1) & mask;
                        table[at as usize] = table[previous as usize];
                        at = previous;
                    }
                    table[place as usize] = Some((hashed, row as u32));
                    members += 1;
                    break 'restart;
                }
                place = (place + 1) & mask;
                distance += 1;
                if distance > MAX_DIB && members as f64 / size as f64 >= MIN_FILL {
                    threshold = 0;
                    continue 'restart;
                }
            }
        }
    }
    // `SH_START_ITERATE` and `SH_ITERATE`: backwards from the first empty
    // bucket.
    let order = arena
        .alloc_slice_with(members as usize, |_| 0u32)
        .map_err(|_| full())?;
    let mask = size - 1;
    let end = (0..size)
        .find(|&at| table[at as usize].is_none())
        .expect("an empty bucket");
    let mut current = end;
    let mut count = 0;
    loop {
        let entry = table[current as usize];
        current = (current + size - 1) & mask;
        if let Some((_, row)) = entry {
            order[count] = row;
            count += 1;
        }
        if current == end {
            break;
        }
    }
    Ok(order)
}
