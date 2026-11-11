//! Function bodies retain their selected inputs and lexical execution across waits.

use super::call_environment::{CallEnvironment, SavedCallEnvironment};
use super::*;
use crate::mem::lease_pool::SharedSlice;

pub(crate) enum FunctionOutput<'a> {
    Scalar(Datum<'a>),
    Rows(&'a [&'a [u8]]),
}

#[derive(Clone, Copy)]
struct OutputField {
    name: SqlName,
    type_oid: i32,
    local: Option<usize>,
}

enum ReturnBinding {
    Value,
    Void,
    OutputValue(Option<usize>),
    OutputRecord(SharedSlice<OutputField>),
    Rows,
}

impl ReturnBinding {
    fn select(
        routine: &crate::storage::RoutineDef,
        kind: PlpgsqlProgramKind,
        locals: &[TriggerLocalDecl],
        storage: &Storage,
        transaction: u32,
    ) -> Result<Self, SqlError> {
        let local = |name: SqlName| {
            (!name.as_str().is_empty())
                .then(|| {
                    locals
                        .iter()
                        .position(|declaration| declaration.name == name)
                })
                .flatten()
        };
        match kind {
            PlpgsqlProgramKind::Function => Ok(Self::Value),
            PlpgsqlProgramKind::VoidFunction => Ok(Self::Void),
            PlpgsqlProgramKind::SetFunction => Ok(Self::Rows),
            PlpgsqlProgramKind::OutputFunction => {
                let mut outputs = routine
                    .parameters()
                    .iter()
                    .filter(|parameter| parameter.mode.is_output());
                if matches!(routine.kind, crate::storage::RoutineKind::Function { .. }) {
                    return Ok(Self::OutputValue(
                        outputs.next().and_then(|parameter| local(parameter.name)),
                    ));
                }
                let count = outputs.clone().count();
                let mut fields = storage
                    .execution_control
                    .lease(count)
                    .map_err(control_flow::execution_full)?;
                for parameter in outputs {
                    fields
                        .push_reserved(OutputField {
                            name: parameter.name,
                            type_oid: storage
                                .routine_type_oid(parameter.ctype, parameter.user_type, transaction)
                                .ok_or_else(|| {
                                    sql_err!(
                                        sqlstate::INTERNAL_ERROR,
                                        "PL/pgSQL output type is absent from the type catalog"
                                    )
                                })?,
                            local: local(parameter.name),
                        })
                        .map_err(control_flow::execution_full)?;
                }
                Ok(Self::OutputRecord(fields.freeze()))
            }
            _ => Err(changed()),
        }
    }

    fn finish<'a>(
        &self,
        execution: &execution_frame::ExecutionFrame<'_, 'a>,
        flow: Option<TriggerFlow<'a>>,
        result_type: ColType,
        (storage, txid): (&crate::storage::Storage, u32),
        arena: &'a Arena,
    ) -> Result<FunctionOutput<'a>, SqlError> {
        let output = match (self, flow) {
            (
                Self::Value | Self::OutputValue(_) | Self::OutputRecord(_),
                Some(TriggerFlow::Return(TriggerReturnValue::Value(value))),
            ) => Ok(FunctionOutput::Scalar(value)),
            (Self::Void, None | Some(TriggerFlow::Return(TriggerReturnValue::Void))) => {
                Ok(FunctionOutput::Scalar(Datum::Text("")))
            }
            (
                Self::OutputValue(local),
                None | Some(TriggerFlow::Return(TriggerReturnValue::Void)),
            ) => Ok(FunctionOutput::Scalar(
                local.map_or(Datum::Null, |index| execution.values[index]),
            )),
            (
                Self::OutputRecord(outputs),
                None | Some(TriggerFlow::Return(TriggerReturnValue::Void)),
            ) => {
                let fields = arena
                    .alloc_slice_with(outputs.len(), |_| RecordField {
                        name: "",
                        type_oid: 0,
                        value: Datum::Null,
                    })
                    .map_err(|_| super::super::query::arena_full_pub())?;
                for (field, output) in fields.iter_mut().zip(outputs.iter()) {
                    *field = RecordField {
                        name: arena
                            .alloc_str(output.name.as_str())
                            .map_err(|_| super::super::query::arena_full_pub())?,
                        type_oid: output.type_oid,
                        value: output
                            .local
                            .map_or(Datum::Null, |index| execution.values[index]),
                    };
                }
                Ok(FunctionOutput::Scalar(Datum::Record(fields)))
            }
            (Self::Rows, None | Some(TriggerFlow::Return(TriggerReturnValue::Void))) => {
                Ok(FunctionOutput::Rows(
                    execution
                        .status
                        .set_result
                        .as_ref()
                        .expect("set invocation owns its result collector")
                        .rows(arena)?,
                ))
            }
            (Self::Value, None) => Err(sql_err!(
                sqlstate::FUNCTION_EXECUTED_NO_RETURN_STATEMENT,
                "control reached end of function without RETURN"
            )),
            _ => Err(unsupported_trigger_body()),
        }?;
        match output {
            FunctionOutput::Scalar(value) => Ok(FunctionOutput::Scalar(cast_to(
                super::plpgsql_conversion_input(
                    detach_routine_datum(value, arena)?,
                    storage.routine_type_oid(result_type, None, txid),
                    storage,
                    txid,
                    arena,
                )?,
                result_type,
                arena,
            )?)),
            FunctionOutput::Rows(rows) => Ok(FunctionOutput::Rows(rows)),
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) struct FunctionOwner {
    identity: super::super::ExecutionIdentity,
    transaction: u32,
    command: u32,
    depth: u16,
}

impl FunctionOwner {
    pub(super) fn capture(context: &TriggerExecContext<'_, '_, '_>) -> Self {
        Self {
            identity: super::super::execution_identity(),
            transaction: context.txn.txid,
            command: context.txn.command_id(),
            depth: context.txn.trigger_depth(),
        }
    }
}

struct FunctionBinding {
    schema: SqlName,
    name: SqlName,
    oid: i32,
    version: u64,
    kind: PlpgsqlProgramKind,
    input_owner: super::super::type_input_plans::Owner,
    input_collation: crate::sql::ast::Collation,
    variable_conflict: super::super::guc::VariableConflict,
    print_strict_params: bool,
    fresh_snapshots: bool,
    parameters: SharedSlice<RoutineArgumentDef>,
    inputs: local_values::SavedValues,
    returns: ReturnBinding,
    result_type: ColType,
    environment: SavedCallEnvironment,
    owner: FunctionOwner,
    boundary: super::super::txn::StatementMark,
    type_sources: Option<crate::storage::StatementFileLease>,
}

pub(crate) enum SuspendedFunction {
    Plpgsql(SuspendedPlpgsqlFunction),
    Sql(super::sql_function_execution::SuspendedSqlFunction),
}

impl SuspendedFunction {
    pub(crate) fn oid(&self) -> i32 {
        match self {
            Self::Plpgsql(call) => call.oid(),
            Self::Sql(call) => call.oid(),
        }
    }
    pub(crate) fn is_set_returning(&self) -> bool {
        match self {
            Self::Plpgsql(call) => call.is_set_returning(),
            Self::Sql(call) => call.is_set_returning(),
        }
    }
    #[expect(
        clippy::too_many_arguments,
        reason = "saved calls resume against their original execution services"
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
        match self {
            Self::Plpgsql(call) => call.poll(
                engine,
                transaction,
                prepared,
                cursors,
                settings,
                arena,
                responder,
            ),
            Self::Sql(call) => call.poll(
                engine,
                transaction,
                prepared,
                cursors,
                settings,
                arena,
                responder,
            ),
        }
    }
}

pub(crate) struct SuspendedPlpgsqlFunction {
    state: MutableSlice<SuspendedFunctionState>,
}

struct SuspendedFunctionState {
    binding: FunctionBinding,
    execution: Option<execution_frame::SuspendedExecutionFrame>,
    progress: super::super::sequence::SavedStatementPosition,
}

impl FunctionBinding {
    fn invocation<'a>(
        &self,
        input_sites: &'a [super::super::plpgsql_inputs::Site<'a>],
        arena: &'a Arena,
    ) -> Result<TriggerInvocation<'a>, SqlError> {
        Ok(TriggerInvocation {
            program_kind: self.kind,
            name: "",
            routine_schema: arena
                .alloc_str(self.schema.as_str())
                .map_err(|_| super::super::query::arena_full_pub())?,
            routine_name: arena
                .alloc_str(self.name.as_str())
                .map_err(|_| super::super::query::arena_full_pub())?,
            table_schema: "",
            table_name: "",
            relid: 0,
            routine_oid: self.oid,
            input_origin: super::super::plpgsql_inputs::Origin::Routine {
                oid: self.oid,
                version: self.version,
            },
            input_owner: self.input_owner,
            input_collation: self.input_collation,
            input_sites,
            variable_conflict: self.variable_conflict,
            print_strict_params: self.print_strict_params,
            fresh_snapshots: self.fresh_snapshots,
            materialized_relations: None,
            nargs: i32::try_from(self.parameters.len()).expect("bounded routine argument count"),
            when: "",
            level: "",
            operation: "",
            argv: Datum::Null,
            event: "",
            tag: "",
            exception: None,
        })
    }
}

impl SuspendedPlpgsqlFunction {
    pub(crate) fn oid(&self) -> i32 {
        self.state[0].binding.oid
    }
    pub(crate) fn is_set_returning(&self) -> bool {
        self.state[0].binding.kind == PlpgsqlProgramKind::SetFunction
    }

    #[expect(
        clippy::too_many_arguments,
        reason = "a saved function attaches its original call to the caller's execution services"
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
            || !std::sync::Arc::ptr_eq(
                self.state[0].binding.inputs.pool(),
                &engine.storage.row_work,
            )
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
        if state.binding.owner != FunctionOwner::capture(&context) {
            return Err(changed());
        }
        let prior = state.progress.take();
        let frame = state.execution.take().ok_or_else(changed)?;
        let attached =
            super::super::type_input_plans::with_owner(state.binding.input_owner, || {
                prior.with_resume(
                    frame,
                    super::super::lock::AdvisoryPositionTail::Unchanged,
                    |frame, admission| state.resume_body(frame, admission, &mut context),
                )
            });
        match attached {
            Ok(Ok(result)) => result,
            Ok(Err((frame, error))) | Err((frame, error)) => {
                state.execution = Some(frame);
                state.progress = prior;
                Err(error)
            }
        }
    }
}

impl SuspendedFunctionState {
    #[expect(
        clippy::result_large_err,
        reason = "failed attachment returns the original function execution without allocation"
    )]
    fn resume_body<'a>(
        &mut self,
        frame: execution_frame::SuspendedExecutionFrame,
        admission: &mut super::super::sequence::StatementPositionAdmission<'_>,
        context: &mut TriggerExecContext<'_, 'a, '_>,
    ) -> Result<
        Result<FunctionOutput<'a>, SqlError>,
        (execution_frame::SuspendedExecutionFrame, SqlError),
    > {
        let arena = context.arena;
        let _types = match self
            .binding
            .type_sources
            .as_ref()
            .map(|sources| sources.enter())
            .transpose()
        {
            Ok(scope) => scope,
            Err(error) => return Err((frame, error)),
        };
        let environment = match self.binding.environment.enter(context.txn) {
            Ok(environment) => environment,
            Err(error) => return Err((frame, error)),
        };
        let (parsed, code) = match frame.source().parse(arena, None) {
            Ok(parsed) => parsed,
            Err(error) => return Err((frame, error)),
        };
        let inputs = match self.binding.inputs.decode(arena) {
            Ok(inputs) => inputs,
            Err(error) => return Err((frame, error)),
        };
        let invocation = match self.binding.invocation(parsed.input_sites, arena) {
            Ok(invocation) => invocation,
            Err(error) => return Err((frame, error)),
        };
        let catalog =
            super::super::query::storage_catalog(context.storage(), arena, context.txn.txid);
        let _formal = match enter_routine_parameter_types(
            &self.binding.parameters,
            Some(&catalog),
            self.binding.input_collation,
        ) {
            Ok(formal) => formal,
            Err(error) => return Err((frame, error)),
        };
        let seeded = match seeded_arguments(&self.binding.parameters, inputs, arena) {
            Ok(seeded) => seeded,
            Err(error) => return Err((frame, error)),
        };
        let mut execution = frame.restore(arena, &code)?;
        admission.commit();
        context.params = inputs;
        let result = run_body(
            &mut execution,
            &code,
            invocation,
            context,
            seeded,
            Some(self.binding.boundary),
        );
        if let Err(error) = admission.preserve_caller_inputs() {
            return Ok(Err(error));
        }
        match result {
            Ok(flow) => {
                let output = control_flow::at_routine_exit(context, invocation, || {
                    self.binding.returns.finish(
                        &execution,
                        flow,
                        self.binding.result_type,
                        (context.storage(), context.txn.txid),
                        arena,
                    )
                });
                self.binding.environment.complete();
                Ok(output)
            }
            Err(error) if control_flow::is_execution_wait(&error) => {
                let captured = SavedCallEnvironment::capture(
                    &environment,
                    &context.storage().execution_control,
                )
                .and_then(|environment| {
                    execution
                        .suspend()
                        .map(|frame| (frame, environment))
                        .map_err(|(_, error)| error)
                });
                match captured {
                    Ok((frame, environment)) => {
                        self.binding.environment = environment;
                        self.execution = Some(frame);
                        match super::super::sequence::SavedStatementPosition::capture() {
                            Ok(progress) => self.progress = progress,
                            Err(error) => return Ok(Err(error)),
                        }
                        Ok(Err(error))
                    }
                    Err(error) => Ok(Err(error)),
                }
            }
            Err(error) => Ok(Err(error)),
        }
    }
}

fn run_body<'a>(
    execution: &mut execution_frame::ExecutionFrame<'_, 'a>,
    code: &CompiledProgram<'a>,
    invocation: TriggerInvocation<'a>,
    context: &mut TriggerExecContext<'_, 'a, '_>,
    seeded: &[Datum<'a>],
    boundary: Option<super::super::txn::StatementMark>,
) -> Result<Option<TriggerFlow<'a>>, SqlError> {
    let prior = context.txn.write_completion_boundary;
    if let Some(mark) = boundary {
        context.txn.write_completion_boundary = Some(super::super::txn::WriteCompletionBoundary {
            mark,
            command: context.txn.command_id(),
            depth: context.txn.trigger_depth(),
        });
    }
    let definition = crate::storage::RelationDefinition::EMPTY;
    let result = (|| {
        execution.initialize(
            context,
            invocation,
            TriggerTransition {
                definition,
                old: None,
                new: None,
            },
            code,
            seeded,
        )?;
        execution.execute_block(
            context, definition, invocation, None, &mut None, false, None, code,
        )
    })();
    context.txn.write_completion_boundary = prior;
    result
}

fn seeded_arguments<'a>(
    parameters: &[RoutineArgumentDef],
    inputs: &[Datum<'a>],
    arena: &'a Arena,
) -> Result<&'a [Datum<'a>], SqlError> {
    let count = parameters
        .iter()
        .filter(|parameter| !parameter.name.as_str().is_empty())
        .count();
    let seeded = arena
        .alloc_slice_with(count, |_| Datum::Null)
        .map_err(|_| super::super::query::arena_full_pub())?;
    for (destination, (_, value)) in seeded.iter_mut().zip(
        parameters
            .iter()
            .zip(inputs)
            .filter(|(parameter, _)| !parameter.name.as_str().is_empty()),
    ) {
        *destination = *value;
    }
    Ok(seeded)
}

fn changed() -> SqlError {
    sql_err!(
        sqlstate::SERIALIZATION_FAILURE,
        "saved function belongs to another execution or command"
    )
}

#[expect(
    clippy::too_many_arguments,
    reason = "a function starts with its explicitly selected contract, environment and continuation policy"
)]
pub(crate) fn execute<'a>(
    engine: &mut super::super::Engine,
    transaction: &mut TxnState,
    prepared: &mut super::super::prep::SqlPreparedPool,
    cursors: &mut super::super::cursor::CursorPool,
    settings: &super::super::guc::GucState,
    routine: &crate::storage::RoutineDef,
    collation: crate::sql::ast::Collation,
    inputs: &[Datum<'a>],
    arena: &'a Arena,
    responder: &mut Responder<'_>,
    environment: &CallEnvironment,
    policy: super::super::query::RoutineExecution,
    suspended: &mut Option<SuspendedFunction>,
    compiled_conflict: super::super::guc::PlpgsqlSettings,
) -> Result<FunctionOutput<'a>, SqlError> {
    let inputs = arena
        .alloc_slice_copy(inputs)
        .map_err(|_| super::super::query::arena_full_pub())?;
    let kind = plpgsql_function_program_kind(routine);
    let (program, code) = compile_plpgsql_execution(
        routine.body.as_str(),
        kind,
        &engine.storage,
        transaction.txid,
        arena,
        &|name| {
            routine
                .parameters()
                .iter()
                .any(|parameter| parameter.name == name)
                || routine.result_columns[..routine.result_column_count]
                    .iter()
                    .any(|column| column.name == name)
        },
    )?;
    let set_returning = kind == PlpgsqlProgramKind::SetFunction;
    let inputs_named = routine
        .arguments()
        .iter()
        .filter(|argument| !argument.name.as_str().is_empty())
        .count();
    let output_count = if set_returning {
        routine.result_column_count
    } else {
        0
    };
    let output_names = if set_returning {
        output_count
    } else {
        routine
            .parameters()
            .iter()
            .filter(|parameter| parameter.mode.is_output())
            .count()
    };
    let capacity = inputs_named
        .checked_add(output_names)
        .and_then(|count| count.checked_add(program.locals.len()))
        .ok_or_else(super::super::query::arena_full_pub)?;
    let declarations = arena
        .alloc_slice_with(capacity, |_| empty_plpgsql_local())
        .map_err(|_| super::super::query::arena_full_pub())?;
    let mut seeded =
        allocate_plpgsql_local_values(&engine.storage.execution_control, inputs_named)?;
    let mut count = 0;
    for (index, argument) in routine.arguments().iter().enumerate() {
        if argument.name.as_str().is_empty() {
            continue;
        }
        declarations[count] = TriggerLocalDecl {
            label: Some(routine.name),
            scope: PlpgsqlScope::Invocation,
            name: argument.name,
            ctype: argument.ctype,
            user_type: argument.user_type,
            type_mod: -1,
            record_shape: None,
            constant: false,
            not_null: false,
        };
        seeded[count] = inputs[index];
        count += 1;
    }
    let seeded_count = count;
    let mut add_output = |output: RoutineArgumentDef| {
        if output.name.as_str().is_empty()
            || declarations[..count]
                .iter()
                .any(|prior| prior.name == output.name)
        {
            return;
        }
        declarations[count] = TriggerLocalDecl {
            label: Some(routine.name),
            scope: PlpgsqlScope::Invocation,
            name: output.name,
            ctype: output.ctype,
            user_type: output.user_type,
            type_mod: -1,
            record_shape: None,
            constant: false,
            not_null: false,
        };
        count += 1;
    };
    if set_returning {
        for output in &routine.result_columns[..output_count] {
            add_output(*output);
        }
    } else {
        for output in routine
            .parameters()
            .iter()
            .filter(|parameter| parameter.mode.is_output())
        {
            add_output(RoutineArgumentDef {
                name: output.name,
                ctype: output.ctype,
                user_type: output.user_type,
            });
        }
    }
    for local in program.locals {
        declarations[count] = *local;
        count += 1;
    }
    let locals = &declarations[..count];
    // Select declared types before any initializer or body effect can run.
    let type_sources = engine.storage.select_type_definition_graph(
        routine
            .arguments()
            .iter()
            .filter_map(|argument| argument.user_type)
            .chain(locals.iter().filter_map(|local| local.user_type))
            .chain(
                routine.result_columns[..routine.result_column_count]
                    .iter()
                    .filter_map(|output| output.user_type),
            )
            .chain(
                routine
                    .function_result_contract()
                    .and_then(|result| result.user_type),
            ),
        transaction.txid,
        arena,
    )?;
    let _types = type_sources
        .as_ref()
        .map(|sources| sources.enter())
        .transpose()?;
    let returns = ReturnBinding::select(routine, kind, locals, &engine.storage, transaction.txid)?;
    let invocation = TriggerInvocation::function(
        routine,
        kind,
        collation,
        program.settings(compiled_conflict),
        arena,
    )?
    .with_input_sites(program.input_sites);
    let mut execution = execution_frame::ExecutionFrame::new(
        arena,
        &code,
        locals,
        output_count,
        &engine.storage.execution_control,
        &engine.storage.row_work,
    )?;
    if set_returning {
        execution
            .status
            .collect_set_rows(PlpgsqlSetResult::for_routine(
                routine,
                &engine.storage.row_work,
            )?);
        let indices = arena
            .alloc_slice_with(output_count, |_| 0)
            .map_err(|_| super::super::query::arena_full_pub())?;
        for (index, output) in routine.result_columns[..output_count].iter().enumerate() {
            indices[index] = locals
                .iter()
                .position(|local| local.name == output.name)
                .filter(|_| !output.name.as_str().is_empty())
                .ok_or_else(|| {
                    sql_err!(
                        sqlstate::INVALID_FUNCTION_DEFINITION,
                        "set-returning output \"{}\" needs a name",
                        output.name.as_str()
                    )
                })?;
        }
        execution
            .status
            .set_output_locals(indices, execution.values.len());
    }
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
        params: inputs,
        responder,
    };
    let retained_boundary =
        (policy == super::super::query::RoutineExecution::Retained).then_some(boundary);
    let result = run_body(
        &mut execution,
        &code,
        invocation,
        &mut context,
        &seeded[..seeded_count],
        retained_boundary,
    );
    drop(seeded);
    match result {
        Ok(flow) => control_flow::at_routine_exit(&context, invocation, || {
            returns.finish(
                &execution,
                flow,
                routine.kind.function_result().ok_or_else(changed)?,
                (context.storage(), context.txn.txid),
                arena,
            )
        }),
        Err(error) if control_flow::is_execution_wait(&error) => {
            if policy == super::super::query::RoutineExecution::StatementReplay {
                return Err(error);
            }
            let storage = context.storage();
            let mut state = storage
                .execution_control
                .lease(1)
                .map_err(control_flow::execution_full)?;
            let parameters = storage
                .execution_control
                .copy_slice(routine.arguments())
                .map_err(control_flow::execution_full)?;
            let mut saved_inputs = local_values::SavedValues::new(&storage.row_work)?;
            saved_inputs.replace(inputs)?;
            let environment =
                SavedCallEnvironment::capture(environment, &storage.execution_control)?;
            let owner = FunctionOwner::capture(&context);
            let progress = super::super::sequence::SavedStatementPosition::capture()?;
            let execution = execution.suspend().map_err(|(_, error)| error)?;
            state
                .push_reserved(SuspendedFunctionState {
                    binding: FunctionBinding {
                        schema: routine.schema,
                        name: routine.name,
                        oid: crate::storage::routine_oid(routine),
                        version: routine.version,
                        kind,
                        input_owner: invocation.input_owner,
                        input_collation: collation,
                        variable_conflict: invocation.variable_conflict,
                        print_strict_params: invocation.print_strict_params,
                        fresh_snapshots: invocation.fresh_snapshots,
                        parameters,
                        inputs: saved_inputs,
                        returns,
                        result_type: routine.kind.function_result().ok_or_else(changed)?,
                        environment,
                        owner,
                        boundary,
                        type_sources,
                    },
                    execution: Some(execution),
                    progress,
                })
                .map_err(control_flow::execution_full)?;
            *suspended = Some(SuspendedFunction::Plpgsql(SuspendedPlpgsqlFunction {
                state,
            }));
            Err(error)
        }
        Err(error) => Err(error),
    }
}
