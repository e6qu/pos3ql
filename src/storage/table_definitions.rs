//! Guards retain committed definitions and the pending slots that issued a view.

use core::mem::size_of;
use core::ops::Deref;
use std::sync::{RwLock, RwLockReadGuard};

use super::{PendingTableDef, PendingTableDefSlot, TableDef};
use crate::mem::budget::{Budget, BudgetError};
use crate::mem::fixed_vec::FixedVec;

pub(super) struct DefinitionState {
    pub(super) committed: TableDef,
    pub(super) pending: Option<PendingDefinitionHead>,
}

#[derive(Clone, Copy)]
pub(super) struct PendingDefinitionHead {
    pub(super) tail: u32,
    pub(super) transaction: u32,
}

pub(super) struct TableDefinition {
    state: RwLock<DefinitionState>,
}

impl TableDefinition {
    pub(super) fn new(committed: TableDef) -> Self {
        Self { state: RwLock::new(DefinitionState {
            committed, pending: None,
        }) }
    }

    pub(super) fn read(&self) -> RwLockReadGuard<'_, DefinitionState> {
        self.state.read().expect("table definition lock poisoned")
    }

    pub(super) fn exclusive(&mut self) -> &mut DefinitionState {
        self.state.get_mut().expect("table definition lock poisoned")
    }
}

pub(super) struct DefinitionVersions {
    state: RwLock<FixedVec<PendingTableDefSlot>>,
}

impl DefinitionVersions {
    pub(super) const fn control_bytes() -> usize { size_of::<Self>() }

    pub(super) fn new(budget: &mut Budget, capacity: usize) -> Result<Self, BudgetError> {
        budget.draw(Self::control_bytes(), "table definition version controls")?;
        Ok(Self { state: RwLock::new(FixedVec::new(budget, "pending_table_defs", capacity)?) })
    }

    pub(super) fn read(&self) -> RwLockReadGuard<'_, FixedVec<PendingTableDefSlot>> {
        self.state.read().expect("table definition version lock poisoned")
    }

    pub(super) fn exclusive(&mut self) -> &mut FixedVec<PendingTableDefSlot> {
        self.state.get_mut().expect("table definition version lock poisoned")
    }

    #[cfg(test)]
    pub(super) fn capacity(&self) -> usize { self.read().capacity() }
}

/// References borrow this issued guard, never an unowned pending slot.
pub struct TableDefinitionRead<'a> {
    state: RwLockReadGuard<'a, DefinitionState>,
    pending: Option<(RwLockReadGuard<'a, FixedVec<PendingTableDefSlot>>, usize)>,
}

impl Deref for TableDefinitionRead<'_> {
    type Target = TableDef;
    fn deref(&self) -> &TableDef {
        match &self.pending {
            Some((versions, slot)) => &versions[*slot].version.def,
            None => &self.state.committed,
        }
    }
}

impl<'a> TableDefinitionRead<'a> {
    pub(super) fn committed(definition: &'a TableDefinition) -> Self {
        Self { state: definition.read(), pending: None }
    }

    pub(super) fn visible(
        definition: &'a TableDefinition,
        versions: &'a DefinitionVersions,
        transaction: u32,
    ) -> Self {
        let state = definition.read();
        if !state.pending.is_some_and(|head| head.transaction == transaction) {
            return Self { state, pending: None };
        }
        // Pending-pool ownership precedes table ownership. Release the probe
        // before acquiring the pool and recheck the head under both guards.
        drop(state);
        let versions = versions.read();
        let state = definition.read();
        let pending = state.pending.filter(|head| head.transaction == transaction)
            .map(|head| (versions, head.tail as usize));
        Self { state, pending }
    }
}

pub(super) struct PendingDefinitionRead<'a> {
    versions: RwLockReadGuard<'a, FixedVec<PendingTableDefSlot>>,
    slot: usize,
}

impl Deref for PendingDefinitionRead<'_> {
    type Target = PendingTableDef;
    fn deref(&self) -> &PendingTableDef { &self.versions[self.slot].version }
}

impl<'a> PendingDefinitionRead<'a> {
    pub(super) fn current(definition: &TableDefinition, versions: &'a DefinitionVersions) -> Option<Self> {
        let versions = versions.read();
        let slot = definition.read().pending?.tail as usize;
        Some(Self { versions, slot })
    }
}
