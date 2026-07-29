//! A fixed-capacity value→rowid index: a hash *multimap* over 64-bit value
//! hashes. It answers "which rows might hold this value" for uniqueness and
//! foreign-key probes in O(1) instead of a full table scan.
//!
//! Unlike [`super::fixed_map::FixedMap`], several entries may share a key: a
//! genuine 64-bit collision between two distinct column values must keep both
//! rows discoverable, and the caller re-verifies every candidate against the
//! actual row bytes, so a shared-hash entry is only ever a false positive, never
//! a false negative. Open addressing with linear probing and backward-shift
//! deletion (no tombstones); the slot array is sized to keep the load factor at
//! or below one half; inserting past the requested capacity is a loud error
//! naming the index — never growth.

use core::fmt;

use super::budget::{Budget, BudgetError};

pub struct ValueIndex {
    what: &'static str,
    slots: Box<[Option<(u64, u64)>]>, // (value_hash, rowid)
    mask: usize,
    len: usize,
    max_len: usize,
    /// Whether this cache contains every committed row. A full cache becomes
    /// incomplete instead of imposing a correctness limit on the table; its
    /// callers must then use the authoritative row store for a conclusive
    /// negative answer.
    complete: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IndexFull {
    pub what: &'static str,
    pub capacity: usize,
}

impl fmt::Display for IndexFull {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "value index '{}' is full (capacity {})",
            self.what, self.capacity
        )
    }
}

impl std::error::Error for IndexFull {}

impl ValueIndex {
    /// Bytes `new` will draw from the budget for a given capacity.
    pub fn budget_bytes(capacity: usize) -> usize {
        Self::slot_count(capacity) * size_of::<Option<(u64, u64)>>()
    }

    fn slot_count(capacity: usize) -> usize {
        capacity
            .checked_mul(2)
            .and_then(|n| n.checked_next_power_of_two())
            .unwrap_or_else(|| panic!("value index capacity {capacity} is unrepresentable"))
            .max(8)
    }

    pub fn new(
        budget: &mut Budget,
        what: &'static str,
        capacity: usize,
    ) -> Result<Self, BudgetError> {
        let slot_count = Self::slot_count(capacity);
        budget.draw_array(slot_count, size_of::<Option<(u64, u64)>>(), what)?;
        let mut slots = Vec::new();
        slots.resize_with(slot_count, || None);
        Ok(Self {
            what,
            slots: slots.into_boxed_slice(),
            mask: slot_count - 1,
            len: 0,
            max_len: capacity,
            complete: true,
        })
    }

    fn home(&self, hash: u64) -> usize {
        (hash as usize) & self.mask
    }

    /// Records that the row `rowid` holds a value with hash `hash`. Fails
    /// loudly at capacity — a constrained table cannot silently outgrow its
    /// index.
    pub fn insert(&mut self, hash: u64, rowid: u64) -> Result<(), IndexFull> {
        if self.len == self.max_len {
            return Err(IndexFull {
                what: self.what,
                capacity: self.max_len,
            });
        }
        let mut i = self.home(hash);
        while self.slots[i].is_some() {
            i = (i + 1) & self.mask;
        }
        self.slots[i] = Some((hash, rowid));
        self.len += 1;
        Ok(())
    }

    /// Visits the rowid of every entry whose value hash equals `hash`. All such
    /// entries lie in the probe run from `home(hash)` up to the first empty
    /// slot, so scanning that run and filtering by hash finds every candidate.
    pub fn probe(&self, hash: u64, mut visit: impl FnMut(u64)) {
        let mut i = self.home(hash);
        while let Some((h, rowid)) = self.slots[i] {
            if h == hash {
                visit(rowid);
            }
            i = (i + 1) & self.mask;
        }
    }

    /// Removes the specific `(hash, rowid)` entry, restoring the probe invariant
    /// by backward-shifting displaced entries into the hole. Returns whether an
    /// entry was found.
    pub fn remove(&mut self, hash: u64, rowid: u64) -> bool {
        let mut hole = self.home(hash);
        loop {
            match self.slots[hole] {
                Some((h, r)) if h == hash && r == rowid => break,
                Some(_) => hole = (hole + 1) & self.mask,
                None => return false,
            }
        }
        self.remove_at(hole);
        true
    }

    /// Removes the entry belonging to `rowid`, regardless of its value hash.
    ///
    /// A table enforcer holds at most one entry per row. Commit publication
    /// uses this path so replacing an object-resident row never has to fetch
    /// its old bytes after the WAL record is already durable.
    pub fn remove_rowid(&mut self, rowid: u64) -> bool {
        let Some(hole) = self
            .slots
            .iter()
            .position(|entry| matches!(entry, Some((_, candidate)) if *candidate == rowid))
        else {
            return false;
        };
        self.remove_at(hole);
        true
    }

    fn remove_at(&mut self, mut hole: usize) {
        self.slots[hole] = None;
        self.len -= 1;

        let mut probe = hole;
        loop {
            probe = (probe + 1) & self.mask;
            let Some((h, _)) = self.slots[probe] else {
                break;
            };
            let home = self.home(h);
            // The entry at `probe` may fill the hole only if its home does not
            // lie cyclically within (hole, probe] — otherwise lookups starting
            // at its home would no longer reach it.
            let home_in_between = if hole < probe {
                hole < home && home <= probe
            } else {
                home > hole || home <= probe
            };
            if !home_in_between {
                self.slots[hole] = self.slots[probe].take();
                hole = probe;
            }
        }
    }

    pub fn len(&self) -> usize {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    pub fn capacity(&self) -> usize {
        self.max_len
    }

    pub fn is_complete(&self) -> bool {
        self.complete
    }

    pub fn mark_incomplete(&mut self) {
        self.complete = false;
    }

    pub fn clear(&mut self) {
        self.slots.fill_with(|| None);
        self.len = 0;
        self.complete = true;
    }
}

/// A startup-allocated pool of [`ValueIndex`] buffers with a free list. Every
/// index's slot array is reserved once at construction, so acquiring one for a
/// new constraint at runtime allocates nothing — it just clears a spare and
/// hands back its slot. Exhausting the pool is a loud error, never growth.
pub struct ValueIndexPool {
    indexes: Box<[ValueIndex]>,
    free: super::fixed_vec::FixedVec<u32>,
}

impl ValueIndexPool {
    /// Bytes `new` draws for `capacity` indexes each holding up to `rows`
    /// entries (the container, each index's slots, and the free list).
    pub fn budget_bytes(capacity: usize, rows: usize) -> usize {
        capacity * size_of::<ValueIndex>()
            + capacity * ValueIndex::budget_bytes(rows)
            + capacity * size_of::<u32>()
    }

    pub fn new(budget: &mut Budget, capacity: usize, rows: usize) -> Result<Self, BudgetError> {
        budget.draw_array(capacity, size_of::<ValueIndex>(), "value_index_pool")?;
        let mut indexes = Vec::with_capacity(capacity);
        for _ in 0..capacity {
            indexes.push(ValueIndex::new(budget, "value_index", rows)?);
        }
        let mut free = super::fixed_vec::FixedVec::new(budget, "value_index_free", capacity)?;
        for slot in (0..capacity as u32).rev() {
            free.push(slot).expect("free list sized to capacity");
        }
        Ok(Self {
            indexes: indexes.into_boxed_slice(),
            free,
        })
    }

    /// Claims a cleared index, returning its slot, or `None` if the pool is
    /// exhausted (the caller raises a loud error).
    pub fn acquire(&mut self) -> Option<u32> {
        let slot = self.free.pop()?;
        self.indexes[slot as usize].clear();
        Some(slot)
    }

    /// Returns a slot to the pool, clearing its index.
    pub fn release(&mut self, slot: u32) {
        self.indexes[slot as usize].clear();
        self.free.push(slot).expect("free list sized to capacity");
    }

    pub fn get(&self, slot: u32) -> &ValueIndex {
        &self.indexes[slot as usize]
    }

    pub fn get_mut(&mut self, slot: u32) -> &mut ValueIndex {
        &mut self.indexes[slot as usize]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn collect(index: &ValueIndex, hash: u64) -> Vec<u64> {
        let mut out = Vec::new();
        index.probe(hash, |rowid| out.push(rowid));
        out.sort_unstable();
        out
    }

    #[test]
    fn insert_probe_remove() {
        let mut budget = Budget::new(1 << 16);
        let mut index = ValueIndex::new(&mut budget, "test", 8).unwrap();
        index.insert(0x1111, 1).unwrap();
        index.insert(0x2222, 2).unwrap();
        assert_eq!(collect(&index, 0x1111), [1]);
        assert_eq!(collect(&index, 0x2222), [2]);
        assert_eq!(collect(&index, 0x3333), Vec::<u64>::new());
        assert!(index.remove(0x1111, 1));
        assert_eq!(collect(&index, 0x1111), Vec::<u64>::new());
        assert_eq!(collect(&index, 0x2222), [2]);
        assert!(!index.remove(0x1111, 1));
        assert_eq!(index.len(), 1);
    }

    #[test]
    fn shared_hash_keeps_every_rowid() {
        // Distinct values that collide to one hash must both stay discoverable.
        let mut budget = Budget::new(1 << 16);
        let mut index = ValueIndex::new(&mut budget, "test", 16).unwrap();
        for rowid in 0..5u64 {
            index.insert(0xdead, rowid).unwrap();
        }
        assert_eq!(collect(&index, 0xdead), [0, 1, 2, 3, 4]);
        // Removing one leaves the rest reachable, in any order.
        assert!(index.remove(0xdead, 2));
        assert_eq!(collect(&index, 0xdead), [0, 1, 3, 4]);
        assert!(index.remove(0xdead, 0));
        assert!(index.remove(0xdead, 4));
        assert_eq!(collect(&index, 0xdead), [1, 3]);
    }

    #[test]
    fn completeness_is_explicit_and_reset_with_the_cache() {
        let mut budget = Budget::new(1 << 16);
        let mut index = ValueIndex::new(&mut budget, "test", 1).unwrap();
        index.insert(7, 1).unwrap();
        assert!(index.insert(8, 2).is_err());
        index.mark_incomplete();
        assert!(!index.is_complete());
        assert_eq!(collect(&index, 7), [1]);
        index.clear();
        assert!(index.is_complete());
        assert!(index.is_empty());
    }

    #[test]
    fn remove_rowid_restores_every_probe_run() {
        let mut budget = Budget::new(1 << 16);
        let mut index = ValueIndex::new(&mut budget, "test", 8).unwrap();
        // All four homes collide, and the run wraps around the slot array.
        for (hash, rowid) in [(7, 10), (15, 20), (23, 30), (31, 40)] {
            index.insert(hash, rowid).unwrap();
        }
        assert!(index.remove_rowid(20));
        assert!(!index.remove_rowid(99));
        assert_eq!(collect(&index, 7), [10]);
        assert_eq!(collect(&index, 15), Vec::<u64>::new());
        assert_eq!(collect(&index, 23), [30]);
        assert_eq!(collect(&index, 31), [40]);
        assert_eq!(index.len(), 3);
    }

    #[test]
    fn full_index_is_a_loud_error() {
        let mut budget = Budget::new(1 << 16);
        let mut index = ValueIndex::new(&mut budget, "constrained", 2).unwrap();
        index.insert(1, 1).unwrap();
        index.insert(2, 2).unwrap();
        let err = index.insert(3, 3).unwrap_err();
        assert_eq!(err.what, "constrained");
        assert_eq!(err.capacity, 2);
    }

    #[test]
    fn probe_finds_entries_displaced_across_wraparound() {
        // Force home near the top of the slot array so the probe run wraps.
        let mut budget = Budget::new(1 << 16);
        let mut index = ValueIndex::new(&mut budget, "test", 4).unwrap();
        // slot_count = 8, mask = 7; hash & 7 == 7 for all of these.
        let hashes = [7u64, 15, 23, 31];
        for (rowid, h) in hashes.iter().enumerate() {
            index.insert(*h, rowid as u64).unwrap();
        }
        for (rowid, h) in hashes.iter().enumerate() {
            assert_eq!(collect(&index, *h), [rowid as u64]);
        }
        // Remove the middle two; the survivors stay reachable across the wrap.
        assert!(index.remove(15, 1));
        assert!(index.remove(23, 2));
        assert_eq!(collect(&index, 7), [0]);
        assert_eq!(collect(&index, 31), [3]);
    }

    #[test]
    fn differential_against_reference_multimap() {
        // Deterministic xorshift64* drives a random insert/remove/probe mix,
        // cross-checked against a Vec of (hash, rowid) pairs.
        let mut state: u64 = 0x1234_5678_9abc_def0;
        let mut next = move || {
            state ^= state >> 12;
            state ^= state << 25;
            state ^= state >> 27;
            state = state.wrapping_mul(0x2545_f491_4f6c_dd1d);
            state
        };
        let mut budget = Budget::new(1 << 20);
        let mut index = ValueIndex::new(&mut budget, "test", 64).unwrap();
        let mut reference: Vec<(u64, u64)> = Vec::new();
        for _ in 0..20_000 {
            // Few distinct hashes so collisions are frequent.
            let hash = next() % 8;
            let rowid = next() % 200;
            match next() % 3 {
                0 if reference.len() < 64 && !reference.contains(&(hash, rowid)) => {
                    index.insert(hash, rowid).unwrap();
                    reference.push((hash, rowid));
                }
                1 => {
                    let removed = index.remove(hash, rowid);
                    let before = reference.len();
                    reference.retain(|&e| e != (hash, rowid));
                    assert_eq!(removed, before != reference.len());
                }
                _ => {
                    let mut want: Vec<u64> = reference
                        .iter()
                        .filter(|&&(h, _)| h == hash)
                        .map(|&(_, r)| r)
                        .collect();
                    want.sort_unstable();
                    assert_eq!(collect(&index, hash), want);
                }
            }
            assert_eq!(index.len(), reference.len());
        }
    }

    #[test]
    fn operations_do_not_allocate() {
        let mut budget = Budget::new(1 << 16);
        let mut index = ValueIndex::new(&mut budget, "test", 32).unwrap();
        crate::mem::guard::forbid_alloc(|| {
            for k in 0..32u64 {
                index.insert(k % 4, k).unwrap();
            }
            for k in 0..32u64 {
                index.remove(k % 4, k);
            }
        });
    }
}
