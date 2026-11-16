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

/// PostgreSQL 17+ evaluates the expressions of `CREATE INDEX`, `REINDEX`,
/// `CLUSTER`, `VACUUM`, `ANALYZE` and materialized view population under
/// `search_path = pg_catalog, pg_temp` (`RestrictSearchPath`), after the
/// command's own names are resolved. Routine bodies then find no unqualified
/// user object, `current_setting('search_path')` reports the restricted
/// value, and a type outside the path is named qualified in a diagnostic
/// (`value for domain public.posint violates ...`). Dropping the scope
/// restores the session's setting, path and schemas.
pub(crate) struct MaintenancePath {
    _config: crate::sql::guc::RoutineConfigScope,
    prior_path: crate::storage::PathContext,
    prior_schemas: crate::sql::eval::funcs::system::SessionSchemas,
    prior_session: Option<crate::storage::PathContext>,
}

std::thread_local! {
    /// The session's own path while a maintenance operation restricts it.
    static SESSION_PATH: core::cell::Cell<Option<crate::storage::PathContext>> =
        const { core::cell::Cell::new(None) };
}

/// Runs `run` under the session's own search path when a maintenance
/// operation restricted it. PostgreSQL names a PL/pgSQL function in its
/// error context by the signature it formatted when the session compiled
/// the function, which its validation at `CREATE FUNCTION` or an earlier
/// call did under the session's path.
pub(crate) fn with_session_path<R>(run: impl FnOnce() -> R) -> R {
    match SESSION_PATH.with(core::cell::Cell::get) {
        Some(path) => {
            let restricted = crate::storage::replace_execution_path(path);
            let result = run();
            crate::storage::replace_execution_path(restricted);
            result
        }
        None => run(),
    }
}

impl MaintenancePath {
    pub(crate) fn enter(storage: &Storage, txid: u32) -> Result<Self, SqlError> {
        let config = crate::sql::guc::enter_active_restricted_search_path()?;
        let user = crate::sql::eval::funcs::system::current_user_owned();
        let path =
            storage.compute_path(crate::sql::guc::RESTRICTED_SEARCH_PATH, user.as_str(), txid);
        let prior_schemas = crate::sql::eval::funcs::system::session_schemas();
        crate::sql::eval::funcs::system::set_session_schemas(
            crate::sql::eval::funcs::system::path_session_schemas(storage, &path, txid),
        );
        let prior_path = crate::storage::replace_execution_path(path);
        let prior_session =
            SESSION_PATH.with(|session| session.replace(session.get().or(Some(prior_path))));
        Ok(Self {
            _config: config,
            prior_path,
            prior_schemas,
            prior_session,
        })
    }
}

impl Drop for MaintenancePath {
    fn drop(&mut self) {
        // A routine call that left the operation for the engine runs its
        // body under the same restricted path.
        crate::sql::query::with_active_routine_invocations(|context| {
            if let Some((invocations, _)) = context {
                invocations.mark_pending_restricted();
            }
        });
        crate::storage::replace_execution_path(self.prior_path);
        crate::sql::eval::funcs::system::set_session_schemas(self.prior_schemas);
        SESSION_PATH.with(|session| session.set(self.prior_session));
    }
}
