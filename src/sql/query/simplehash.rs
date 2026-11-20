//! The order in which `ExecAgg` returns a hash aggregate's groups: its
//! `simplehash` table (`lib/simplehash.h`) of the groups' combined hashes
//! with robin hood insertion, doubling when 90% full or when a run grows
//! too long, walked by `SH_START_ITERATE` backwards from the first empty
//! bucket.
//! <https://github.com/postgres/postgres/blob/REL_18_6/src/include/lib/simplehash.h>
//!
//! The bucket array is leased from `plan_build_bytes`; growing it leases
//! the doubled array and returns the old one, so a table that outgrows the
//! pool fails with a named error.

use std::sync::Arc;

use crate::mem::lease_pool::{LeaseError, LeasePool, MutableSlice};
use crate::sql::eval::{SqlError, sqlstate};
use crate::sql_err;

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

/// One bucket: a group's combined hash and the identity its owner chose.
type Bucket = Option<(u32, u32)>;

/// A hash table's memory did not fit `plan_build_bytes`.
pub(crate) fn table_full(error: LeaseError) -> SqlError {
    sql_err!(
        sqlstate::PROGRAM_LIMIT_EXCEEDED,
        "a hash table exceeds plan_build_bytes: {}",
        error
    )
}

/// A `simplehash` table of groups, each held as its combined hash and an
/// identity the owner chose.
pub(crate) struct SimpleHash {
    table: MutableSlice<Bucket>,
    size: u64,
    members: u64,
    threshold: u64,
}

impl SimpleHash {
    /// A table created for `buckets` entries (`SH_CREATE`).
    pub(crate) fn new(buckets: usize, pool: &Arc<LeasePool>) -> Result<Self, SqlError> {
        let size = ((buckets as f64 / FILL) as u64).max(2).next_power_of_two();
        Ok(Self {
            table: Self::allocate(pool, size)?,
            size,
            members: 0,
            threshold: (size as f64 * FILL) as u64,
        })
    }

    fn allocate(pool: &Arc<LeasePool>, size: u64) -> Result<MutableSlice<Bucket>, SqlError> {
        let mut table = pool.lease(size as usize).map_err(table_full)?;
        for _ in 0..size {
            table.push_reserved(None).map_err(table_full)?;
        }
        Ok(table)
    }

    /// The buckets now.
    pub(crate) fn buckets(&self) -> usize {
        self.size as usize
    }

    /// `SH_RESET`: the groups are dropped, the bucket count and growth
    /// threshold stay.
    pub(crate) fn reset(&mut self) {
        self.table.as_mut_slice().fill(None);
        self.members = 0;
    }

    /// `SH_LOOKUP`: whether a group with `hash` for which `same` holds is
    /// in the table.
    pub(crate) fn contains(
        &self,
        hash: u32,
        same: &mut dyn FnMut(u32) -> Result<bool, SqlError>,
    ) -> Result<bool, SqlError> {
        let table = self.table.as_slice();
        let mask = self.size - 1;
        let mut place = u64::from(hash) & mask;
        while let Some((held_hash, held)) = table[place as usize] {
            if held_hash == hash && same(held)? {
                return Ok(true);
            }
            place = (place + 1) & mask;
        }
        Ok(false)
    }

    /// `SH_GROW`: each entry moved, from the first bucket that is empty or
    /// holds an entry at its optimal place, into the doubled array.
    fn grow(&mut self) -> Result<(), SqlError> {
        let old_size = self.size;
        let size = old_size * 2;
        let mask = size - 1;
        let mut grown = Self::allocate(self.table.pool(), size)?;
        {
            let old = self.table.as_slice();
            let target = grown.as_mut_slice();
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
                    while target[place as usize].is_some() {
                        place = (place + 1) & mask;
                    }
                    target[place as usize] = Some(entry);
                }
            }
        }
        self.table = grown;
        self.size = size;
        self.threshold = (size as f64 * FILL) as u64;
        Ok(())
    }

    /// `SH_INSERT`: grows the table when its entries reached its fill
    /// factor (whether or not the group is new), then adds a group
    /// `identity` with `hash` unless `same` holds for a group already
    /// there. Returns whether it was added.
    pub(crate) fn insert(
        &mut self,
        hash: u32,
        identity: u32,
        same: &mut dyn FnMut(u32) -> Result<bool, SqlError>,
    ) -> Result<bool, SqlError> {
        loop {
            if self.members >= self.threshold {
                self.grow()?;
            }
            let size = self.size;
            let mask = size - 1;
            let members = self.members;
            let table = self.table.as_mut_slice();
            let mut place = u64::from(hash) & mask;
            let mut distance = 0u64;
            let mut regrow = false;
            loop {
                let Some((held_hash, held)) = table[place as usize] else {
                    table[place as usize] = Some((hash, identity));
                    self.members += 1;
                    return Ok(true);
                };
                if held_hash == hash && same(held)? {
                    return Ok(false);
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
                            regrow = true;
                            break;
                        }
                    }
                    if regrow {
                        break;
                    }
                    let mut at = empty;
                    while at != place {
                        let previous = (at + size - 1) & mask;
                        table[at as usize] = table[previous as usize];
                        at = previous;
                    }
                    table[place as usize] = Some((hash, identity));
                    self.members += 1;
                    return Ok(true);
                }
                place = (place + 1) & mask;
                distance += 1;
                if distance > MAX_DIB && members as f64 / size as f64 >= MIN_FILL {
                    regrow = true;
                    break;
                }
            }
            if regrow {
                self.threshold = 0;
            }
        }
    }

    /// `SH_START_ITERATE` and `SH_ITERATE`: the groups' identities,
    /// walking backwards from the first empty bucket.
    pub(crate) fn iterate(
        &self,
        visit: &mut dyn FnMut(u32) -> Result<bool, SqlError>,
    ) -> Result<(), SqlError> {
        let table = self.table.as_slice();
        let mask = self.size - 1;
        let end = (0..self.size)
            .find(|&at| table[at as usize].is_none())
            .expect("an empty bucket");
        let mut current = end;
        loop {
            let entry = table[current as usize];
            current = (current + self.size - 1) & mask;
            if let Some((_, identity)) = entry
                && !visit(identity)?
            {
                return Ok(());
            }
            if current == end {
                return Ok(());
            }
        }
    }
}
