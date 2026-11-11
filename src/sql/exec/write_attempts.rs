//! Routine calls inside a procedural data change.
//!
//! A call whose body needs the engine ends the write attempt before the call
//! runs. Until the write's first row change, the attempt rolls back, the call
//! runs and records its result in the statement's invocation log, and the
//! write runs again, replaying that result instead of calling again.

use super::*;
use crate::sql::{query, sequence};

/// Transaction-visible state an attempt can change. A changed attempt has
/// started its row changes, so rolling it back would hide them from the
/// calls that follow.
pub(super) struct WriteAttemptEffects<'a> {
    transaction: crate::sql::txn::TransactionEffectMark,
    descriptors: &'a [crate::sql::txn::LargeObjectDescriptor],
    advisory: crate::sql::lock::AdvisoryStatementMark,
    settings: crate::sql::guc::StatementSettings,
}

impl<'a> WriteAttemptEffects<'a> {
    pub(super) fn capture(
        context: &TriggerExecContext<'_, 'a, '_>,
    ) -> Result<Option<Self>, SqlError> {
        let PlpgsqlExecHost::Routine { guc, .. } = &context.host else {
            return Ok(None);
        };
        Ok(Some(Self {
            transaction: context.txn.transaction_effect_mark(),
            descriptors: context
                .txn
                .statement_large_object_descriptors(context.arena)?,
            advisory: context.storage().advisory_statement_mark(),
            settings: guc.statement_settings(),
        }))
    }

    pub(super) fn changed(&self, context: &TriggerExecContext<'_, '_, '_>) -> bool {
        context.txn.transaction_effect_mark() != self.transaction
            || !context
                .txn
                .matches_statement_large_object_descriptors(self.descriptors)
            || context.storage().advisory_statement_mark() != self.advisory
            || match &context.host {
                PlpgsqlExecHost::Routine { guc, .. } => {
                    !guc.matches_statement_settings(&self.settings)
                }
                PlpgsqlExecHost::Atomic { .. } => true,
            }
    }
}

pub(super) fn retained_progress_required() -> SqlError {
    sql_err!(
        sqlstate::FEATURE_NOT_SUPPORTED,
        "SQL row expression requires retained write progress"
    )
}

fn host_changed() -> SqlError {
    sql_err!(
        sqlstate::INTERNAL_ERROR,
        "procedural write lost its statement executor"
    )
}

fn response_full(_: WireFull) -> SqlError {
    sql_err!(
        sqlstate::PROGRAM_LIMIT_EXCEEDED,
        "procedural write response exceeds the send buffer"
    )
}

/// Runs a PL/pgSQL `INSERT`, `UPDATE` or `DELETE`, static or dynamic, whose
/// expressions can call routines. A `retained` frame keeps the invocation log
/// in `status.write_calls` across a wait.
pub(super) fn execute_procedural_write<'a, T>(
    context: &mut TriggerExecContext<'_, 'a, '_>,
    status: &mut TriggerExecutionStatus<'a>,
    retained: bool,
    attempt: &mut dyn FnMut(
        &mut TriggerExecContext<'_, 'a, '_>,
        &mut TriggerExecutionStatus<'a>,
    ) -> Result<T, SqlError>,
) -> Result<T, SqlError> {
    if matches!(context.host, PlpgsqlExecHost::Atomic { .. }) {
        // Without the engine, a routine call reports its own limitation.
        return attempt(context, status);
    }
    let arena = context.arena;
    let owner = arena.retention_owner();
    let policy = if retained {
        query::RoutineExecution::Retained
    } else {
        query::RoutineExecution::StatementReplay
    };
    let calls = status
        .write_calls
        .take()
        .unwrap_or_else(|| query::RoutineInvocationState::new(owner, policy));
    calls.require_arena(owner)?;
    let replay = sequence::begin_replay_cursor()?;
    let result = loop {
        sequence::rewind_replay_cursor(replay)?;
        calls.begin_attempt();
        let PlpgsqlExecHost::Routine {
            engine,
            guc,
            sqlprep,
            cursors,
            ..
        } = &mut context.host
        else {
            return Err(host_changed());
        };
        if calls.suspended.borrow().is_some()
            && let Err(error) = engine
                .resume_pending_function(
                    &calls,
                    owner,
                    arena,
                    context.txn,
                    sqlprep,
                    cursors,
                    guc,
                    context.responder,
                )
                .map_err(response_full)?
        {
            break Err(error);
        }
        let mark = context.txn.statement_mark(
            engine.wal.stage_mark(context.txn.txid),
            engine.storage.lock_mark(),
        );
        let output = context.responder.mark();
        let front = arena.mark();
        let effects = WriteAttemptEffects::capture(context)?.ok_or_else(host_changed)?;
        let outcome = query::with_routine_invocation_scope(
            Some(query::RoutineInvocationContext::new(&calls, owner)),
            || attempt(context, status),
        );
        let handed_off = status.pending_write.is_some();
        match outcome {
            Err(error)
                if error.sqlstate == sqlstate::INTERNAL_ROUTINE_INVOCATION && !handed_off =>
            {
                if effects.changed(context) {
                    break Err(retained_progress_required());
                }
                let PlpgsqlExecHost::Routine {
                    engine,
                    guc,
                    sqlprep,
                    cursors,
                    ..
                } = &mut context.host
                else {
                    return Err(host_changed());
                };
                engine.rollback_waiting_statement(context.txn, mark);
                context.responder.truncate_to(output)?;
                // The attempt's front allocations are unreachable; completed
                // calls own the arena's retained tail.
                unsafe { arena.rewind_to(front) };
                let pending = calls.take_pending().ok_or_else(|| {
                    sql_err!(
                        sqlstate::INTERNAL_ERROR,
                        "procedural write yielded without a pending call"
                    )
                })?;
                if let Err(error) = engine
                    .complete_pending_routine(
                        pending,
                        &calls,
                        owner,
                        arena,
                        context.txn,
                        sqlprep,
                        cursors,
                        guc,
                        context.responder,
                    )
                    .map_err(response_full)?
                {
                    break Err(error);
                }
            }
            Err(error)
                if !retained
                    && !handed_off
                    && control_flow::is_execution_wait(&error)
                    && effects.changed(context) =>
            {
                break Err(retained_progress_required());
            }
            outcome => break outcome,
        }
    };
    if retained
        && status.pending_write.is_none()
        && result.as_ref().is_err_and(control_flow::is_execution_wait)
    {
        status.write_calls = Some(calls);
    }
    result
}
