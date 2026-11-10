//! Routine settings, identity and clocks leave the worker while a call waits.

use super::*;
use crate::mem::lease_pool::SharedSlice;

pub(in crate::sql) struct CallEnvironment {
    config: Option<crate::sql::guc::RoutineConfigScope>,
    security: Option<crate::sql::eval::funcs::system::CurrentUserScope>,
    prior_clock: Option<crate::sql::datetime::StatementClock>,
    prior_namespace: Option<(
        crate::storage::PathContext,
        crate::sql::eval::funcs::system::SessionSchemas,
    )>,
    _deadline: Option<crate::sql::query::DeadlineScope>,
}

impl CallEnvironment {
    pub(in crate::sql) fn new(
        config: Option<crate::sql::guc::RoutineConfigScope>,
        security: Option<crate::sql::eval::funcs::system::CurrentUserScope>,
    ) -> Self {
        Self {
            config,
            security,
            prior_clock: None,
            prior_namespace: None,
            _deadline: None,
        }
    }
}

impl Drop for CallEnvironment {
    fn drop(&mut self) {
        if let Some((path, schemas)) = self.prior_namespace.take() {
            crate::storage::replace_execution_path(path);
            crate::sql::eval::funcs::system::set_session_schemas(schemas);
        }
        if let Some(clock) = self.prior_clock.take() {
            crate::sql::datetime::restore_statement_clock(clock);
        }
    }
}

pub(super) struct SavedCallEnvironment {
    security: Option<crate::storage::SessionIdentity>,
    config: Option<SharedSlice<crate::sql::guc::RoutineConfigImage>>,
    path: crate::storage::PathContext,
    schemas: crate::sql::eval::funcs::system::SessionSchemas,
    clock: crate::sql::datetime::StatementClock,
    deadline: i64,
}

impl SavedCallEnvironment {
    pub(super) fn complete(&mut self) {
        self.security = None;
        self.config = None;
    }

    pub(super) fn capture(
        environment: &CallEnvironment,
        controls: &std::sync::Arc<crate::mem::lease_pool::LeasePool>,
    ) -> Result<Self, SqlError> {
        let config = environment
            .config
            .as_ref()
            .map(|scope| {
                controls
                    .copy_slice(&[scope.save()?])
                    .map_err(control_flow::execution_full)
            })
            .transpose()?;
        Ok(Self {
            security: environment
                .security
                .as_ref()
                .map(|_| crate::sql::eval::funcs::system::current_identity()),
            config,
            path: crate::storage::execution_path(),
            schemas: crate::sql::eval::funcs::system::session_schemas(),
            clock: crate::sql::datetime::stored_statement_clock(),
            deadline: crate::sql::query::statement_deadline(),
        })
    }

    pub(super) fn enter(&self, txn: &TxnState) -> Result<CallEnvironment, SqlError> {
        let security = self
            .security
            .map(crate::sql::eval::funcs::system::enter_current_user);
        let config = self
            .config
            .as_ref()
            .map(|image| image[0].enter())
            .transpose()?;
        let prior_namespace = (
            crate::storage::replace_execution_path(self.path),
            crate::sql::eval::funcs::system::session_schemas(),
        );
        crate::sql::eval::funcs::system::set_session_schemas(self.schemas);
        let prior_clock = crate::sql::datetime::stored_statement_clock();
        let deadline = crate::sql::query::enter_deadline_scope();
        crate::sql::datetime::restore_statement_clock(self.clock);
        txn.restore_statement_deadline(self.deadline);
        Ok(CallEnvironment {
            config,
            security,
            prior_clock: Some(prior_clock),
            prior_namespace: Some(prior_namespace),
            _deadline: Some(deadline),
        })
    }
}
