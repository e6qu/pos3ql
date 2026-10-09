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

#[cfg(test)]
mod tests {
    use super::*;
    use super::super::{ColumnMeta, SqlName, MAX_COLUMNS};

    fn definition(epoch: usize) -> TableDef {
        let mut definition = TableDef::empty();
        definition.name = SqlName::parse("guarded").unwrap();
        definition.n_columns = epoch;
        definition.has_toast = epoch % 2 == 1;
        definition.columns[0] = ColumnMeta::EMPTY;
        definition
    }

    fn versions(capacity: usize) -> DefinitionVersions {
        let bytes = DefinitionVersions::control_bytes() + capacity * size_of::<PendingTableDefSlot>();
        let mut budget = Budget::new(bytes);
        let versions = DefinitionVersions::new(&mut budget, capacity).unwrap();
        assert_eq!(budget.remaining(), 0);
        versions
    }

    fn pending(epoch: usize, transaction: u32) -> PendingTableDefSlot {
        PendingTableDefSlot {
            used: true, previous: None, depth: 1,
            version: PendingTableDef {
                txid: transaction, def: definition(epoch),
                column_mapping: [None; MAX_COLUMNS], rewrites_rows: false,
            },
        }
    }

    #[test]
    fn live_definition_ownership_retains_committed_and_transaction_views() {
        let mut versions = versions(1);
        versions.exclusive().push(pending(2, 7)).unwrap();
        let mut table = TableDefinition::new(definition(1));
        table.exclusive().pending = Some(PendingDefinitionHead { tail: 0, transaction: 7 });
        crate::mem::guard::forbid_alloc(|| {
            let committed = TableDefinitionRead::visible(&table, &versions, 8);
            assert_eq!(committed.n_columns, 1);
            assert!(table.state.try_write().is_err());
            // A committed reader does not retain the global pending pool.
            assert!(versions.state.try_write().is_ok());
            drop(committed);
            let visible = TableDefinitionRead::visible(&table, &versions, 7);
            assert_eq!(visible.n_columns, 2);
            assert!(table.state.try_write().is_err());
            assert!(versions.state.try_write().is_err());
            drop(visible);
            let pending = PendingDefinitionRead::current(&table, &versions).unwrap();
            assert_eq!(pending.txid, 7);
            assert!(versions.state.try_write().is_err());
            drop(pending);
            assert!(versions.state.try_write().is_ok());
            assert!(table.state.try_write().is_ok());
        });
    }

    #[test]
    fn live_definition_ownership_rollback_retirement_and_slot_reuse_release_guards() {
        let mut versions = versions(1);
        versions.exclusive().push(pending(2, 7)).unwrap();
        let mut table = TableDefinition::new(definition(1));
        table.exclusive().pending = Some(PendingDefinitionHead { tail: 0, transaction: 7 });
        crate::mem::guard::forbid_alloc(|| {
            assert_eq!(TableDefinitionRead::visible(&table, &versions, 7).n_columns, 2);
            // Retire the old version and reuse the same bounded slot.
            table.exclusive().pending = None;
            versions.exclusive()[0].used = false;
            assert!(PendingDefinitionRead::current(&table, &versions).is_none());
            assert_eq!(TableDefinitionRead::visible(&table, &versions, 7).n_columns, 1);
            versions.exclusive()[0] = pending(3, 9);
            table.exclusive().pending = Some(PendingDefinitionHead { tail: 0, transaction: 9 });
            assert_eq!(TableDefinitionRead::visible(&table, &versions, 7).n_columns, 1);
            assert_eq!(TableDefinitionRead::visible(&table, &versions, 9).n_columns, 3);
            let promoted = *TableDefinitionRead::visible(&table, &versions, 9);
            table.exclusive().committed = promoted;
            table.exclusive().pending = None;
            versions.exclusive()[0].used = false;
            assert_eq!(TableDefinitionRead::visible(&table, &versions, 7).n_columns, 3);
            assert!(versions.state.try_write().is_ok());
            assert!(table.state.try_write().is_ok());
        });
    }

    #[test]
    fn live_definition_ownership_controls_and_pending_capacity_are_exactly_charged() {
        let _ = versions(2);
        let mut short = Budget::new(DefinitionVersions::control_bytes() - 1);
        let error = match DefinitionVersions::new(&mut short, 0) {
            Ok(_) => panic!("undersized definition controls accepted"),
            Err(error) => error,
        };
        assert_eq!(error.what, "table definition version controls");
        let mut versions = versions(1);
        crate::mem::guard::forbid_alloc(|| {
            versions.exclusive().push(pending(1, 7)).unwrap();
            assert!(versions.exclusive().push(pending(2, 8)).is_err());
            assert_eq!(versions.read()[0].version.txid, 7);
            assert_eq!(versions.read()[0].version.def.n_columns, 1);
        });
    }

    #[test]
    fn live_definition_ownership_concurrent_readers_observe_coherent_publication() {
        let mut versions = versions(1);
        versions.exclusive().push(pending(2, 7)).unwrap();
        let mut table = TableDefinition::new(definition(1));
        table.exclusive().pending = Some(PendingDefinitionHead { tail: 0, transaction: 7 });
        let start = std::sync::Barrier::new(4);
        std::thread::scope(|scope| {
            for _ in 0..3 {
                let table = &table;
                let versions = &versions;
                let start = &start;
                scope.spawn(move || {
                    start.wait();
                    crate::mem::guard::forbid_alloc(|| {
                        for _ in 0..500 {
                            let committed = TableDefinitionRead::visible(table, versions, 8);
                            assert!(committed.has_toast);
                            assert_eq!(committed.n_columns % 2, 1);
                            drop(committed);
                            let visible = TableDefinitionRead::visible(table, versions, 7);
                            if visible.state.pending.is_some_and(|head| head.transaction == 7) {
                                assert!(!visible.has_toast);
                                assert_eq!(visible.n_columns % 2, 0);
                                let (slots, slot) = visible.pending.as_ref().unwrap();
                                assert_eq!(slots[*slot].version.txid, 7);
                            } else {
                                assert!(visible.has_toast);
                                assert_eq!(visible.n_columns % 2, 1);
                                assert!(visible.pending.is_none());
                            }
                        }
                    });
                });
            }
            start.wait();
            crate::mem::guard::forbid_alloc(|| {
                for epoch in 1..=250 {
                    let mut versions = versions.state.write().unwrap();
                    let mut state = table.state.write().unwrap();
                    state.committed = definition(epoch * 2 + 1);
                    let transaction = if epoch % 2 == 0 { 7 } else { 9 };
                    versions[0] = pending(epoch * 2 + 2, transaction);
                    state.pending = (epoch % 3 != 0).then_some(PendingDefinitionHead { tail: 0, transaction });
                }
            });
        });
        assert_eq!(TableDefinitionRead::visible(&table, &versions, 7).n_columns, 502);
        assert_eq!(TableDefinitionRead::visible(&table, &versions, 8).n_columns, 501);
    }
}
