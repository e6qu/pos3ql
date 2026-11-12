//! SQL bodies own their statement position, child calls and unfinished writes.

use super::call_environment::{CallEnvironment, SavedCallEnvironment};
use super::function_execution::FunctionOwner;
use super::*;
use crate::mem::lease_pool::SharedSlice;
use crate::sql::{query, sequence, type_input_plans, type_inputs};

struct StoredSource {
    path: crate::storage::PathContext,
    dependencies: SharedSlice<crate::storage::StoredQueryDependency>,
    inputs: crate::storage::StoredTypeInputs,
}

impl StoredSource {
    fn view(&self) -> crate::storage::StoredQueryDependencyView<'_> {
        crate::storage::StoredQueryDependencyView::new(&self.dependencies)
            .with_type_inputs(&self.inputs)
    }
}

struct SqlSource {
    text: SharedSlice<u8>,
    checked_inputs: SharedSlice<u8>,
    parameters: SharedSlice<RoutineArgumentDef>,
    columns: SharedSlice<RoutineArgumentDef>,
    kind: crate::storage::RoutineKind,
    return_type: RoutineArgumentDef,
    body_kind: crate::storage::RoutineBodyKind,
    name: SqlName,
    oid: i32,
    collation: crate::sql::ast::Collation,
    owner: type_input_plans::Owner,
    input_source: type_inputs::Source,
    stored: Option<StoredSource>,
    /// Whether the routine's declaration lets PostgreSQL inline a body that
    /// is a single plain `SELECT` of one expression into its caller, which
    /// then reports no context line for it.
    inlinable: bool,
    /// PostgreSQL's planner runs an IMMUTABLE function called with constant
    /// arguments instead of inlining it.
    planned_with_constants: bool,
}

impl SqlSource {
    fn inlining(
        &self,
        program: &query::RoutineFunctionProgram<'_>,
    ) -> crate::sql::error_context::SqlFunctionInlining {
        choose_inlining(self.planned_with_constants, self.inlinable, program)
    }
}

struct ParsedSource<'a> {
    text: &'a str,
    inputs: &'a [type_inputs::Binding<'a>],
    program: query::RoutineFunctionProgram<'a>,
}

impl SqlSource {
    /// Only functions run here; their kind carries any output record.
    fn result_contract<'a>(
        &self,
        storage: &Storage,
        transaction: u32,
        arena: &'a Arena,
    ) -> Result<query::RoutineResultContract<'a>, SqlError> {
        query::RoutineResultContract::declared(
            storage,
            transaction,
            self.kind,
            &self.columns,
            false,
            arena,
        )
    }

    fn declared_result<'a>(
        &self,
        storage: &Storage,
        transaction: u32,
        arena: &'a Arena,
    ) -> Result<&'a str, SqlError> {
        self.result_contract(storage, transaction, arena)
            .map(query::RoutineResultContract::name)
    }

    fn parse<'a>(
        &self,
        storage: &Storage,
        transaction: u32,
        arena: &'a Arena,
    ) -> Result<ParsedSource<'a>, SqlError> {
        let text = arena
            .alloc_str(core::str::from_utf8(&self.text).expect("SQL sources copy UTF-8"))
            .map_err(|_| query::arena_full_pub())?;
        let bytes = arena
            .alloc_slice_copy(&self.checked_inputs)
            .map_err(|_| query::arena_full_pub())?;
        let inputs = type_inputs::decode(bytes, arena)?;
        let name = arena
            .alloc_str(self.name.as_str())
            .map_err(|_| query::arena_full_pub())?;
        let program = query::parse_stored_routine_function_program(
            self.body_kind,
            text,
            arena,
            self.result_contract(storage, transaction, arena)?,
            name,
            &self.parameters,
            inputs,
            |parser| match &self.stored {
                Some(stored) => {
                    query::validate_stored_type_inputs(parser, storage, transaction, stored.view())
                }
                None => type_inputs::validate_restored(parser, storage, transaction),
            },
        )?;
        Ok(ParsedSource {
            text,
            inputs,
            program,
        })
    }

    fn bind_statement<'a>(
        &self,
        statement: &'a Stmt<'a>,
        storage: &Storage,
        transaction: u32,
        arguments: &[Datum<'a>],
        arena: &'a Arena,
    ) -> Result<&'a Stmt<'a>, SqlError> {
        match &self.stored {
            Some(stored) => query::bind_routine_statement_source(
                statement,
                storage,
                transaction,
                stored.path,
                stored.view(),
                arena,
                arguments,
            ),
            None => Ok(statement),
        }
    }
}

enum SqlResult {
    Scalar {
        value: local_values::SavedValues,
        present: bool,
        checked: bool,
    },
    Rows(set_rows::SavedRows),
}

impl SqlResult {
    fn clear(&mut self) {
        match self {
            Self::Scalar {
                value,
                present,
                checked,
            } => {
                value.clear();
                *present = false;
                *checked = false;
            }
            Self::Rows(rows) => rows.clear(),
        }
    }

    #[expect(
        clippy::too_many_arguments,
        reason = "return conversion uses its explicit row, selected contract and evaluation context"
    )]
    fn capture(
        &mut self,
        source: &SqlSource,
        shape: query::RoutineResultShape<'_>,
        values: &[Datum<'_>],
        storage: &Storage,
        txid: u32,
        sequences: &dyn crate::sql::eval::SequenceAccess,
        write: bool,
        arena: &Arena,
    ) -> Result<(), SqlError> {
        match self {
            Self::Scalar {
                value,
                present,
                checked,
            } => {
                if !*present {
                    let captured = shape.capture(
                        values,
                        || source.declared_result(storage, txid, arena),
                        arena,
                    )?;
                    let captured = if write {
                        coerce_sql_result(captured, source, storage, txid, arena, Some(sequences))?
                    } else {
                        captured
                    };
                    value.replace(&[captured])?;
                    *present = true;
                    *checked = write;
                }
                Ok(())
            }
            Self::Rows(rows) => {
                let width =
                    if matches!(source.kind, crate::storage::RoutineKind::SetFunction { .. }) {
                        1
                    } else {
                        source.columns.len()
                    };
                if values.len() != width {
                    return Err(query::routine_result_width_mismatch(
                        source.declared_result(storage, txid, arena)?,
                        !matches!(
                            source.kind.function_result(),
                            Some(ColType::Record | ColType::Composite(_))
                        ),
                        values.len(),
                        width,
                    ));
                }
                let converted = arena
                    .alloc_slice_with(width, |_| Datum::Null)
                    .map_err(|_| query::arena_full_pub())?;
                for (index, value) in values.iter().copied().enumerate() {
                    let target =
                        if matches!(source.kind, crate::storage::RoutineKind::SetFunction { .. }) {
                            source.return_type
                        } else {
                            source.columns[index]
                        };
                    converted[index] = coerce_routine_argument(
                        detach_routine_datum(value, arena)?,
                        target,
                        storage,
                        txid,
                        arena,
                        Some(sequences),
                    )?;
                }
                rows.append(converted)
            }
        }
    }
}

/// Whether PostgreSQL's `inline_function` would replace a call of this body
/// by its expression: one `SELECT` of one expression with no other clause,
/// subquery or set-returning call.
/// Whether PostgreSQL's planner may inline a call of `routine`, by its
/// declaration alone.
fn inlinable_declaration(routine: &crate::storage::RoutineDef) -> bool {
    !routine.attributes.security_definer
        && !routine.attributes.strict
        && routine.config_count == 0
        && !routine.kind.is_set_returning()
        && routine.kind.function_result() != Some(ColType::Record)
}

/// How PostgreSQL's planner treats a call of a SQL-language function, which
/// decides its error context: an IMMUTABLE function called with constant
/// arguments runs at plan time, a body of one plain expression is inlined,
/// and any other body runs as its own statements. A folded body's errors are
/// raised in planning, so a call that runs is never `during inlining`.
pub(crate) fn sql_function_inlining(
    routine: &crate::storage::RoutineDef,
    program: &query::RoutineFunctionProgram<'_>,
    constant_arguments: bool,
) -> crate::sql::error_context::SqlFunctionInlining {
    choose_inlining(
        planned_with_constants(routine, constant_arguments),
        inlinable_declaration(routine),
        program,
    )
}

fn planned_with_constants(routine: &crate::storage::RoutineDef, constant_arguments: bool) -> bool {
    constant_arguments
        && routine.attributes.volatility == crate::storage::RoutineVolatility::Immutable
}

fn choose_inlining(
    planned_with_constants: bool,
    inlinable: bool,
    program: &query::RoutineFunctionProgram<'_>,
) -> crate::sql::error_context::SqlFunctionInlining {
    use crate::sql::error_context::SqlFunctionInlining;
    // A body that fails while folded with constants fails in planning; one
    // that runs here was inlined and its errors carry no frame.
    if planned_with_constants || !inlinable || !inlined_by_postgresql(program) {
        SqlFunctionInlining::Executed
    } else {
        SqlFunctionInlining::Inlined
    }
}

fn inlined_by_postgresql(program: &query::RoutineFunctionProgram<'_>) -> bool {
    let query::RoutineFunctionResult::Query(query::RoutineQuery::Select(select)) = program.result
    else {
        return false;
    };
    let [crate::sql::ast::SelectItem::Expr { expression, .. }] = select.items else {
        return false;
    };
    program.preceding.is_empty()
        && !select.distinct
        && select.distinct_on.is_empty()
        && select.from.is_none()
        && select.where_clause.is_none()
        && select.group_by.is_empty()
        && select.grouping_sets.is_empty()
        && select.having.is_none()
        && select.order_by.is_empty()
        && select.limit.is_none()
        && select.offset.is_none()
        && select.with.is_empty()
        && select.set_body.is_none()
        && select.locking.is_empty()
        && !expression.contains_subquery()
}

pub(crate) struct SuspendedSqlFunction {
    state: MutableSlice<SqlExecution>,
}

struct SqlExecution {
    source: SqlSource,
    arguments: local_values::SavedValues,
    result: SqlResult,
    types: Option<crate::storage::StatementFileLease>,
    environment: SavedCallEnvironment,
    owner: FunctionOwner,
    boundary: super::super::txn::StatementMark,
    policy: query::RoutineExecution,
    step: usize,
    finished_body: bool,
    pending_query: Option<query::RoutineInvocationState>,
    pending_write: Option<write_completion::PendingNestedWrite>,
    command: Option<crate::sql::txn::SuspendedTriggerSql>,
    step_position: Option<sequence::SavedStatementPosition>,
    suspended_position: Option<sequence::SavedStatementPosition>,
}

impl SuspendedSqlFunction {
    pub(crate) fn oid(&self) -> i32 {
        self.state[0].source.oid
    }
    pub(crate) fn is_set_returning(&self) -> bool {
        self.state[0].source.kind.is_set_returning()
    }

    #[expect(
        clippy::too_many_arguments,
        reason = "saved SQL calls attach to their original execution services"
    )]
    pub(crate) fn poll<'a>(
        &mut self,
        engine: &mut super::super::Engine,
        transaction: &mut TxnState,
        prepared: &mut super::super::prep::SqlPreparedPool,
        cursors: &mut super::super::cursor::CursorPool,
        settings: &super::super::guc::GucState,
        arena: &'a Arena,
        responder: &mut Responder<'_>,
    ) -> Result<FunctionOutput<'a>, SqlError> {
        if !std::sync::Arc::ptr_eq(self.state.pool(), &engine.storage.execution_control)
            || !std::sync::Arc::ptr_eq(self.state[0].arguments.pool(), &engine.storage.row_work)
        {
            return Err(changed());
        }
        let state = &mut self.state[0];
        let mut context = TriggerExecContext {
            host: PlpgsqlExecHost::Routine {
                engine,
                guc: settings,
                sqlprep: prepared,
                cursors,
                transaction_context: PlpgsqlTransactionContext::Atomic,
            },
            txn: transaction,
            arena,
            params: crate::sql::eval::NO_PARAMS,
            responder,
        };
        if state.owner != FunctionOwner::capture(&context) {
            return Err(changed());
        }
        let position = state.suspended_position.take().ok_or_else(changed)?;
        let mut attached = false;
        let outcome = position.with_resume(
            (),
            super::super::lock::AdvisoryPositionTail::Unchanged,
            |(), admission| {
                let _types = state
                    .types
                    .as_ref()
                    .map(|types| types.enter())
                    .transpose()?;
                let environment = state.environment.enter(context.txn)?;
                let parsed = state
                    .source
                    .parse(context.storage(), context.txn.txid, arena)?;
                let arguments = state.arguments.decode(arena)?;
                let _formal = enter_routine_parameter_types(
                    &state.source.parameters,
                    Some(&query::storage_catalog(
                        context.storage(),
                        arena,
                        context.txn.txid,
                    )),
                    state.source.collation,
                )?;
                if let Some(query) = &state.pending_query {
                    query.require_arena(arena.retention_owner())?;
                }
                admission.commit();
                attached = true;
                let result = state.execute(&parsed, arguments, &mut context);
                admission.preserve_caller_inputs()?;
                if result.as_ref().is_err_and(control_flow::is_execution_wait) {
                    state.environment = SavedCallEnvironment::capture(
                        &environment,
                        &context.storage().execution_control,
                    )?;
                    state.suspended_position = Some(sequence::SavedStatementPosition::capture()?);
                } else if result.is_ok() {
                    state.environment.complete();
                }
                result
            },
        );
        match outcome {
            Ok(result) => {
                if !attached
                    || state.suspended_position.is_none()
                        && result.as_ref().is_err_and(control_flow::is_execution_wait)
                {
                    state.suspended_position = Some(position);
                }
                result
            }
            Err(((), error)) => {
                state.suspended_position = Some(position);
                Err(error)
            }
        }
    }
}

impl SqlExecution {
    fn enter_step(&mut self) -> Result<(), SqlError> {
        if let Some(position) = self.step_position.take() {
            if let Err(((), error)) = position.with_resume(
                (),
                super::super::lock::AdvisoryPositionTail::Appended,
                |(), admission| admission.commit(),
            ) {
                self.step_position = Some(position);
                return Err(error);
            }
            self.step_position = Some(position);
        } else {
            self.step_position = Some(sequence::SavedStatementPosition::capture()?);
        }
        Ok(())
    }

    fn execute<'a>(
        &mut self,
        parsed: &ParsedSource<'a>,
        arguments: &'a [Datum<'a>],
        context: &mut TriggerExecContext<'_, 'a, '_>,
    ) -> Result<FunctionOutput<'a>, SqlError> {
        let prior = context.txn.write_completion_boundary;
        if self.policy == query::RoutineExecution::Retained {
            context.txn.write_completion_boundary =
                Some(super::super::txn::WriteCompletionBoundary {
                    mark: self.boundary,
                    command: context.txn.command_id(),
                    depth: context.txn.trigger_depth(),
                });
        }
        let name = self.source.name;
        let inlining = self.source.inlining(&parsed.program);
        let frame = crate::sql::error_context::SqlFunctionFrame {
            name: name.as_str(),
            body: parsed.text,
            statement: core::cell::Cell::new(0),
            inlining,
        };
        let result = crate::sql::error_context::within(&frame, || {
            crate::sql::error_position::with_internal_source(parsed.text, || {
                type_input_plans::with_owner(self.source.owner, || {
                    type_inputs::with_source(
                        self.source.input_source,
                        parsed.text,
                        parsed.inputs,
                        || self.execute_body(parsed, arguments, context, &frame),
                    )
                })
            })
        });
        context.txn.write_completion_boundary = prior;
        result
    }

    fn execute_body<'a>(
        &mut self,
        parsed: &ParsedSource<'a>,
        arguments: &'a [Datum<'a>],
        context: &mut TriggerExecContext<'_, 'a, '_>,
        frame: &crate::sql::error_context::SqlFunctionFrame<'_>,
    ) -> Result<FunctionOutput<'a>, SqlError> {
        while !self.finished_body {
            frame.statement.set(self.step + 1);
            self.enter_step()?;
            let final_step = self.step == parsed.program.preceding.len();
            let mut command = if let Some(command) = self.command.take() {
                match command.resume(context.txn) {
                    Ok(command) => command,
                    Err((command, error)) => {
                        self.command = Some(command);
                        return Err(error);
                    }
                }
            } else {
                context.txn.enter_trigger_sql(context.host.storage())?
            };
            let result = {
                let mut statement_context = TriggerExecContext {
                    host: context.host.reborrow(),
                    txn: &mut command,
                    arena: context.arena,
                    params: context.params,
                    responder: context.responder,
                };
                if final_step {
                    match parsed.program.result {
                        query::RoutineFunctionResult::Query(query) => {
                            self.query(query, arguments, &mut statement_context, true)
                        }
                        query::RoutineFunctionResult::DataModification(statement) => {
                            self.statement(statement, arguments, &mut statement_context, true)
                        }
                        query::RoutineFunctionResult::Void(statement) => {
                            self.statement(statement, arguments, &mut statement_context, false)
                        }
                        query::RoutineFunctionResult::Forbidden(statement) => {
                            Err(query::routine_forbidden_statement_error(statement))
                        }
                        query::RoutineFunctionResult::Empty => Ok(()),
                    }
                } else {
                    match parsed
                        .program
                        .preceding
                        .get(self.step)
                        .ok_or_else(changed)?
                    {
                        query::RoutinePrelude::Statement(statement) => {
                            self.statement(statement, arguments, &mut statement_context, false)
                        }
                        query::RoutinePrelude::Forbidden(statement) => {
                            Err(query::routine_forbidden_statement_error(statement))
                        }
                    }
                }
            };
            match result {
                Err(error) if control_flow::is_execution_wait(&error) => {
                    self.command = Some(command.suspend());
                    return Err(error);
                }
                result => {
                    drop(command);
                    result?;
                }
            }
            self.pending_query = None;
            self.step_position = None;
            if final_step {
                self.finished_body = true;
            } else {
                self.step += 1;
            }
        }
        self.enter_step()?;
        let output = match &mut self.result {
            SqlResult::Rows(rows) => Ok(FunctionOutput::Rows(rows.rows(context.arena)?)),
            SqlResult::Scalar {
                value,
                present,
                checked,
            } => {
                let arena = context.arena;
                if !*present {
                    return Ok(FunctionOutput::Scalar(Datum::Null));
                }
                let value = value.read(0, arena)?;
                if *checked {
                    return Ok(FunctionOutput::Scalar(value));
                }
                let txid = context.txn.txid;
                execute_procedural_query(
                    context,
                    false,
                    (self.policy == query::RoutineExecution::Retained)
                        .then_some(&mut self.pending_query),
                    |storage, sequences| {
                        coerce_sql_result(
                            value,
                            &self.source,
                            storage,
                            txid,
                            arena,
                            Some(sequences),
                        )
                    },
                )
                .map(FunctionOutput::Scalar)
            }
        };
        if output.is_ok() {
            self.step_position = None;
        }
        output
    }

    fn query<'a>(
        &mut self,
        query: &query::RoutineQuery<'a>,
        arguments: &'a [Datum<'a>],
        context: &mut TriggerExecContext<'_, 'a, '_>,
        capture: bool,
    ) -> Result<(), SqlError> {
        // Data-modifying WITH queries run before the query that reads them,
        // under the statement's snapshot, as in a top-level query. A query
        // resuming after a wait continues from its saved state; its WITH
        // writes already ran and must not run again.
        if let query::RoutineQuery::Select(select) = query
            && select.with.iter().any(|cte| cte.dml.is_some())
            && self.pending_query.is_none()
        {
            let params = core::mem::replace(&mut context.params, arguments);
            let bound = super::bind_plpgsql_dml_ctes(context, select);
            context.params = params;
            let (bound, written) = bound?;
            self.query(
                &query::RoutineQuery::Select(bound),
                arguments,
                context,
                capture,
            )?;
            let params = core::mem::replace(&mut context.params, arguments);
            let finished = super::finish_plpgsql_dml_ctes(context, select, written);
            context.params = params;
            return finished;
        }
        let arena = context.arena;
        let txid = context.txn.txid;
        let PlpgsqlExecHost::Routine {
            engine,
            guc,
            sqlprep,
            cursors,
            ..
        } = &mut context.host
        else {
            return Err(changed());
        };
        let snapshot = engine.storage.read_snapshot();
        engine
            .execute_retained_expression(
                arena,
                (self.policy == query::RoutineExecution::Retained)
                    .then_some(&mut self.pending_query),
                snapshot,
                context.txn,
                sqlprep,
                cursors,
                guc,
                context.responder,
                |storage, work, sequences, _calls, _cursors| {
                    if capture {
                        self.result.clear();
                    }
                    let shape = query::RoutineResultShape::from_contract(
                        self.source.kind,
                        &self.source.columns,
                        storage,
                        txid,
                        arena,
                    )?;
                    let stored = self
                        .source
                        .stored
                        .as_ref()
                        .map(|saved| (saved.path, saved.view()));
                    let scalar = capture && !self.source.kind.is_set_returning();
                    let mut emit = |values: &[Datum]| {
                        if capture {
                            self.result.capture(
                                &self.source,
                                shape,
                                values,
                                storage,
                                txid,
                                sequences,
                                false,
                                arena,
                            )
                        } else {
                            Ok(())
                        }
                    };
                    let evaluate =
                        |emit: &mut dyn for<'row> FnMut(&[Datum<'row>]) -> Result<(), SqlError>| {
                            query::execute_routine_query_under(
                                query,
                                storage,
                                txid,
                                work,
                                arguments,
                                true,
                                Some(sequences),
                                stored,
                                emit,
                            )
                        };
                    if scalar {
                        query::consume_query_rows(core::num::NonZeroU64::MIN, evaluate, &mut emit)
                    } else {
                        evaluate(&mut emit)
                    }
                },
            )
            .map_err(response_full)?
    }

    fn statement<'a>(
        &mut self,
        statement: &'a Stmt<'a>,
        arguments: &'a [Datum<'a>],
        context: &mut TriggerExecContext<'_, 'a, '_>,
        capture: bool,
    ) -> Result<(), SqlError> {
        match statement {
            Stmt::Select(select) => {
                return self.query(
                    &query::RoutineQuery::Select(select),
                    arguments,
                    context,
                    capture,
                );
            }
            Stmt::SetQuery(query) => {
                return self.query(
                    &query::RoutineQuery::Set(query),
                    arguments,
                    context,
                    capture,
                );
            }
            _ => {}
        }
        if let Some(pending) = &mut self.pending_write {
            pending.poll(context)?;
            self.pending_write = None;
            context.responder.take_affected_rows();
            return Ok(());
        }
        let statement = self.source.bind_statement(
            statement,
            context.storage(),
            context.txn.txid,
            arguments,
            context.arena,
        )?;
        self.write(statement, arguments, context, capture)
    }

    fn write<'a>(
        &mut self,
        statement: &'a Stmt<'a>,
        arguments: &'a [Datum<'a>],
        context: &mut TriggerExecContext<'_, 'a, '_>,
        capture: bool,
    ) -> Result<(), SqlError> {
        let arena = context.arena;
        let owner = arena.retention_owner();
        let calls = self
            .pending_query
            .take()
            .unwrap_or_else(|| query::RoutineInvocationState::new(owner, self.policy));
        calls.require_arena(owner)?;
        let replay = sequence::begin_replay_cursor()?;
        let result = loop {
            sequence::rewind_replay_cursor(replay)?;
            calls.begin_attempt();
            let unfinished = calls.suspended.borrow().is_some();
            if unfinished {
                let PlpgsqlExecHost::Routine {
                    engine,
                    guc,
                    sqlprep,
                    cursors,
                    ..
                } = &mut context.host
                else {
                    return Err(changed());
                };
                if let Err(error) = engine
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
            }
            if capture {
                self.result.clear();
            }
            let shape = query::RoutineResultShape::from_contract(
                self.source.kind,
                &self.source.columns,
                context.storage(),
                context.txn.txid,
                arena,
            )?;
            let mark = {
                let PlpgsqlExecHost::Routine { engine, .. } = &context.host else {
                    return Err(changed());
                };
                context.txn.statement_mark(
                    engine.wal.stage_mark(context.txn.txid),
                    engine.storage.lock_mark(),
                )
            };
            let before = context.txn.transaction_effect_mark();
            let descriptors = context.txn.statement_large_object_descriptors(arena)?;
            let advisory = context.storage().advisory_statement_mark();
            let settings = match &context.host {
                PlpgsqlExecHost::Routine { guc, .. } => guc.statement_settings(),
                _ => return Err(changed()),
            };
            let outcome = query::with_routine_invocation_scope(
                Some(query::RoutineInvocationContext::new(&calls, owner)),
                || {
                    let mut capture_rows =
                        |storage: &Storage,
                         txid: u32,
                         sequences: &dyn crate::sql::eval::SequenceAccess,
                         values: &[Datum]| {
                            if capture {
                                self.result.capture(
                                    &self.source,
                                    shape,
                                    values,
                                    storage,
                                    txid,
                                    sequences,
                                    true,
                                    arena,
                                )
                            } else {
                                Ok(())
                            }
                        };
                    if matches!(
                        statement,
                        Stmt::Insert(_) | Stmt::Update(_) | Stmt::Delete(_)
                    ) {
                        execute_nested_data_modification(
                            context,
                            statement,
                            arguments,
                            None,
                            Some(&mut capture_rows),
                            &mut self.pending_write,
                        )
                    } else {
                        let output_mark = context.responder.mark();
                        let PlpgsqlExecHost::Routine {
                            engine,
                            guc,
                            sqlprep,
                            cursors,
                            ..
                        } = &mut context.host
                        else {
                            return Err(changed());
                        };
                        let result = context
                            .responder
                            .without_query_output(|responder| {
                                engine.execute_routine_stmt(
                                    statement,
                                    arena,
                                    arguments,
                                    context.txn,
                                    sqlprep,
                                    cursors,
                                    guc,
                                    responder,
                                    if capture {
                                        Some(&mut capture_rows)
                                    } else {
                                        None
                                    },
                                )
                            })
                            .map_err(response_full)?;
                        context.responder.truncate_to(output_mark)?;
                        result.map(|()| 0)
                    }
                },
            );
            let changed_effects = context.txn.transaction_effect_mark() != before
                || !context
                    .txn
                    .matches_statement_large_object_descriptors(descriptors)
                || context.storage().advisory_statement_mark() != advisory
                || match &context.host {
                    PlpgsqlExecHost::Routine { guc, .. } => {
                        !guc.matches_statement_settings(&settings)
                    }
                    _ => true,
                };
            match outcome {
                Ok(_) => break Ok(()),
                Err(error) if error.sqlstate == sqlstate::INTERNAL_ROUTINE_INVOCATION => {
                    if let Some(write) = &mut self.pending_write {
                        let result = write.poll(context).map(|_| ());
                        if result.is_ok() {
                            self.pending_write = None;
                            context.responder.take_affected_rows();
                        }
                        break result;
                    }
                    if changed_effects {
                        break Err(sql_err!(
                            sqlstate::FEATURE_NOT_SUPPORTED,
                            "SQL row expression requires retained write progress"
                        ));
                    }
                    let PlpgsqlExecHost::Routine {
                        engine,
                        guc,
                        sqlprep,
                        cursors,
                        ..
                    } = &mut context.host
                    else {
                        return Err(changed());
                    };
                    engine.rollback_waiting_statement(context.txn, mark);
                    let pending = calls.take_pending().ok_or_else(|| {
                        sql_err!(
                            sqlstate::INTERNAL_ERROR,
                            "SQL write yielded without a pending call"
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
                Err(error) if changed_effects && control_flow::is_execution_wait(&error) => {
                    break Err(sql_err!(
                        sqlstate::FEATURE_NOT_SUPPORTED,
                        "SQL row expression requires retained write progress"
                    ));
                }
                Err(error) => break Err(error),
            }
        };
        if self.policy == query::RoutineExecution::Retained
            && result.as_ref().is_err_and(control_flow::is_execution_wait)
        {
            self.pending_query = Some(calls);
        }
        result
    }
}

#[expect(
    clippy::too_many_arguments,
    reason = "SQL calls capture their selected contract and original execution services"
)]
pub(in crate::sql) fn execute_sql_function<'a>(
    engine: &mut super::super::Engine,
    transaction: &mut TxnState,
    prepared: &mut super::super::prep::SqlPreparedPool,
    cursors: &mut super::super::cursor::CursorPool,
    settings: &super::super::guc::GucState,
    slot: usize,
    routine: &crate::storage::RoutineDef,
    collation: crate::sql::ast::Collation,
    inputs: &[Datum<'a>],
    body_inputs: super::super::PreparedTypeInputs<'_>,
    arena: &'a Arena,
    responder: &mut Responder<'_>,
    environment: &CallEnvironment,
    policy: query::RoutineExecution,
    suspended: &mut Option<SuspendedFunction>,
    constant_arguments: bool,
) -> Result<FunctionOutput<'a>, SqlError> {
    let controls = &engine.storage.execution_control;
    let rows = &engine.storage.row_work;
    let mut state = controls.lease(1).map_err(control_flow::execution_full)?;
    let parameters = controls
        .copy_slice(routine.arguments())
        .map_err(control_flow::execution_full)?;
    let columns = controls
        .copy_slice(&routine.result_columns[..routine.result_column_count])
        .map_err(control_flow::execution_full)?;
    let text = rows
        .copy_slice(routine.body.as_str().as_bytes())
        .map_err(control_flow::execution_full)?;
    let length = type_inputs::encoded_len(body_inputs.bindings)?;
    let mut checked_inputs = rows.lease(length).map_err(control_flow::execution_full)?;
    for _ in 0..length {
        checked_inputs
            .push_reserved(0)
            .map_err(control_flow::execution_full)?;
    }
    type_inputs::encode_into(body_inputs.bindings, &mut checked_inputs)?;
    let stored = if routine.body_kind == crate::storage::RoutineBodyKind::String {
        None
    } else {
        let image = engine
            .storage
            .routine_dependencies_for(slot, transaction.txid);
        Some(StoredSource {
            path: engine.storage.compute_path(
                routine.creation_path.as_str(),
                super::super::eval::funcs::system::session_user_owned().as_str(),
                transaction.txid,
            ),
            dependencies: controls
                .copy_slice(image.entries())
                .map_err(control_flow::execution_full)?,
            inputs: image
                .view()
                .type_inputs()
                .expect("stored SQL dependencies own their input image")
                .clone(),
        })
    };
    let mut arguments = local_values::SavedValues::new(rows)?;
    arguments.replace(inputs)?;
    let result = if routine.kind.is_set_returning() {
        SqlResult::Rows(set_rows::SavedRows::new(rows)?)
    } else {
        SqlResult::Scalar {
            value: local_values::SavedValues::new(rows)?,
            present: false,
            checked: false,
        }
    };
    let return_type = RoutineArgumentDef {
        name: SqlName::EMPTY,
        ctype: routine.kind.function_result().ok_or_else(changed)?,
        user_type: routine
            .function_result_contract()
            .and_then(|result| result.user_type),
    };
    let input_types = arena
        .alloc_slice_with(body_inputs.bindings.len(), |_| None)
        .map_err(|_| query::arena_full_pub())?;
    for (selected, input) in input_types.iter_mut().zip(body_inputs.bindings) {
        if crate::storage::NamedTypeKind::from_oid(input.type_oid).is_some() {
            *selected = catalog_column_type(&engine.storage, transaction.txid, input.type_oid)
                .ok_or_else(|| {
                    sql_err!(
                        sqlstate::UNDEFINED_OBJECT,
                        "selected SQL input type is unavailable"
                    )
                })?
                .1;
        }
    }
    let types = engine.storage.select_type_definition_graph(
        routine
            .arguments()
            .iter()
            .filter_map(|argument| argument.user_type)
            .chain(columns.iter().filter_map(|column| column.user_type))
            .chain(return_type.user_type)
            .chain(input_types.iter().copied().flatten()),
        transaction.txid,
        arena,
    )?;
    let _types = types.as_ref().map(|types| types.enter()).transpose()?;
    let saved_environment = SavedCallEnvironment::capture(environment, controls)?;
    let boundary = transaction.statement_mark(
        engine.wal.stage_mark(transaction.txid),
        engine.storage.lock_mark(),
    );
    let mut context = TriggerExecContext {
        host: PlpgsqlExecHost::Routine {
            engine,
            guc: settings,
            sqlprep: prepared,
            cursors,
            transaction_context: PlpgsqlTransactionContext::Atomic,
        },
        txn: transaction,
        arena,
        params: crate::sql::eval::NO_PARAMS,
        responder,
    };
    let owner = FunctionOwner::capture(&context);
    state
        .push_reserved(SqlExecution {
            source: SqlSource {
                text,
                checked_inputs: checked_inputs.freeze(),
                parameters,
                columns,
                kind: routine.kind,
                return_type,
                body_kind: routine.body_kind,
                name: routine.name,
                oid: crate::storage::routine_oid(routine),
                collation,
                owner: body_inputs.owner,
                input_source: body_inputs.source,
                stored,
                inlinable: inlinable_declaration(routine),
                planned_with_constants: planned_with_constants(routine, constant_arguments),
            },
            arguments,
            result,
            types,
            environment: saved_environment,
            owner,
            boundary,
            policy,
            step: 0,
            finished_body: false,
            pending_query: None,
            pending_write: None,
            command: None,
            step_position: None,
            suspended_position: None,
        })
        .map_err(control_flow::execution_full)?;
    let parsed = state[0]
        .source
        .parse(context.storage(), context.txn.txid, arena)?;
    let arguments = state[0].arguments.decode(arena)?;
    check_polymorphic_sql_result(
        context.storage(),
        context.txn.txid,
        routine,
        arguments,
        parsed.text,
        &parsed.program,
        arena,
    )?;
    let result = state[0].execute(&parsed, arguments, &mut context);
    if policy == query::RoutineExecution::Retained
        && result.as_ref().is_err_and(control_flow::is_execution_wait)
    {
        state[0].environment =
            SavedCallEnvironment::capture(environment, &context.storage().execution_control)?;
        state[0].suspended_position = Some(sequence::SavedStatementPosition::capture()?);
        *suspended = Some(SuspendedFunction::Sql(SuspendedSqlFunction { state }));
    }
    result
}

/// PostgreSQL's `check_sql_fn_retval` for a function declared to return a
/// polymorphic type, once this call's arguments resolve it, and for any other
/// scalar result: an inlinable body fails `during inlining`, any other at its
/// final statement.
pub(crate) fn check_polymorphic_sql_result<'a>(
    storage: &Storage,
    txid: u32,
    routine: &crate::storage::RoutineDef,
    arguments: &[Datum<'_>],
    body: &str,
    program: &query::RoutineFunctionProgram<'a>,
    arena: &'a Arena,
) -> Result<(), SqlError> {
    if routine.kind.is_set_returning() || arguments.len() > crate::storage::MAX_ROUTINE_ARGUMENTS {
        return Ok(());
    }
    let mut argument_oids = [0; crate::storage::MAX_ROUTINE_ARGUMENTS];
    for (target, argument) in argument_oids.iter_mut().zip(arguments) {
        *target = argument.type_oid();
    }
    check_polymorphic_result(
        storage,
        txid,
        routine,
        &argument_oids[..arguments.len()],
        body,
        program,
        inlined_by_postgresql(program),
        arena,
    )
}

#[expect(clippy::too_many_arguments, reason = "SQL function check context")]
fn check_polymorphic_result<'a>(
    storage: &Storage,
    txid: u32,
    routine: &crate::storage::RoutineDef,
    argument_oids: &[i32],
    body: &str,
    program: &query::RoutineFunctionProgram<'a>,
    inlined: bool,
    arena: &'a Arena,
) -> Result<(), SqlError> {
    let _ = argument_oids;
    // A polymorphic call runs an instance whose result is already resolved.
    let scalar = routine.kind.function_result().is_some_and(|result| {
        !matches!(
            result,
            ColType::Composite(_) | ColType::Record | ColType::Void
        )
    });
    let Some(resolved) = scalar
        .then(|| storage.routine_function_result_oid(routine, txid))
        .flatten()
    else {
        return Ok(());
    };
    let declared = crate::sql::catalog::format_type_name(storage, txid, resolved)?;
    let frame = crate::sql::error_context::SqlFunctionFrame {
        name: routine.name.as_str(),
        body,
        // PostgreSQL 18 checks the final query as it prepares that statement.
        statement: core::cell::Cell::new(program.preceding.len() + 1),
        inlining: if inlined && inlinable_declaration(routine) {
            crate::sql::error_context::SqlFunctionInlining::Folded
        } else {
            crate::sql::error_context::SqlFunctionInlining::Executed
        },
    };
    crate::sql::error_context::within(&frame, || {
        query::check_routine_scalar_result(
            program,
            resolved,
            declared.as_str(),
            storage,
            txid,
            arena,
        )
    })
}

/// The planner's inlining attempt for a call of `routine` checks a
/// polymorphic result as PostgreSQL's `inline_function` does.
#[expect(clippy::too_many_arguments, reason = "planner inlining context")]
pub(crate) fn check_inlined_polymorphic_result<'a>(
    storage: &Storage,
    txid: u32,
    routine: &crate::storage::RoutineDef,
    argument_oids: &[i32],
    body: &'a str,
    statement: &'a crate::sql::ast::Stmt<'a>,
    inlinable: bool,
    arena: &'a Arena,
) -> Result<(), SqlError> {
    let query = match statement {
        crate::sql::ast::Stmt::Select(select) => query::RoutineQuery::Select(select),
        crate::sql::ast::Stmt::SetQuery(set) => query::RoutineQuery::Set(set),
        _ => return Ok(()),
    };
    let program = query::RoutineFunctionProgram {
        preceding: &[],
        result: query::RoutineFunctionResult::Query(
            arena.alloc(query).map_err(|_| query::arena_full_pub())?,
        ),
    };
    check_polymorphic_result(
        storage,
        txid,
        routine,
        argument_oids,
        body,
        &program,
        inlinable,
        arena,
    )
}

fn response_full(_: WireFull) -> SqlError {
    sql_err!(
        sqlstate::PROGRAM_LIMIT_EXCEEDED,
        "SQL function response exceeds the send buffer"
    )
}

fn coerce_sql_result<'a>(
    value: Datum<'a>,
    source: &SqlSource,
    storage: &Storage,
    txid: u32,
    arena: &'a Arena,
    sequences: Option<&dyn crate::sql::eval::SequenceAccess>,
) -> Result<Datum<'a>, SqlError> {
    if matches!(
        source.kind,
        crate::storage::RoutineKind::RecordFunction {
            set_returning: false
        }
    ) {
        let Datum::Record(fields) = value else {
            return Err(sql_err!(
                sqlstate::DATATYPE_MISMATCH,
                "SQL record result has no declared fields"
            ));
        };
        if fields.len() != source.columns.len() {
            return Err(changed());
        }
        let fields = arena
            .alloc_slice_copy(fields)
            .map_err(|_| query::arena_full_pub())?;
        for (field, column) in fields.iter_mut().zip(source.columns.iter().copied()) {
            field.value =
                coerce_routine_argument(field.value, column, storage, txid, arena, sequences)?;
        }
        Ok(Datum::Record(fields))
    } else {
        coerce_routine_argument(value, source.return_type, storage, txid, arena, sequences)
    }
}

fn changed() -> SqlError {
    sql_err!(
        sqlstate::SERIALIZATION_FAILURE,
        "SQL function execution source or owner was replaced"
    )
}
