//! Backend-local invalidations survive transaction workspaces and PREPARE.

use core::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use super::{
    eval::{SqlError, sqlstate},
    txn::DdlUndo,
};
use crate::mem::{Budget, BudgetError, FixedVec, lease_pool::SharedSlice, mutex::Mutex};
use crate::sql_err;
use crate::storage::{RuntimeInputDependency, RuntimeInputScope, SelectedPlanTarget};

#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct CatalogPlanChange {
    pub global: bool,
    pub roles: bool,
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct PlanInvalidationEvent {
    pub scope: RuntimeInputScope,
    pub change: CatalogPlanChange,
    pub selected: Option<SelectedPlanTarget>,
}

impl PlanInvalidationEvent {
    pub(crate) fn capture(
        storage: &crate::storage::Storage,
        undo: &DdlUndo,
        transaction: u32,
    ) -> Result<Self, SqlError> {
        let mut change = undo.catalog_plan_change();
        change.global |= storage.operator_family_plan_change(undo, transaction);
        Ok(Self {
            scope: storage.runtime_input_scope(),
            change,
            selected: storage.selected_plan_target(undo, transaction)?,
        })
    }
}

pub(crate) struct LocalPlanInvalidations {
    global: AtomicU64,
    roles: AtomicU64,
    watches: Mutex<Watches>,
}

struct Watch {
    scope: RuntimeInputScope,
    dependencies: SharedSlice<RuntimeInputDependency>,
    own_routine: Option<i32>,
    generation: u64,
    epoch: u64,
}

struct Watches {
    entries: FixedVec<Option<Watch>>,
    next_generation: u64,
}

#[derive(Clone, Copy)]
struct WatchToken {
    slot: usize,
    generation: u64,
    epoch: u64,
}

pub(crate) struct LocalPlanStamp {
    owner: Arc<LocalPlanInvalidations>,
    global: u64,
    roles: Option<u64>,
    watch: Option<WatchToken>,
}

impl LocalPlanInvalidations {
    pub(crate) const fn budget_bytes(slots: usize) -> usize {
        (core::mem::size_of::<Self>() + 2 * core::mem::size_of::<usize>())
            .saturating_add(slots.saturating_mul(core::mem::size_of::<Option<Watch>>()))
    }

    pub(crate) fn new(slots: usize, budget: &mut Budget) -> Result<Arc<Self>, BudgetError> {
        budget.draw(
            core::mem::size_of::<Self>() + 2 * core::mem::size_of::<usize>(),
            "backend plan invalidation controls",
        )?;
        let mut entries = FixedVec::new(budget, "routine_input_slots invalidation watches", slots)?;
        for _ in 0..slots {
            entries
                .push(None)
                .expect("startup reserved every plan watch");
        }
        Ok(Arc::new(Self {
            global: AtomicU64::new(0),
            roles: AtomicU64::new(0),
            watches: Mutex::new(Watches {
                entries,
                next_generation: 1,
            }),
        }))
    }

    pub(crate) fn change(&self, change: CatalogPlanChange) {
        for (changed, generation) in [(change.global, &self.global), (change.roles, &self.roles)] {
            if changed {
                let _ = generation.try_update(Ordering::AcqRel, Ordering::Acquire, |value| {
                    Some(value.saturating_add(1))
                });
            }
        }
    }

    pub(crate) fn apply(&self, event: PlanInvalidationEvent) {
        self.change(event.change);
        if let Some(target) = event.selected {
            self.selected_changed(event.scope, target);
        }
    }

    fn generations(&self, depends_on_roles: bool) -> Result<(u64, Option<u64>), SqlError> {
        let global = self.global.load(Ordering::Acquire);
        let roles = depends_on_roles.then(|| self.roles.load(Ordering::Acquire));
        if global == u64::MAX || roles == Some(u64::MAX) {
            return Err(sql_err!(
                sqlstate::PROGRAM_LIMIT_EXCEEDED,
                "backend SQL plan invalidation identities exhausted"
            ));
        }
        Ok((global, roles))
    }

    pub(crate) fn stamp(
        self: &Arc<Self>,
        depends_on_roles: bool,
    ) -> Result<LocalPlanStamp, SqlError> {
        let (global, roles) = self.generations(depends_on_roles)?;
        Ok(LocalPlanStamp {
            owner: Arc::clone(self),
            global,
            roles,
            watch: None,
        })
    }

    pub(crate) fn watch(
        self: &Arc<Self>,
        depends_on_roles: bool,
        scope: RuntimeInputScope,
        dependencies: SharedSlice<RuntimeInputDependency>,
        own_routine: Option<i32>,
    ) -> Result<LocalPlanStamp, SqlError> {
        let mut stamp = self.stamp(depends_on_roles)?;
        let mut watches = self.watches.lock().expect("plan watch lock poisoned");
        let slot = watches
            .entries
            .iter()
            .position(Option::is_none)
            .ok_or_else(|| {
                sql_err!(
                    sqlstate::PROGRAM_LIMIT_EXCEEDED,
                    "routine_input_slots ({}) invalidation watches are full",
                    watches.entries.len()
                )
            })?;
        let generation = watches.next_generation;
        watches.next_generation = generation.checked_add(1).ok_or_else(|| {
            sql_err!(
                sqlstate::PROGRAM_LIMIT_EXCEEDED,
                "SQL plan invalidation watch identities exhausted"
            )
        })?;
        watches.entries[slot] = Some(Watch {
            scope,
            dependencies,
            own_routine,
            generation,
            epoch: 0,
        });
        stamp.watch = Some(WatchToken {
            slot,
            generation,
            epoch: 0,
        });
        Ok(stamp)
    }

    pub(crate) fn selected_changed(&self, scope: RuntimeInputScope, target: SelectedPlanTarget) {
        let mut watches = self.watches.lock().expect("plan watch lock poisoned");
        for watch in watches.entries.iter_mut().flatten() {
            if watch.scope == scope
                && (matches!(target, SelectedPlanTarget::Routine { oid } if watch.own_routine == Some(oid))
                    || watch
                        .dependencies
                        .iter()
                        .any(|dependency| dependency.matches_target(target)))
            {
                watch.epoch = watch.epoch.saturating_add(1);
            }
        }
    }

    fn release_watch(&self, token: WatchToken) {
        let mut watches = self.watches.lock().expect("plan watch lock poisoned");
        if let Some(entry) = watches.entries.get_mut(token.slot)
            && entry
                .as_ref()
                .is_some_and(|watch| watch.generation == token.generation)
        {
            *entry = None;
        }
    }
}

impl LocalPlanStamp {
    pub(crate) fn finish_environment(&mut self) -> Result<(), SqlError> {
        let (global, roles) = self.owner.generations(self.roles.is_some())?;
        self.global = global;
        self.roles = roles;
        Ok(())
    }

    pub(crate) fn matches(&self) -> Result<bool, SqlError> {
        let (global, roles) = self.owner.generations(self.roles.is_some())?;
        if global != self.global || roles != self.roles {
            return Ok(false);
        }
        self.selected_matches()
    }

    pub(crate) fn selected_matches(&self) -> Result<bool, SqlError> {
        if let Some(token) = self.watch {
            let watches = self.owner.watches.lock().expect("plan watch lock poisoned");
            let watch = watches
                .entries
                .get(token.slot)
                .and_then(Option::as_ref)
                .filter(|watch| watch.generation == token.generation)
                .ok_or_else(|| {
                    sql_err!(
                        sqlstate::SERIALIZATION_FAILURE,
                        "SQL plan invalidation watch was replaced"
                    )
                })?;
            if watch.epoch == u64::MAX {
                return Err(sql_err!(
                    sqlstate::PROGRAM_LIMIT_EXCEEDED,
                    "SQL plan invalidation event identities exhausted"
                ));
            }
            if watch.epoch != token.epoch {
                return Ok(false);
            }
        }
        Ok(true)
    }
}

impl Drop for LocalPlanStamp {
    fn drop(&mut self) {
        if let Some(token) = self.watch {
            self.owner.release_watch(token);
        }
    }
}

impl DdlUndo {
    pub(crate) fn catalog_plan_change(&self) -> CatalogPlanChange {
        use crate::storage::{AccessClass, foreign::ForeignObjectClass};
        let global = match self {
            Self::TemporaryNamespaceCreated(_)
            | Self::SchemaCreated(_)
            | Self::SchemaDropped(_)
            | Self::SchemaRenamed(_)
            | Self::OperatorCreated(_)
            | Self::OperatorAltered { .. }
            | Self::OperatorDropped(_)
            | Self::ForeignDataWrapperCreated(_)
            | Self::ForeignDataWrapperAltered { .. }
            | Self::ForeignDataWrapperDropped(_)
            | Self::ForeignServerCreated(_)
            | Self::ForeignServerAltered { .. }
            | Self::ForeignServerDropped(_) => true,
            Self::ObjectOwnerChanged { object, .. } => matches!(
                object.class,
                AccessClass::Schema | AccessClass::ForeignDataWrapper | AccessClass::ForeignServer
            ),
            Self::ObjectAclChanged { prior, .. } => matches!(
                prior.object().class,
                AccessClass::Schema | AccessClass::ForeignDataWrapper | AccessClass::ForeignServer
            ),
            Self::ForeignOwnerChanged { class, .. } => matches!(
                class,
                ForeignObjectClass::Wrapper | ForeignObjectClass::Server
            ),
            _ => false,
        };
        let roles = match self {
            Self::RoleChanged { .. }
            | Self::RoleMembershipChanged { .. }
            | Self::DatabaseCreated(_)
            | Self::DatabaseAltered { .. }
            | Self::DatabaseDropped(_) => true,
            Self::ObjectOwnerChanged { object, .. } => object.class == AccessClass::Database,
            Self::ObjectAclChanged { prior, .. } => prior.object().class == AccessClass::Database,
            _ => false,
        };
        CatalogPlanChange { global, roles }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backend_invalidations_are_role_sensitive_and_never_wrap_without_allocation() {
        let mut budget = Budget::new(LocalPlanInvalidations::budget_bytes(0));
        let owner = LocalPlanInvalidations::new(0, &mut budget).unwrap();
        crate::mem::guard::forbid_alloc(|| {
            let ordinary = owner.stamp(false).unwrap();
            let secure = owner.stamp(true).unwrap();
            owner.change(CatalogPlanChange {
                global: false,
                roles: true,
            });
            assert!(ordinary.matches().unwrap());
            assert!(!secure.matches().unwrap());
            owner.change(CatalogPlanChange {
                global: true,
                roles: false,
            });
            assert!(!ordinary.matches().unwrap());
            owner.global.store(u64::MAX, Ordering::Release);
            owner.change(CatalogPlanChange {
                global: true,
                roles: false,
            });
            assert_eq!(
                owner.stamp(false).err().unwrap().sqlstate,
                sqlstate::PROGRAM_LIMIT_EXCEEDED
            );
            assert_eq!(
                ordinary.matches().err().unwrap().sqlstate,
                sqlstate::PROGRAM_LIMIT_EXCEEDED
            );
        });
    }

    #[test]
    fn selected_watches_are_bounded_scoped_and_generation_safe_without_allocation() {
        let mut budget = Budget::new(LocalPlanInvalidations::budget_bytes(2));
        let owner = LocalPlanInvalidations::new(2, &mut budget).unwrap();
        let scope = RuntimeInputScope::test_scope(crate::storage::DatabaseOid::POSTGRES);
        let unrelated = RuntimeInputScope::test_scope(crate::storage::DatabaseOid::POSTGRES);
        crate::mem::guard::forbid_alloc(|| {
            let mut first = owner
                .watch(false, scope, SharedSlice::EMPTY, Some(100_001))
                .unwrap();
            let mut other = owner
                .watch(false, scope, SharedSlice::EMPTY, Some(100_002))
                .unwrap();
            let error = owner
                .watch(false, scope, SharedSlice::EMPTY, None)
                .err()
                .unwrap();
            assert_eq!(error.sqlstate, sqlstate::PROGRAM_LIMIT_EXCEEDED);
            assert!(error.message.as_str().contains("routine_input_slots"));
            owner.selected_changed(unrelated, SelectedPlanTarget::Routine { oid: 100_001 });
            assert!(first.matches().unwrap());
            owner.selected_changed(scope, SelectedPlanTarget::Routine { oid: 100_001 });
            assert!(!first.matches().unwrap());
            assert!(other.matches().unwrap());
            owner.change(CatalogPlanChange {
                global: true,
                roles: false,
            });
            assert!(!other.matches().unwrap());
            assert!(other.selected_matches().unwrap());
            assert!(!first.selected_matches().unwrap());
            other.finish_environment().unwrap();
            assert!(other.matches().unwrap());
            first.finish_environment().unwrap();
            assert!(
                !first.matches().unwrap(),
                "finishing settings must preserve selected-definition invalidation"
            );
            let stale = first.watch.unwrap();
            drop(first);
            let replacement = owner
                .watch(false, scope, SharedSlice::EMPTY, Some(100_001))
                .unwrap();
            assert_ne!(replacement.watch.unwrap().generation, stale.generation);
            owner.release_watch(stale);
            assert!(replacement.matches().unwrap());
            drop((replacement, other));
            assert!(
                owner
                    .watches
                    .lock()
                    .unwrap()
                    .entries
                    .iter()
                    .all(Option::is_none)
            );
        });
    }

    #[test]
    fn transaction_workspaces_share_backend_lineage_and_prepared_work_detaches() {
        let mut budget = Budget::new(
            2 * LocalPlanInvalidations::budget_bytes(0)
                + 3 * super::super::txn::TxnState::budget_bytes(8),
        );
        let first = LocalPlanInvalidations::new(0, &mut budget).unwrap();
        let other = LocalPlanInvalidations::new(0, &mut budget).unwrap();
        let mut transaction = super::super::txn::TxnState::new(&mut budget, 8).unwrap();
        let mut replacement = super::super::txn::TxnState::new(&mut budget, 8).unwrap();
        let mut detached = super::super::txn::TxnState::new(&mut budget, 8).unwrap();
        crate::mem::guard::forbid_alloc(|| {
            transaction.bind_plan_invalidations(&first).unwrap();
            let old = transaction.plan_invalidation_stamp(false).unwrap();
            first.change(DdlUndo::SchemaCreated(3).catalog_plan_change());
            assert!(!old.matches().unwrap());
            let pending = transaction.plan_invalidation_stamp(false).unwrap();
            transaction.rewind_ddl(0);
            first.change(DdlUndo::SchemaCreated(3).catalog_plan_change());
            assert!(!pending.matches().unwrap());
            let reused = first.stamp(false).unwrap();
            replacement.bind_plan_invalidations(&first).unwrap();
            replacement.clear();
            assert!(reused.matches().unwrap());
            first.change(DdlUndo::SchemaCreated(3).catalog_plan_change());
            transaction.mode = super::super::txn::TxnMode::Explicit;
            let same_backend = Arc::clone(transaction.plan_invalidation_owner().unwrap());
            transaction.bind_plan_invalidations(&same_backend).unwrap();
            assert_eq!(
                transaction
                    .bind_plan_invalidations(&other)
                    .unwrap_err()
                    .sqlstate,
                sqlstate::OBJECT_IN_USE
            );
            let before_prepare = first.stamp(false).unwrap();
            transaction.invalidate_prepared_plan_inputs();
            transaction.swap_prepared_state(&mut detached);
            assert!(before_prepare.matches().unwrap());
            transaction.bind_plan_invalidations(&first).unwrap();
            assert!(detached.is_active());
            assert_eq!(
                detached
                    .plan_invalidation_stamp(false)
                    .err()
                    .unwrap()
                    .sqlstate,
                sqlstate::INTERNAL_ERROR
            );
            let after_prepare = first.stamp(false).unwrap();
            detached.clear();
            assert!(
                after_prepare.matches().unwrap(),
                "resolving prepared work cannot invalidate its former backend locally"
            );
            replacement.bind_plan_invalidations(&other).unwrap();
            replacement.mode = super::super::txn::TxnMode::Explicit;
            replacement.swap_prepared_state(&mut detached);
            replacement.bind_plan_invalidations(&other).unwrap();
            assert!(detached.plan_invalidation_owner().is_none());
            replacement.swap_prepared_state(&mut detached);
            replacement.bind_plan_invalidations(&other).unwrap();
            assert_eq!(
                replacement
                    .bind_plan_invalidations(&first)
                    .unwrap_err()
                    .sqlstate,
                sqlstate::OBJECT_IN_USE
            );
            let resolver = other.stamp(false).unwrap();
            let resolved: Result<(), ()> = replacement.with_detached_plan_owner(|transaction| {
                assert!(transaction.plan_invalidation_owner().is_none());
                transaction.clear();
                Err(())
            });
            assert!(resolved.is_err());
            assert!(resolver.matches().unwrap());
            replacement.bind_plan_invalidations(&other).unwrap();
            replacement.mode = super::super::txn::TxnMode::Explicit;
            assert_eq!(
                replacement
                    .bind_plan_invalidations(&first)
                    .unwrap_err()
                    .sqlstate,
                sqlstate::OBJECT_IN_USE
            );
        });
        let unwind = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            replacement.with_detached_plan_owner(|transaction| {
                assert!(transaction.plan_invalidation_owner().is_none());
                panic!("prepared resolution unwinds");
            });
        }));
        assert!(unwind.is_err());
        replacement.bind_plan_invalidations(&other).unwrap();
        assert_eq!(
            replacement
                .bind_plan_invalidations(&first)
                .unwrap_err()
                .sqlstate,
            sqlstate::OBJECT_IN_USE
        );
    }
}
