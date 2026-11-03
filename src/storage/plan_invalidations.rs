//! Saved SQL plans follow catalog changes, independently of durable data writes.

use crate::mem::{Budget, BudgetError, FixedVec, mutex::Mutex};
use crate::sql::eval::{SqlError, sqlstate};
use crate::sql_err;

use super::{DatabaseOid, Storage};

impl Storage {
    pub(crate) fn publish_plan_invalidations(&self, changes: &[crate::sql::txn::DdlUndo]) {
        for change in changes {
            let change = change.catalog_plan_change();
            if change.global {
                self.plan_invalidations
                    .change_database(super::current_database());
            }
            if change.roles {
                self.plan_invalidations.change_roles();
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
    pub(super) fn budget_bytes(databases: usize) -> usize {
        core::mem::size_of::<Self>()
            .saturating_add(databases.saturating_mul(core::mem::size_of::<DatabaseEpoch>()))
    }

    pub(super) fn new(databases: usize, budget: &mut Budget) -> Result<Self, BudgetError> {
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
        Ok(Self {
            state: Mutex::new(State {
                databases: entries,
                roles: 0,
                exhausted: false,
            }),
        })
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
        let mut budget = Budget::new(PlanInvalidations::budget_bytes(3));
        let epochs = PlanInvalidations::new(3, &mut budget).unwrap();
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
}
