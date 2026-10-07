//! Startup-reserved table slots with guard-bound shared access.

use std::sync::{RwLock, RwLockReadGuard};

use super::{PendingTableDef, Table, TableDef};
use crate::mem::budget::{Budget, BudgetError};
use crate::mem::fixed_vec::{CapacityError, FixedVec};

pub(super) struct TableSlots {
    slots: FixedVec<RwLock<Table>>,
}

impl TableSlots {
    pub(super) fn new(budget: &mut Budget, capacity: usize) -> Result<Self, BudgetError> {
        Ok(Self {
            slots: FixedVec::new(budget, "tables", capacity)?,
        })
    }

    pub(super) fn push(&mut self, table: Table) -> Result<(), CapacityError> {
        self.slots.push(RwLock::new(table))
    }

    pub(super) fn len(&self) -> usize {
        self.slots.len()
    }

    pub(super) fn read(&self, index: usize) -> RwLockReadGuard<'_, Table> {
        self.slots[index].read().expect("table state lock poisoned")
    }

    pub(super) fn get(&self, index: usize) -> Option<RwLockReadGuard<'_, Table>> {
        self.slots
            .get(index)
            .map(|slot| slot.read().expect("table state lock poisoned"))
    }

    // Exclusive storage access excludes every shared guard. Recovery and
    // mutation can borrow the payload directly without relocking a slot.
    pub(super) fn get_mut(&mut self, index: usize) -> &mut Table {
        self.slots[index]
            .get_mut()
            .expect("table state lock poisoned")
    }

    pub(super) fn iter(&self) -> impl Iterator<Item = RwLockReadGuard<'_, Table>> {
        self.slots
            .iter()
            .map(|slot| slot.read().expect("table state lock poisoned"))
    }

    pub(super) fn iter_mut(&mut self) -> impl Iterator<Item = &mut Table> {
        self.slots
            .iter_mut()
            .map(|slot| slot.get_mut().expect("table state lock poisoned"))
    }

    #[cfg(test)]
    pub(super) fn write(&self, index: usize) -> std::sync::RwLockWriteGuard<'_, Table> {
        self.slots[index].write().expect("table state lock poisoned")
    }

    #[cfg(test)]
    pub(super) fn try_write(&self, index: usize) -> Option<std::sync::RwLockWriteGuard<'_, Table>> {
        match self.slots[index].try_write() {
            Ok(guard) => Some(guard),
            Err(std::sync::TryLockError::WouldBlock) => None,
            Err(std::sync::TryLockError::Poisoned(_)) => panic!("table state lock poisoned"),
        }
    }
}

/// A live definition borrow retains the relation guard. Pending versions
/// also borrow Storage, whose exclusive mutation releases or reuses them.
pub struct TableDefinitionRead<'a> {
    pub(super) table: RwLockReadGuard<'a, Table>,
    pub(super) pending: Option<&'a PendingTableDef>,
}

impl core::ops::Deref for TableDefinitionRead<'_> {
    type Target = TableDef;

    fn deref(&self) -> &Self::Target {
        self.pending.map_or(&self.table.def, |pending| &pending.def)
    }
}
