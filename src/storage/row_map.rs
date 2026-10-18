//! Guarded relation row metadata. Chain traversal requires an issued row read.

use std::sync::{RwLock, RwLockReadGuard, RwLockWriteGuard};

use super::RowState;
use crate::mem::budget::{Budget, BudgetError};
use crate::mem::fixed_map::{FixedMap, MapFull};

/// Hash buckets contain only lookup positions. Removing another key never
/// moves a live row slot, so maintenance may resume at a physical slot.
pub(crate) struct RowEntries {
    index: FixedMap<u64, usize>,
    slots: Box<[RowSlot]>,
    free: Option<usize>,
    scan_end: usize,
    what: &'static str,
}

enum RowSlot {
    Free { next: Option<usize> },
    Occupied { rowid: u64, state: RowState },
}

impl RowEntries {
    fn new(budget: &mut Budget, what: &'static str, capacity: usize) -> Result<Self, BudgetError> {
        let index = FixedMap::new(budget, what, capacity)?;
        budget.draw_array(capacity, size_of::<RowSlot>(), what)?;
        let slots = (0..capacity)
            .map(|slot| RowSlot::Free {
                next: (slot + 1 < capacity).then_some(slot + 1),
            })
            .collect::<Vec<_>>()
            .into_boxed_slice();
        Ok(Self {
            index,
            slots,
            free: (capacity != 0).then_some(0),
            scan_end: 0,
            what,
        })
    }

    pub(super) fn get(&self, rowid: &u64) -> Option<&RowState> {
        let &slot = self.index.get(rowid)?;
        match &self.slots[slot] {
            RowSlot::Occupied {
                rowid: stored,
                state,
            } => {
                debug_assert_eq!(stored, rowid);
                Some(state)
            }
            RowSlot::Free { .. } => panic!("row lookup references a free slot"),
        }
    }

    pub(super) fn get_mut(&mut self, rowid: &u64) -> Option<&mut RowState> {
        let &slot = self.index.get(rowid)?;
        match &mut self.slots[slot] {
            RowSlot::Occupied {
                rowid: stored,
                state,
            } => {
                debug_assert_eq!(stored, rowid);
                Some(state)
            }
            RowSlot::Free { .. } => panic!("row lookup references a free slot"),
        }
    }

    pub(super) fn insert(
        &mut self,
        rowid: u64,
        state: RowState,
    ) -> Result<Option<RowState>, MapFull> {
        if let Some(existing) = self.get_mut(&rowid) {
            return Ok(Some(core::mem::replace(existing, state)));
        }
        let Some(slot) = self.free else {
            return Err(MapFull {
                what: self.what,
                capacity: self.slots.len(),
            });
        };
        let next = match &self.slots[slot] {
            RowSlot::Free { next } => *next,
            RowSlot::Occupied { .. } => panic!("row free list references a live slot"),
        };
        let previous = self.index.insert(rowid, slot)?;
        debug_assert!(previous.is_none());
        self.slots[slot] = RowSlot::Occupied { rowid, state };
        self.scan_end = self.scan_end.max(slot + 1);
        self.free = next;
        Ok(None)
    }

    pub(super) fn remove(&mut self, rowid: &u64) -> Option<RowState> {
        let slot = self.index.remove(rowid)?;
        let removed = core::mem::replace(&mut self.slots[slot], RowSlot::Free { next: self.free });
        self.free = Some(slot);
        match removed {
            RowSlot::Occupied {
                rowid: stored,
                state,
            } => {
                debug_assert_eq!(stored, *rowid);
                Some(state)
            }
            RowSlot::Free { .. } => panic!("row removal references a free slot"),
        }
    }

    pub(super) fn retain(&mut self, mut keep: impl FnMut(&u64, &mut RowState) -> bool) {
        for slot in 0..self.scan_end {
            let remove = match &mut self.slots[slot] {
                RowSlot::Occupied { rowid, state } => (!keep(rowid, state)).then_some(*rowid),
                RowSlot::Free { .. } => None,
            };
            if let Some(rowid) = remove {
                self.remove(&rowid).expect("selected row exists");
            }
        }
    }

    pub(super) fn len(&self) -> usize {
        self.index.len()
    }

    pub(super) fn is_empty(&self) -> bool {
        self.index.is_empty()
    }

    pub(super) fn capacity(&self) -> usize {
        self.slots.len()
    }

    /// Monotone so clearing and reusing a table cannot invalidate a paused cursor.
    pub(super) fn scan_end(&self) -> usize {
        self.scan_end
    }

    pub(super) fn entry_at_slot(&self, slot: usize) -> Option<(&u64, &RowState)> {
        match self.slots.get(slot)? {
            RowSlot::Occupied { rowid, state } => Some((rowid, state)),
            RowSlot::Free { .. } => None,
        }
    }

    pub(super) fn entry_at_slot_mut(&mut self, slot: usize) -> Option<(&u64, &mut RowState)> {
        match self.slots.get_mut(slot)? {
            RowSlot::Occupied { rowid, state } => Some((&*rowid, state)),
            RowSlot::Free { .. } => None,
        }
    }

    pub(super) fn iter(&self) -> impl Iterator<Item = (&u64, &RowState)> {
        self.slots[..self.scan_end]
            .iter()
            .filter_map(|slot| match slot {
                RowSlot::Occupied { rowid, state } => Some((rowid, state)),
                RowSlot::Free { .. } => None,
            })
    }

    pub(super) fn iter_mut(&mut self) -> impl Iterator<Item = (&u64, &mut RowState)> {
        self.slots[..self.scan_end]
            .iter_mut()
            .filter_map(|slot| match slot {
                RowSlot::Occupied { rowid, state } => Some((&*rowid, state)),
                RowSlot::Free { .. } => None,
            })
    }

    pub(super) fn clear(&mut self) {
        self.index.clear();
        let capacity = self.slots.len();
        for (slot, entry) in self.slots.iter_mut().enumerate() {
            *entry = RowSlot::Free {
                next: (slot + 1 < capacity).then_some(slot + 1),
            };
        }
        self.free = (capacity != 0).then_some(0);
    }
}

pub struct RowMap {
    state: RwLock<RowEntries>,
}

impl RowMap {
    pub(super) fn budget_bytes(capacity: usize) -> usize {
        FixedMap::<u64, usize>::budget_bytes(capacity)
            .saturating_add(capacity.saturating_mul(size_of::<RowSlot>()))
    }

    pub(super) fn new(
        budget: &mut Budget,
        name: &'static str,
        capacity: usize,
    ) -> Result<Self, BudgetError> {
        Ok(Self {
            state: RwLock::new(RowEntries::new(budget, name, capacity)?),
        })
    }

    pub(super) fn try_read_for_batch(
        &self,
    ) -> Result<RwLockReadGuard<'_, RowEntries>, super::row_ownership::LockWait<'_>> {
        super::row_ownership::try_read(&self.state, "table row state lock poisoned")
    }

    pub(super) fn try_write_for_batch(
        &self,
    ) -> Result<RwLockWriteGuard<'_, RowEntries>, super::row_ownership::LockWait<'_>> {
        super::row_ownership::try_write(&self.state, "table row state lock poisoned")
    }

    pub(super) fn read(&self) -> RwLockReadGuard<'_, RowEntries> {
        self.state.read().expect("table row state lock poisoned")
    }

    pub(super) fn write(&self) -> RwLockWriteGuard<'_, RowEntries> {
        self.state.write().expect("table row state lock poisoned")
    }

    #[cfg(test)]
    pub(super) fn test_write(
        &self,
    ) -> std::sync::TryLockResult<std::sync::RwLockWriteGuard<'_, RowEntries>> {
        self.state.try_write()
    }

    fn exclusive(&mut self) -> &mut RowEntries {
        self.state.get_mut().expect("table row state lock poisoned")
    }

    pub fn get(&self, rowid: &u64) -> Option<RowState> {
        self.read().get(rowid).copied()
    }

    pub fn get_mut(&mut self, rowid: &u64) -> Option<&mut RowState> {
        self.exclusive().get_mut(rowid)
    }

    pub fn insert(&mut self, rowid: u64, state: RowState) -> Result<Option<RowState>, MapFull> {
        self.exclusive().insert(rowid, state)
    }

    pub fn remove(&mut self, rowid: &u64) -> Option<RowState> {
        self.exclusive().remove(rowid)
    }

    pub fn clear(&mut self) {
        self.exclusive().clear();
    }

    pub fn len(&self) -> usize {
        self.read().len()
    }

    pub fn is_empty(&self) -> bool {
        self.read().is_empty()
    }

    pub fn capacity(&self) -> usize {
        self.read().capacity()
    }

    pub fn iter(&self) -> RowMapIter<'_> {
        RowMapIter {
            state: self.read(),
            next_slot: 0,
        }
    }

    pub fn iter_mut(&mut self) -> impl Iterator<Item = (&u64, &mut RowState)> {
        self.exclusive().iter_mut()
    }
}

/// Holds one coherent map view while returning copies that survive release.
pub struct RowMapIter<'a> {
    state: RwLockReadGuard<'a, RowEntries>,
    next_slot: usize,
}

impl Iterator for RowMapIter<'_> {
    type Item = (u64, RowState);

    fn next(&mut self) -> Option<Self::Item> {
        while self.next_slot < self.state.scan_end() {
            let slot = self.next_slot;
            self.next_slot += 1;
            if let Some((&rowid, state)) = self.state.entry_at_slot(slot) {
                return Some((rowid, *state));
            }
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::RowLoc;

    fn image(lsn: u64) -> RowState {
        RowState::committed_only_at(RowLoc::test(0, 4), lsn)
    }

    fn map(capacity: usize) -> RowMap {
        let mut budget = Budget::new(RowMap::budget_bytes(capacity));
        let map = RowMap::new(&mut budget, "test table rows", capacity).unwrap();
        assert_eq!(budget.remaining(), 0);
        map
    }

    #[test]
    fn table_state_lookup_images_survive_removal_and_slot_reuse() {
        let mut rows = map(2);
        crate::mem::guard::forbid_alloc(|| {
            rows.insert(1, image(7)).unwrap();
            let retained = rows.get(&1).unwrap();
            rows.get_mut(&1).unwrap().committed_lsn = 8;
            assert_eq!(retained.committed_lsn, 7);
            assert_eq!(rows.remove(&1).unwrap().committed_lsn, 8);
            rows.insert(2, image(9)).unwrap();
            assert!(rows.get(&1).is_none());
            assert_eq!(retained.committed_lsn, 7);
            assert_eq!(rows.get(&2).unwrap().committed_lsn, 9);
            rows.clear();
            assert!(rows.is_empty());
            assert_eq!(retained.committed_lsn, 7);
        });
    }

    #[test]
    fn table_state_iteration_retains_one_guard_and_returns_detached_images() {
        let mut rows = map(2);
        rows.insert(1, image(7)).unwrap();
        rows.insert(2, image(8)).unwrap();
        crate::mem::guard::forbid_alloc(|| {
            let mut view = rows.iter();
            let (rowid, retained) = view.next().unwrap();
            assert!(rows.state.try_write().is_err());
            assert_eq!(view.count(), 1);
            // Consuming the iterator releases its guard; its detached item
            // cannot retain a lock or observe later mutation.
            let mut writer = rows.state.try_write().unwrap();
            writer.get_mut(&rowid).unwrap().committed_lsn = 99;
            assert_ne!(retained.committed_lsn, 99);
        });
    }

    #[test]
    fn table_state_readers_observe_complete_row_map_updates_without_allocation() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<RowMap>();
        let mut rows = map(4);
        for rowid in 1..=4 {
            rows.insert(rowid, image(0)).unwrap();
        }
        let start = std::sync::Barrier::new(4);
        std::thread::scope(|scope| {
            for _ in 0..3 {
                let rows = &rows;
                let start = &start;
                scope.spawn(move || {
                    start.wait();
                    crate::mem::guard::forbid_alloc(|| {
                        for _ in 0..500 {
                            let point = rows.get(&1).unwrap();
                            assert_eq!(point.committed_lsn, point.checkpoint_change_lsn);
                            let mut epoch = None;
                            let mut count = 0;
                            for (_, state) in rows.iter() {
                                assert_eq!(state.committed_lsn, state.checkpoint_change_lsn);
                                assert_eq!(
                                    *epoch.get_or_insert(state.committed_lsn),
                                    state.committed_lsn
                                );
                                count += 1;
                            }
                            assert_eq!(count, 4);
                        }
                    });
                });
            }
            start.wait();
            crate::mem::guard::forbid_alloc(|| {
                for generation in 1..=500 {
                    let mut writer = rows.state.write().unwrap();
                    for (_, state) in writer.iter_mut() {
                        state.committed_lsn = generation;
                        state.checkpoint_change_lsn = generation;
                    }
                }
            });
        });
        assert_eq!(rows.get(&1).unwrap().committed_lsn, 500);
    }

    #[test]
    fn table_state_capacity_failure_preserves_rows_and_reuses_released_space() {
        let mut rows = map(4);
        crate::mem::guard::forbid_alloc(|| {
            for rowid in 1..=4 {
                rows.insert(rowid, image(rowid)).unwrap();
            }
            let error = rows.insert(5, image(5)).unwrap_err();
            assert_eq!(error.what, "test table rows");
            assert_eq!(error.capacity, 4);
            assert_eq!(rows.len(), rows.capacity());
            rows.remove(&2).unwrap();
            rows.insert(5, image(5)).unwrap();
            let mut found = 0u8;
            for (rowid, state) in rows.iter() {
                assert_eq!(state.committed_lsn, rowid);
                assert_eq!(found & (1 << rowid), 0);
                found |= 1 << rowid;
            }
            assert_eq!(found, (1 << 1) | (1 << 3) | (1 << 4) | (1 << 5));
        });
    }

    fn colliding_keys<const N: usize>(capacity: usize) -> [u64; N] {
        use core::hash::{Hash, Hasher};
        let mask = (capacity * 2).next_power_of_two().max(8) - 1;
        let mut keys = [0; N];
        let mut count = 0;
        for key in 0..131_072u64 {
            let mut hash = crate::mem::fixed_map::Fnv1aHasher::default();
            key.hash(&mut hash);
            if hash.finish() as usize & mask == mask {
                keys[count] = key;
                count += 1;
                if count == N {
                    return keys;
                }
            }
        }
        panic!("collision fixture did not fill its fixed key array");
    }

    #[test]
    fn stable_row_slots_collision_backshift_preserves_unvisited_positions() {
        let mut rows = map(8);
        let keys = colliding_keys::<9>(8);
        crate::mem::guard::forbid_alloc(|| {
            for (slot, rowid) in keys[..8].iter().enumerate() {
                rows.insert(*rowid, image(slot as u64)).unwrap();
            }
            let mut state = rows.write();
            for (slot, rowid) in keys[..8].iter().enumerate() {
                assert_eq!(state.entry_at_slot(slot).unwrap().0, rowid);
            }
            // Lookup deletion crosses the wrapped cluster; row cells stay put.
            state.remove(&keys[0]).unwrap();
            state.remove(&keys[3]).unwrap();
            state.insert(keys[8], image(99)).unwrap();
            assert_eq!(state.entry_at_slot(3).unwrap().0, &keys[8]);
            for slot in [1, 2, 4, 5, 6, 7] {
                assert_eq!(state.entry_at_slot(slot).unwrap().0, &keys[slot]);
                assert_eq!(state.get(&keys[slot]).unwrap().committed_lsn, slot as u64);
            }
            let mut visited = 0;
            for slot in 1..state.scan_end() {
                if state.entry_at_slot(slot).is_some() {
                    visited += 1;
                }
            }
            assert_eq!(visited, 7);
            assert!(state.get(&keys[0]).is_none());
            assert!(state.get(&keys[3]).is_none());
        });
    }

    #[test]
    fn stable_row_slots_exhaustion_update_and_reuse_leave_other_rows_valid() {
        let mut rows = map(2);
        crate::mem::guard::forbid_alloc(|| {
            rows.insert(1, image(7)).unwrap();
            rows.insert(2, image(8)).unwrap();
            let retained = rows.get(&1).unwrap();
            let error = rows.insert(3, image(9)).unwrap_err();
            assert_eq!(error.what, "test table rows");
            assert_eq!(error.capacity, 2);
            assert_eq!(rows.len(), 2);
            assert_eq!(rows.insert(1, image(10)).unwrap().unwrap(), retained);
            assert_eq!(rows.remove(&1).unwrap().committed_lsn, 10);
            rows.insert(3, image(11)).unwrap();
            let state = rows.read();
            assert_eq!(state.entry_at_slot(0).unwrap().0, &3);
            assert_eq!(state.entry_at_slot(1).unwrap().0, &2);
            assert_eq!(state.get(&2).unwrap().committed_lsn, 8);
            assert!(state.get(&1).is_none());
            assert_eq!(retained.committed_lsn, 7);
        });
    }

    #[test]
    fn stable_row_slots_retention_visits_wrapped_collision_entries_once() {
        let mut rows = map(8);
        let keys = colliding_keys::<8>(8);
        crate::mem::guard::forbid_alloc(|| {
            for (position, key) in keys.iter().enumerate() {
                rows.insert(*key, image(position as u64)).unwrap();
            }
            let mut visited = [0u8; 8];
            rows.write().retain(|key, state| {
                let position = keys.iter().position(|candidate| candidate == key).unwrap();
                visited[position] += 1;
                state.committed_lsn = 99;
                position % 2 == 1
            });
            assert_eq!(visited, [1; 8]);
            assert_eq!(rows.len(), 4);
            for (position, key) in keys.iter().enumerate() {
                if position % 2 == 1 {
                    assert_eq!(rows.get(key).unwrap().committed_lsn, 99);
                    assert_eq!(rows.read().entry_at_slot(position).unwrap().0, key);
                } else {
                    assert!(rows.get(key).is_none());
                }
            }
        });
    }

    #[test]
    fn stable_row_slots_startup_reserves_both_arrays_without_hidden_capacity() {
        for capacity in [0, 1, 8, 17] {
            let bytes = RowMap::budget_bytes(capacity);
            let mut budget = Budget::new(bytes);
            let rows = RowMap::new(&mut budget, "stable rows", capacity).unwrap();
            assert_eq!(budget.remaining(), 0);
            assert_eq!(rows.capacity(), capacity);
            let mut short = Budget::new(bytes - 1);
            let error = RowMap::new(&mut short, "stable rows", capacity)
                .err()
                .unwrap();
            assert_eq!(error.what, "stable rows");
            let prior = FixedMap::<u64, RowState>::budget_bytes(capacity)
                + size_of::<RwLock<FixedMap<u64, RowState>>>();
            let current = bytes + size_of::<RowMap>();
            assert!(current < prior, "stable metadata exceeds prior reservation");
            eprintln!(
                "row metadata reservation: capacity={capacity} prior={prior} current={current}"
            );
        }
    }

    #[test]
    fn stable_row_slots_sparse_scan_limit_survives_clear_and_reuse() {
        let mut rows = map(128);
        crate::mem::guard::forbid_alloc(|| {
            assert_eq!(rows.read().scan_end(), 0);
            rows.insert(1, image(7)).unwrap();
            let paused = rows.read().scan_end();
            assert_eq!(paused, 1);
            rows.remove(&1).unwrap();
            rows.clear();
            assert_eq!(rows.read().scan_end(), paused);
            rows.insert(2, image(8)).unwrap();
            assert_eq!(rows.read().scan_end(), paused);
            rows.insert(3, image(9)).unwrap();
            assert_eq!(rows.read().scan_end(), 2);
            assert_eq!(rows.capacity(), 128);
            assert_eq!(rows.iter().count(), 2);
            assert_eq!(rows.get(&2).unwrap().committed_lsn, 8);
            assert_eq!(rows.get(&3).unwrap().committed_lsn, 9);
        });
    }
}
