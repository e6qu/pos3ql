//! A PL/pgSQL procedure or `DO` block that waits keeps its lexical position,
//! values, settings and statement-journal position in startup pools. Its
//! statement resumes at the waiting body statement instead of running the body
//! again, so a wait after the procedure's own `COMMIT` or `ROLLBACK` works as
//! in PostgreSQL. Each body statement is its own rollback boundary.

use super::*;
use crate::mem::lease_pool::MutableSlice;
use call_environment::SavedCallEnvironment;

/// What a saved frame executes: a stored procedure version or a `DO` block.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) struct ProcedureIdentity {
    pub(super) oid: i32,
    pub(super) version: u64,
}

impl ProcedureIdentity {
    pub(super) const ANONYMOUS: Self = Self { oid: 0, version: 0 };

    pub(super) fn routine(routine: &crate::storage::RoutineDef) -> Self {
        Self {
            oid: crate::storage::routine_oid(routine),
            version: routine.version,
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
struct ProcedureOwner {
    identity: super::super::ExecutionIdentity,
    transaction: u32,
    command: u32,
    depth: u16,
    procedure: ProcedureIdentity,
}

impl ProcedureOwner {
    fn capture(context: &TriggerExecContext<'_, '_, '_>, procedure: ProcedureIdentity) -> Self {
        Self {
            identity: super::super::execution_identity(),
            transaction: context.txn.txid,
            command: context.txn.command_id(),
            depth: context.txn.trigger_depth(),
            procedure,
        }
    }
}

/// An unfinished procedure body owned by the statement that called it: the
/// top-level `CALL` or `DO` keeps it on the transaction, and a nested `CALL`
/// keeps it in its caller's saved frame.
pub(crate) struct SuspendedProcedure {
    state: MutableSlice<SuspendedProcedureState>,
}

struct SuspendedProcedureState {
    owner: ProcedureOwner,
    inputs: local_values::SavedValues,
    environment: SavedCallEnvironment,
    timing: super::super::statistics::PausedFunctionTiming,
    /// The PL/pgSQL settings the body started with; a resumed body keeps them.
    settings: super::super::guc::PlpgsqlSettings,
    execution: Option<execution_frame::SuspendedExecutionFrame>,
    progress: super::super::sequence::SavedStatementPosition,
}

fn changed() -> SqlError {
    sql_err!(
        sqlstate::SERIALIZATION_FAILURE,
        "saved procedure belongs to another execution or command"
    )
}

/// The body run: compiled program, invocation and the formal input values.
pub(super) struct ProcedureBody<'r, 'a> {
    pub(super) identity: ProcedureIdentity,
    pub(super) code: &'r CompiledProgram<'a>,
    pub(super) invocation: TriggerInvocation<'a>,
    pub(super) seeded: &'r [Datum<'a>],
}

/// Runs a body whose frame `execution` was just created. On a wait it moves
/// the frame, the call environment, the call's timing and the statement
/// journal position into `retention` and returns the wait.
#[expect(
    clippy::too_many_arguments,
    reason = "a retained body names its frame, program, environment, timing and owner slot"
)]
pub(super) fn execute<'a>(
    context: &mut TriggerExecContext<'_, 'a, '_>,
    mut execution: execution_frame::ExecutionFrame<'_, 'a>,
    body: ProcedureBody<'_, 'a>,
    inputs: &[Datum<'a>],
    environment: &CallEnvironment,
    timing: super::super::statistics::FunctionTiming,
    retention: &mut Option<SuspendedProcedure>,
    finish: &mut dyn FnMut(
        &execution_frame::ExecutionFrame<'_, 'a>,
        super::super::statistics::FunctionTiming,
    ) -> Result<(), SqlError>,
) -> Result<(), SqlError> {
    match run(context, &mut execution, &body) {
        Ok(()) => finish(&execution, timing),
        Err(error) if control_flow::is_execution_wait(&error) && !context.txn.procedure_restart => {
            let storage = context.storage();
            let mut saved_inputs = local_values::SavedValues::new(&storage.row_work)?;
            saved_inputs.replace(inputs)?;
            let saved_environment =
                SavedCallEnvironment::capture(environment, &storage.execution_control)?;
            let mut state = storage
                .execution_control
                .lease(1)
                .map_err(control_flow::execution_full)?;
            let progress = super::super::sequence::SavedStatementPosition::capture()?;
            let owner = ProcedureOwner::capture(context, body.identity);
            let execution = execution.suspend().map_err(|(_, error)| error)?;
            state
                .push_reserved(SuspendedProcedureState {
                    owner,
                    inputs: saved_inputs,
                    environment: saved_environment,
                    timing: timing.pause(),
                    settings: super::super::guc::PlpgsqlSettings {
                        variable_conflict: body.invocation.variable_conflict,
                        print_strict_params: body.invocation.print_strict_params,
                    },
                    execution: Some(execution),
                    progress,
                })
                .map_err(control_flow::execution_full)?;
            *retention = Some(SuspendedProcedure { state });
            Err(error)
        }
        Err(error) => Err(error),
    }
}

/// Runs the frame with each body statement as its own rollback boundary.
fn run<'a>(
    context: &mut TriggerExecContext<'_, 'a, '_>,
    execution: &mut execution_frame::ExecutionFrame<'_, 'a>,
    body: &ProcedureBody<'_, 'a>,
) -> Result<(), SqlError> {
    let PlpgsqlExecHost::Routine { engine, .. } = &context.host else {
        return Err(sql_err!(
            sqlstate::INTERNAL_ERROR,
            "a retained procedure needs the statement executor"
        ));
    };
    let mark = context.txn.statement_mark(
        engine.wal.stage_mark(context.txn.txid),
        engine.storage.lock_mark(),
    );
    let prior = context.txn.write_completion_boundary;
    context.txn.write_completion_boundary = Some(super::super::txn::WriteCompletionBoundary {
        mark,
        command: context.txn.command_id(),
        depth: context.txn.trigger_depth(),
        retained: true,
    });
    context.txn.running_procedures = context
        .txn
        .running_procedures
        .checked_add(1)
        .expect("bounded routine nesting cannot exhaust procedure depth");
    let definition = crate::storage::RelationDefinition::EMPTY;
    let result = (|| {
        execution.initialize(
            context,
            body.invocation,
            TriggerTransition {
                definition,
                old: None,
                new: None,
            },
            body.code,
            body.seeded,
        )?;
        execution.execute_block(
            context,
            definition,
            body.invocation,
            None,
            &mut None,
            false,
            None,
            body.code,
        )
    })();
    context.txn.running_procedures -= 1;
    // A body that committed leaves its caller a boundary in the current
    // transaction.
    context.txn.write_completion_boundary = match prior {
        Some(prior) if !context.txn.owns_statement_mark(prior.mark) => {
            let PlpgsqlExecHost::Routine { engine, .. } = &context.host else {
                unreachable!("a retained procedure runs on the statement executor")
            };
            Some(super::super::txn::WriteCompletionBoundary {
                mark: context.txn.statement_mark(
                    engine.wal.stage_mark(context.txn.txid),
                    engine.storage.lock_mark(),
                ),
                ..prior
            })
        }
        prior => prior,
    };
    match result? {
        None | Some(TriggerFlow::Return(TriggerReturnValue::Void)) => Ok(()),
        Some(TriggerFlow::Return(_)) | Some(TriggerFlow::Exit(_) | TriggerFlow::Continue(_)) => {
            Err(unsupported_trigger_body())
        }
    }
}

impl SuspendedProcedure {
    /// Continues the saved body at its waiting statement. `invocation` builds
    /// the call's invocation from the restored program's input sites and its
    /// saved PL/pgSQL settings, and
    /// `seeded` maps the saved inputs to named locals. A failed attachment
    /// leaves the saved body in `retention` unchanged.
    pub(super) fn resume<'a>(
        mut self,
        context: &mut TriggerExecContext<'_, 'a, '_>,
        identity: ProcedureIdentity,
        invocation: &dyn Fn(
            &'a [super::super::plpgsql_inputs::Site<'a>],
            super::super::guc::PlpgsqlSettings,
        ) -> Result<TriggerInvocation<'a>, SqlError>,
        seeded: &dyn Fn(&'a [Datum<'a>]) -> Result<&'a [Datum<'a>], SqlError>,
        retention: &mut Option<SuspendedProcedure>,
        finish: &mut dyn FnMut(
            &execution_frame::ExecutionFrame<'_, 'a>,
            super::super::statistics::FunctionTiming,
        ) -> Result<(), SqlError>,
    ) -> Result<(), SqlError> {
        let storage = context.storage();
        if !std::sync::Arc::ptr_eq(self.state.pool(), &storage.execution_control)
            || !std::sync::Arc::ptr_eq(self.state[0].inputs.pool(), &storage.row_work)
            || self.state[0].owner != ProcedureOwner::capture(context, identity)
        {
            *retention = Some(self);
            return Err(changed());
        }
        let arena = context.arena;
        let state = &mut self.state[0];
        let prior = state.progress.take();
        let Some(frame) = state.execution.take() else {
            state.progress = prior;
            *retention = Some(self);
            return Err(changed());
        };
        let attached = prior.with_resume(
            frame,
            super::super::lock::AdvisoryPositionTail::Unchanged,
            |frame, admission| {
                let environment = match state.environment.enter(context.txn) {
                    Ok(environment) => environment,
                    Err(error) => return Err((frame, error)),
                };
                let (parsed, code) = match frame.source().parse(arena) {
                    Ok(parsed) => parsed,
                    Err(error) => return Err((frame, error)),
                };
                let inputs = match state.inputs.decode(arena) {
                    Ok(inputs) => inputs,
                    Err(error) => return Err((frame, error)),
                };
                let invocation = match invocation(parsed.input_sites, state.settings) {
                    Ok(invocation) => invocation,
                    Err(error) => return Err((frame, error)),
                };
                let seeded = match seeded(inputs) {
                    Ok(seeded) => seeded,
                    Err(error) => return Err((frame, error)),
                };
                let timing = match core::mem::take(&mut state.timing).resume() {
                    Ok(timing) => timing,
                    Err((timing, error)) => {
                        state.timing = timing;
                        return Err((frame, error));
                    }
                };
                let mut execution = match frame.restore(arena, &code) {
                    Ok(execution) => execution,
                    Err((frame, error)) => {
                        state.timing = timing.pause();
                        return Err((frame, error));
                    }
                };
                admission.commit();
                context.params = inputs;
                let transaction = context.txn.txid;
                let result = run(
                    context,
                    &mut execution,
                    &ProcedureBody {
                        identity,
                        code: &code,
                        invocation,
                        seeded,
                    },
                );
                // Transaction control ends the caller's checked-input prefix
                // together with the journal that held it.
                if context.txn.txid == transaction
                    && let Err(error) = admission.preserve_caller_inputs()
                {
                    return Ok(Err(error));
                }
                Ok(match result {
                    Ok(()) => finish(&execution, timing),
                    Err(error)
                        if control_flow::is_execution_wait(&error)
                            && !context.txn.procedure_restart =>
                    {
                        let captured = SavedCallEnvironment::capture(
                            &environment,
                            &context.storage().execution_control,
                        )
                        .and_then(|environment| {
                            let progress =
                                super::super::sequence::SavedStatementPosition::capture()?;
                            let frame = execution.suspend().map_err(|(_, error)| error)?;
                            Ok((environment, progress, frame))
                        });
                        match captured {
                            Ok((environment, progress, frame)) => {
                                state.environment = environment;
                                state.progress = progress;
                                state.execution = Some(frame);
                                state.owner = ProcedureOwner::capture(context, identity);
                                state.timing = timing.pause();
                                Err(error)
                            }
                            Err(capture) => Err(capture),
                        }
                    }
                    Err(error) => Err(error),
                })
            },
        );
        match attached {
            Ok(Ok(Ok(()))) => Ok(()),
            Ok(Ok(Err(error))) => {
                if self.state[0].execution.is_some() {
                    *retention = Some(self);
                }
                Err(error)
            }
            Ok(Err((frame, error))) | Err((frame, error)) => {
                let state = &mut self.state[0];
                state.execution = Some(frame);
                state.progress = prior;
                *retention = Some(self);
                Err(error)
            }
        }
    }
}
