//! Guarded relation row metadata. Chain traversal requires an issued row read.

use std::sync::{RwLock, RwLockReadGuard};

use super::RowState;
use crate::mem::budget::{Budget, BudgetError};
use crate::mem::fixed_map::{FixedMap, MapFull};

pub struct RowMap {
    state: RwLock<FixedMap<u64, RowState>>,
}

impl RowMap {
    pub(super) fn new(
        budget: &mut Budget,
        name: &'static str,
        capacity: usize,
    ) -> Result<Self, BudgetError> {
        Ok(Self {
            state: RwLock::new(FixedMap::new(budget, name, capacity)?),
        })
    }

    pub(super) fn read(&self) -> RwLockReadGuard<'_, FixedMap<u64, RowState>> {
        self.state.read().expect("table row state lock poisoned")
    }

    #[cfg(test)]
    pub(super) fn test_write(
        &self,
    ) -> std::sync::TryLockResult<std::sync::RwLockWriteGuard<'_, FixedMap<u64, RowState>>> {
        self.state.try_write()
    }

    fn exclusive(&mut self) -> &mut FixedMap<u64, RowState> {
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
    state: RwLockReadGuard<'a, FixedMap<u64, RowState>>,
    next_slot: usize,
}

impl Iterator for RowMapIter<'_> {
    type Item = (u64, RowState);

    fn next(&mut self) -> Option<Self::Item> {
        while self.next_slot < self.state.backing_slot_count() {
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
        RowState::committed_only_at(RowLoc { offset: 0, len: 4 }, lsn)
    }

    fn map(capacity: usize) -> RowMap {
        let mut budget = Budget::new(FixedMap::<u64, RowState>::budget_bytes(capacity));
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
}
