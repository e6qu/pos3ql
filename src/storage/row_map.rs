//! Guarded relation row state. Shared lookups return detached row images.

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
