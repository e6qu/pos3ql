//! Backend-local invalidations survive transaction workspaces and PREPARE.

use core::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use super::{
    eval::{SqlError, sqlstate},
    txn::DdlUndo,
};
use crate::mem::{Budget, BudgetError};
use crate::sql_err;

#[derive(Clone, Copy, Default)]
pub(crate) struct CatalogPlanChange {
    pub global: bool,
    pub roles: bool,
}

pub(crate) struct LocalPlanInvalidations {
    global: AtomicU64,
    roles: AtomicU64,
}

#[derive(Clone)]
pub(crate) struct LocalPlanStamp {
    owner: Arc<LocalPlanInvalidations>,
    global: u64,
    roles: Option<u64>,
}

impl LocalPlanInvalidations {
    pub(crate) const fn budget_bytes() -> usize {
        core::mem::size_of::<Self>() + 2 * core::mem::size_of::<usize>()
    }

    pub(crate) fn new(budget: &mut Budget) -> Result<Arc<Self>, BudgetError> {
        budget.draw(Self::budget_bytes(), "backend plan invalidations")?;
        Ok(Arc::new(Self {
            global: AtomicU64::new(0),
            roles: AtomicU64::new(0),
        }))
    }

    pub(crate) fn change(&self, change: CatalogPlanChange) {
        for (changed, generation) in [(change.global, &self.global), (change.roles, &self.roles)] {
            if changed {
                let _ = generation.fetch_update(Ordering::AcqRel, Ordering::Acquire, |value| {
                    Some(value.saturating_add(1))
                });
            }
        }
    }

    pub(crate) fn stamp(
        self: &Arc<Self>,
        depends_on_roles: bool,
    ) -> Result<LocalPlanStamp, SqlError> {
        let global = self.global.load(Ordering::Acquire);
        let roles = depends_on_roles.then(|| self.roles.load(Ordering::Acquire));
        if global == u64::MAX || roles == Some(u64::MAX) {
            return Err(sql_err!(
                sqlstate::PROGRAM_LIMIT_EXCEEDED,
                "backend SQL plan invalidation identities exhausted"
            ));
        }
        Ok(LocalPlanStamp {
            owner: Arc::clone(self),
            global,
            roles,
        })
    }
}

impl LocalPlanStamp {
    pub(crate) fn matches(&self) -> Result<bool, SqlError> {
        let current = self.owner.stamp(self.roles.is_some())?;
        Ok(current.global == self.global && current.roles == self.roles)
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
        let mut budget = Budget::new(LocalPlanInvalidations::budget_bytes());
        let owner = LocalPlanInvalidations::new(&mut budget).unwrap();
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
    fn transaction_workspaces_share_backend_lineage_and_prepared_work_detaches() {
        let mut budget = Budget::new(
            2 * LocalPlanInvalidations::budget_bytes()
                + 2 * super::super::txn::TxnState::budget_bytes(8),
        );
        let first = LocalPlanInvalidations::new(&mut budget).unwrap();
        let other = LocalPlanInvalidations::new(&mut budget).unwrap();
        let mut transaction = super::super::txn::TxnState::new(&mut budget, 8).unwrap();
        let mut replacement = super::super::txn::TxnState::new(&mut budget, 8).unwrap();
        crate::mem::guard::forbid_alloc(|| {
            transaction.bind_plan_invalidations(&first).unwrap();
            let old = transaction.plan_invalidation_stamp(false).unwrap();
            transaction.record_ddl(DdlUndo::SchemaCreated(3)).unwrap();
            assert!(!old.matches().unwrap());
            let pending = transaction.plan_invalidation_stamp(false).unwrap();
            transaction.rewind_ddl(0);
            assert!(!pending.matches().unwrap());
            let reused = first.stamp(false).unwrap();
            replacement.bind_plan_invalidations(&first).unwrap();
            replacement.clear();
            assert!(reused.matches().unwrap());
            transaction.record_ddl(DdlUndo::SchemaCreated(3)).unwrap();
            transaction.mode = super::super::txn::TxnMode::Explicit;
            assert_eq!(
                transaction
                    .bind_plan_invalidations(&other)
                    .unwrap_err()
                    .sqlstate,
                sqlstate::OBJECT_IN_USE
            );
            let before_prepare = first.stamp(false).unwrap();
            transaction.detach_prepared_plan_invalidations();
            assert!(!before_prepare.matches().unwrap());
            let after_prepare = first.stamp(false).unwrap();
            transaction.clear();
            assert!(
                after_prepare.matches().unwrap(),
                "resolving prepared work cannot invalidate its former backend locally"
            );
        });
    }
}
