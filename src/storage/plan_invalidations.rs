//! Saved SQL plans follow catalog changes, independently of durable data writes.

use crate::mem::{Budget, BudgetError, FixedVec, mutex::Mutex};
use crate::sql::eval::{SqlError, sqlstate};
use crate::sql::plan_invalidations::LocalPlanInvalidations;
use crate::sql_err;
use std::sync::{Arc, Weak};

use super::{DatabaseOid, Storage};

impl Storage {
    pub(crate) fn register_plan_backend(
        &self,
        owner: &Arc<LocalPlanInvalidations>,
    ) -> Result<(), SqlError> {
        self.plan_invalidations.register(owner)
    }

    pub(crate) fn publish_plan_invalidations(
        &self,
        events: impl IntoIterator<Item = crate::sql::plan_invalidations::PlanInvalidationEvent>,
    ) {
        for event in events {
            let change = event.change;
            if change.global {
                self.plan_invalidations
                    .change_database(event.scope.database());
            }
            if change.roles {
                self.plan_invalidations.change_roles();
            }
            if let Some(target) = event.selected {
                self.plan_invalidations
                    .selected_changed(event.scope, target);
            }
        }
    }
}

#[derive(Clone, Copy)]
struct DatabaseEpoch {
    database: Option<DatabaseOid>,
    generation: u64,
}

struct State {
    databases: FixedVec<DatabaseEpoch>,
    roles: u64,
    exhausted: bool,
    backends: FixedVec<Option<Weak<LocalPlanInvalidations>>>,
}

pub(super) struct PlanInvalidations {
    state: Mutex<State>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct PlanInvalidationStamp {
    database: DatabaseOid,
    generation: u64,
    roles: Option<u64>,
}

impl PlanInvalidations {
    pub(super) fn backend_capacity(config: &crate::config::Config) -> usize {
        (config.max_connections as usize)
            .saturating_add(config.max_subscriptions)
            .saturating_add(config.query_workspace_slots)
            .saturating_add(config.commit_queue_slots)
    }

    pub(super) fn budget_bytes(databases: usize, backends: usize) -> usize {
        core::mem::size_of::<Self>()
            .saturating_add(databases.saturating_mul(core::mem::size_of::<DatabaseEpoch>()))
            .saturating_add(
                backends
                    .saturating_mul(core::mem::size_of::<Option<Weak<LocalPlanInvalidations>>>()),
            )
    }

    pub(super) fn new(
        databases: usize,
        backends: usize,
        budget: &mut Budget,
    ) -> Result<Self, BudgetError> {
        budget.draw(core::mem::size_of::<Self>(), "plan invalidation controls")?;
        let mut entries = FixedVec::new(budget, "database plan invalidations", databases)?;
        for _ in 0..databases {
            entries
                .push(DatabaseEpoch {
                    database: None,
                    generation: 0,
                })
                .expect("startup reserved every database invalidation control");
        }
        let mut owners = FixedVec::new(budget, "SQL plan backend registry", backends)?;
        for _ in 0..backends {
            owners
                .push(None)
                .expect("startup reserved every plan backend");
        }
        Ok(Self {
            state: Mutex::new(State {
                databases: entries,
                roles: 0,
                exhausted: false,
                backends: owners,
            }),
        })
    }

    fn register(&self, owner: &Arc<LocalPlanInvalidations>) -> Result<(), SqlError> {
        let mut state = self.state.lock().expect("plan invalidation lock poisoned");
        if state
            .backends
            .iter()
            .flatten()
            .any(|backend| backend.as_ptr() == Arc::as_ptr(owner))
        {
            return Ok(());
        }
        let slot = state.backends.iter().position(|backend| backend.as_ref().is_none_or(|backend| backend.strong_count() == 0))
            .ok_or_else(|| sql_err!(sqlstate::PROGRAM_LIMIT_EXCEEDED,
                "SQL plan backend registry (max_connections + max_subscriptions + query_workspace_slots + commit_queue_slots = {}) is full", state.backends.len()))?;
        state.backends[slot] = Some(Arc::downgrade(owner));
        Ok(())
    }

    fn selected_changed(&self, scope: super::RuntimeInputScope, target: super::SelectedPlanTarget) {
        let state = self.state.lock().expect("plan invalidation lock poisoned");
        for backend in state.backends.iter().flatten().filter_map(Weak::upgrade) {
            backend.selected_changed(scope, target);
        }
    }

    pub(super) fn change_database(&self, database: DatabaseOid) {
        let mut state = self.state.lock().expect("plan invalidation lock poisoned");
        let slot = state
            .databases
            .iter()
            .position(|entry| entry.database == Some(database))
            .or_else(|| {
                state
                    .databases
                    .iter()
                    .position(|entry| entry.database.is_none())
            });
        if let Some(slot) = slot {
            let entry = &mut state.databases[slot];
            entry.database = Some(database);
            entry.generation = entry.generation.saturating_add(1);
        } else {
            // Never turn exhaustion into an apparently unchanged generation.
            state.exhausted = true;
        }
    }

    pub(super) fn change_roles(&self) {
        let mut state = self.state.lock().expect("plan invalidation lock poisoned");
        state.roles = state.roles.saturating_add(1);
    }

    pub(super) fn retire_database(&self, database: DatabaseOid) {
        let mut state = self.state.lock().expect("plan invalidation lock poisoned");
        if let Some(entry) = state
            .databases
            .iter_mut()
            .find(|entry| entry.database == Some(database))
        {
            entry.database = None;
        }
    }

    pub(super) fn stamp(
        &self,
        database: DatabaseOid,
        depends_on_roles: bool,
    ) -> Result<PlanInvalidationStamp, SqlError> {
        let state = self.state.lock().expect("plan invalidation lock poisoned");
        let generation = state
            .databases
            .iter()
            .find(|entry| entry.database == Some(database))
            .map_or(0, |entry| entry.generation);
        if state.exhausted
            || generation == u64::MAX
            || (depends_on_roles && state.roles == u64::MAX)
        {
            return Err(sql_err!(
                sqlstate::PROGRAM_LIMIT_EXCEEDED,
                "SQL plan invalidation identities exhausted"
            ));
        }
        Ok(PlanInvalidationStamp {
            database,
            generation,
            roles: depends_on_roles.then_some(state.roles),
        })
    }

    pub(super) fn matches(&self, stamp: PlanInvalidationStamp) -> Result<bool, SqlError> {
        Ok(self.stamp(stamp.database, stamp.roles.is_some())? == stamp)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn catalog_epochs_separate_databases_and_role_dependent_plans_without_allocation() {
        let mut budget = Budget::new(PlanInvalidations::budget_bytes(3, 0));
        let epochs = PlanInvalidations::new(3, 0, &mut budget).unwrap();
        crate::mem::guard::forbid_alloc(|| {
            let database = DatabaseOid::POSTGRES;
            let unrelated = DatabaseOid::TEMPLATE1;
            let plain = epochs.stamp(database, false).unwrap();
            let secure = epochs.stamp(database, true).unwrap();
            epochs.change_database(unrelated);
            assert!(epochs.matches(plain).unwrap());
            assert!(epochs.matches(secure).unwrap());
            epochs.change_roles();
            assert!(epochs.matches(plain).unwrap());
            assert!(!epochs.matches(secure).unwrap());
            epochs.change_database(database);
            assert!(!epochs.matches(plain).unwrap());
            epochs.retire_database(unrelated);
            epochs.change_database(unrelated);
        });
    }

    #[test]
    fn committed_selected_events_reach_registered_owners_without_runtime_allocation() {
        let mut budget = Budget::new(
            PlanInvalidations::budget_bytes(3, 2) + 3 * LocalPlanInvalidations::budget_bytes(1),
        );
        let epochs = PlanInvalidations::new(3, 2, &mut budget).unwrap();
        let first = LocalPlanInvalidations::new(1, &mut budget).unwrap();
        let second = LocalPlanInvalidations::new(1, &mut budget).unwrap();
        let next = LocalPlanInvalidations::new(1, &mut budget).unwrap();
        let scope = super::super::RuntimeInputScope::test_scope(DatabaseOid::POSTGRES);
        crate::mem::guard::forbid_alloc(|| {
            let first_watch = first
                .watch(
                    false,
                    scope,
                    crate::mem::lease_pool::SharedSlice::EMPTY,
                    Some(100_001),
                )
                .unwrap();
            let second_watch = second
                .watch(
                    false,
                    scope,
                    crate::mem::lease_pool::SharedSlice::EMPTY,
                    Some(100_002),
                )
                .unwrap();
            epochs.register(&first).unwrap();
            epochs.register(&first).unwrap();
            epochs.register(&second).unwrap();
            assert_eq!(
                epochs.register(&next).unwrap_err().sqlstate,
                sqlstate::PROGRAM_LIMIT_EXCEEDED
            );
            epochs.selected_changed(
                scope,
                super::super::SelectedPlanTarget::Routine { oid: 100_001 },
            );
            assert!(!first_watch.matches().unwrap());
            assert!(second_watch.matches().unwrap());
            drop((first_watch, first));
            epochs.register(&next).unwrap();
        });
    }
}
