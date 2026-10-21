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

use crate::mem::mutex::Mutex;
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
    published_lsn: u64,
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
    pub(crate) fn is_complete_at(&self, snapshot: u64) -> bool {
        self.complete && self.published_lsn <= snapshot
    }

    pub(crate) fn publish_at(&mut self, lsn: u64) {
        self.published_lsn = self.published_lsn.max(lsn);
    }

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
            published_lsn: 0,
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
        self.published_lsn = 0;
    }
}

/// Cache buffers and their shared owners are allocated once at startup.
/// The pool's sole reference means a slot is free; issued handles prevent reuse.
pub struct ValueIndexPool {
    indexes: Box<[std::sync::Arc<std::sync::RwLock<ValueIndex>>]>,
    allocation: Mutex<()>,
}

/// An issued handle keeps its cache alive independently of the pool. Returned
/// guards borrow this owner, so the last handle cannot release a borrowed cache.
#[derive(Clone)]
pub struct ValueIndexLease {
    owner: std::sync::Arc<std::sync::RwLock<ValueIndex>>,
    slot: usize,
}

impl ValueIndexLease {
    pub fn slot(&self) -> usize {
        self.slot
    }

    pub fn read(&self) -> std::sync::RwLockReadGuard<'_, ValueIndex> {
        self.owner.read().expect("value index lock poisoned")
    }

    pub fn write(&self) -> std::sync::RwLockWriteGuard<'_, ValueIndex> {
        self.owner.write().expect("value index lock poisoned")
    }
}

impl ValueIndexPool {
    fn owner_bytes() -> usize {
        size_of::<std::sync::RwLock<ValueIndex>>() + 2 * size_of::<std::sync::atomic::AtomicUsize>()
    }

    pub fn budget_bytes(capacity: usize, rows: usize) -> usize {
        size_of::<Self>()
            + capacity
                * (size_of::<std::sync::Arc<std::sync::RwLock<ValueIndex>>>()
                    + Self::owner_bytes()
                    + ValueIndex::budget_bytes(rows))
    }

    pub fn new(budget: &mut Budget, capacity: usize, rows: usize) -> Result<Self, BudgetError> {
        budget.draw(size_of::<Self>(), "value index allocation controls")?;
        budget.draw_array(
            capacity,
            size_of::<std::sync::Arc<std::sync::RwLock<ValueIndex>>>(),
            "value_index_pool",
        )?;
        budget.draw_array(capacity, Self::owner_bytes(), "value index owners")?;
        let mut indexes = Vec::with_capacity(capacity);
        for _ in 0..capacity {
            indexes.push(std::sync::Arc::new(std::sync::RwLock::new(
                ValueIndex::new(budget, "value_index", rows)?,
            )));
        }
        Ok(Self {
            indexes: indexes.into_boxed_slice(),
            allocation: Mutex::new(()),
        })
    }

    /// Issuing handles is serialized; cloning an already issued handle cannot
    /// make an occupied slot appear free. No weak owner can revive a lease.
    pub fn acquire(&self) -> Option<ValueIndexLease> {
        let selected = {
            let _allocation = self
                .allocation
                .lock()
                .expect("value index allocation lock poisoned");
            self.indexes.iter().enumerate().find_map(|(slot, owner)| {
                (std::sync::Arc::strong_count(owner) == 1).then(|| ValueIndexLease {
                    owner: std::sync::Arc::clone(owner),
                    slot,
                })
            })
        }?;
        // The claim owns the only issued handle. Clear outside allocation
        // ownership so an unrelated table can claim another reserved cache.
        selected.write().clear();
        Some(selected)
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
    fn shared_committed_publication_cache_slots_are_independent_and_budgeted() {
        let mut budget = Budget::new(ValueIndexPool::budget_bytes(2, 8));
        let pool = ValueIndexPool::new(&mut budget, 2, 8).unwrap();
        assert_eq!(budget.remaining(), 0);
        let first = pool.acquire().unwrap();
        let second = pool.acquire().unwrap();
        let second_slot = second.slot();
        crate::mem::guard::forbid_alloc(|| {
            let reader = first.read();
            let mut writer = second.write();
            writer.insert(7, 1).unwrap();
            writer.publish_at(9);
            assert!(reader.is_empty());
            assert!(!writer.is_complete_at(7));
            assert!(writer.is_complete_at(9));
            drop(writer);
            drop(reader);
            drop(second);
            let reused = pool.acquire().unwrap();
            assert_eq!(reused.slot(), second_slot);
            assert!(reused.read().is_complete_at(0));
            assert!(reused.read().is_empty());
        });
    }

    #[test]
    fn value_cache_owners_prevent_reuse_until_every_reader_releases() {
        let mut budget = Budget::new(ValueIndexPool::budget_bytes(1, 8));
        let pool = ValueIndexPool::new(&mut budget, 1, 8).unwrap();
        let original = pool.acquire().unwrap();
        crate::mem::guard::forbid_alloc(|| {
            original.write().insert(5, 11).unwrap();
            let retained = original.clone();
            drop(original);
            assert!(pool.acquire().is_none());
            let mut found = 0;
            retained.read().probe(5, |row| found = row);
            assert_eq!(found, 11);
            drop(retained);
            let replacement = pool.acquire().unwrap();
            assert!(replacement.read().is_empty());
            assert!(replacement.read().is_complete_at(0));
        });
    }

    #[test]
    fn value_cache_owners_keep_selected_values_after_pool_removal() {
        let mut budget = Budget::new(ValueIndexPool::budget_bytes(1, 8));
        let pool = ValueIndexPool::new(&mut budget, 1, 8).unwrap();
        let retained = pool.acquire().unwrap();
        retained.write().insert(5, 12).unwrap();
        drop(pool);
        crate::mem::guard::forbid_alloc(|| {
            let mut found = 0;
            retained.read().probe(5, |row| found = row);
            assert_eq!(found, 12);
            retained.write().insert(6, 13).unwrap();
        });
    }

    #[test]
    fn value_cache_owners_concurrent_claims_never_share_an_occupied_slot() {
        use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
        const WORKERS: usize = 4;
        let mut budget = Budget::new(ValueIndexPool::budget_bytes(WORKERS, 8));
        let pool = ValueIndexPool::new(&mut budget, WORKERS, 8).unwrap();
        let start = std::sync::Barrier::new(WORKERS);
        let claims = [const { AtomicUsize::new(usize::MAX) }; WORKERS];
        let failed = AtomicBool::new(false);
        std::thread::scope(|scope| {
            for worker in 0..WORKERS {
                let pool = &pool;
                let start = &start;
                let claims = &claims;
                let failed = &failed;
                scope.spawn(move || {
                    crate::mem::guard::forbid_alloc(|| {
                        for epoch in 1..=50 {
                            start.wait();
                            let selected = pool.acquire();
                            claims[worker].store(
                                selected.as_ref().map_or(usize::MAX, ValueIndexLease::slot),
                                Ordering::Release,
                            );
                            if let Some(selected) = &selected {
                                if selected.write().insert(worker as u64, epoch).is_err() {
                                    failed.store(true, Ordering::Release);
                                }
                            } else {
                                failed.store(true, Ordering::Release);
                            }
                            start.wait();
                            if pool.acquire().is_some() {
                                failed.store(true, Ordering::Release);
                            }
                            for other in 0..WORKERS {
                                if other != worker
                                    && claims[worker].load(Ordering::Acquire)
                                        == claims[other].load(Ordering::Acquire)
                                {
                                    failed.store(true, Ordering::Release);
                                }
                            }
                            if let Some(selected) = &selected {
                                let mut found = 0;
                                selected.read().probe(worker as u64, |row| found = row);
                                if found != epoch {
                                    failed.store(true, Ordering::Release);
                                }
                            }
                            start.wait();
                            drop(selected);
                            start.wait();
                        }
                    })
                });
            }
        });
        assert!(
            !failed.load(Ordering::Acquire),
            "simultaneous claims lost or reused an occupied cache"
        );
        let held: [_; WORKERS] = std::array::from_fn(|_| pool.acquire().unwrap());
        assert!(pool.acquire().is_none());
        drop(held);
        assert!(pool.acquire().is_some());
    }

    #[test]
    fn value_cache_owners_startup_controls_and_slots_are_exactly_budgeted() {
        let mut budget = Budget::new(ValueIndexPool::budget_bytes(0, 8));
        let empty = ValueIndexPool::new(&mut budget, 0, 8).unwrap();
        assert_eq!(budget.remaining(), 0);
        assert!(empty.acquire().is_none());
        let mut short = Budget::new(size_of::<ValueIndexPool>() - 1);
        let error = match ValueIndexPool::new(&mut short, 0, 8) {
            Ok(_) => panic!("undersized cache controls accepted"),
            Err(error) => error,
        };
        assert_eq!(error.what, "value index allocation controls");
        let mut short = Budget::new(ValueIndexPool::budget_bytes(2, 8) - 1);
        let error = match ValueIndexPool::new(&mut short, 2, 8) {
            Ok(_) => panic!("undersized cache payload accepted"),
            Err(error) => error,
        };
        assert_eq!(error.what, "value_index");
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
