//! A firing keeps its lexical execution and row context under one owner.

use super::*;
use crate::mem::lease_pool::SharedSlice;
use crate::storage::{DefinitionImageId, RelationDefinition, TableDefinitionImages};

pub(super) struct FiringBinding<'values, 'schema, 'rows> {
    pub(super) invocation: TriggerInvocation<'values>,
    pub(super) definition: RelationDefinition<'schema>,
    pub(super) old: Option<&'rows [Datum<'values>]>,
    pub(super) new: Option<&'rows mut [Datum<'values>]>,
    pub(super) level: TriggerLevel,
    pub(super) before: bool,
}

impl FiringBinding<'_, '_, '_> {
    fn validate(&self) -> Result<(), SqlError> {
        let level_matches = match self.level {
            TriggerLevel::Row => {
                self.invocation.level == "ROW" && (self.old.is_some() || self.new.is_some())
            }
            TriggerLevel::Statement => {
                self.invocation.level == "STATEMENT" && self.old.is_none() && self.new.is_none()
            }
        };
        if self.invocation.program_kind != PlpgsqlProgramKind::Trigger
            || self.invocation.exception.is_some()
            || !level_matches
            || self
                .old
                .is_some_and(|row| row.len() != self.definition.n_columns)
            || self
                .new
                .as_deref()
                .is_some_and(|row| row.len() != self.definition.n_columns)
        {
            return Err(invalid("invalid trigger firing row or exception binding"));
        }
        Ok(())
    }
}

pub(super) struct TriggerFiring<'values, 'schema, 'rows> {
    code: CompiledProgram<'values>,
    execution: execution_frame::ExecutionFrame<'values, 'values>,
    binding: FiringBinding<'values, 'schema, 'rows>,
    environment: FiringEnvironment,
    owner: FiringOwner,
}

pub(super) struct FiringEnvironment {
    config: Option<crate::sql::guc::RoutineConfigScope>,
    security: Option<crate::sql::eval::funcs::system::CurrentUserScope>,
    prior_clock: Option<crate::sql::datetime::StatementClock>,
    _deadline: Option<crate::sql::query::DeadlineScope>,
}

impl FiringEnvironment {
    pub(super) fn new(
        config: Option<crate::sql::guc::RoutineConfigScope>,
        security: Option<crate::sql::eval::funcs::system::CurrentUserScope>,
    ) -> Self {
        Self {
            config,
            security,
            prior_clock: None,
            _deadline: None,
        }
    }
}

impl Drop for FiringEnvironment {
    fn drop(&mut self) {
        if let Some(clock) = self.prior_clock.take() {
            crate::sql::datetime::restore_statement_clock(clock);
        }
    }
}

struct SavedEnvironment {
    security: Option<crate::storage::SessionIdentity>,
    config: Option<SharedSlice<crate::sql::guc::RoutineConfigImage>>,
    clock: crate::sql::datetime::StatementClock,
    deadline: i64,
}

impl SavedEnvironment {
    fn capture(
        environment: &FiringEnvironment,
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
            clock: crate::sql::datetime::stored_statement_clock(),
            deadline: crate::sql::query::statement_deadline(),
        })
    }

    fn enter(&self, txn: &TxnState) -> Result<FiringEnvironment, SqlError> {
        let security = self
            .security
            .map(crate::sql::eval::funcs::system::enter_current_user);
        let config = self
            .config
            .as_ref()
            .map(|image| image[0].enter())
            .transpose()?;
        let prior_clock = crate::sql::datetime::stored_statement_clock();
        let deadline = crate::sql::query::enter_deadline_scope();
        crate::sql::datetime::restore_statement_clock(self.clock);
        txn.restore_statement_deadline(self.deadline);
        Ok(FiringEnvironment {
            config,
            security,
            prior_clock: Some(prior_clock),
            _deadline: Some(deadline),
        })
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
struct FiringOwner {
    identity: super::super::ExecutionIdentity,
    transaction: u32,
}

impl<'values, 'schema, 'rows> TriggerFiring<'values, 'schema, 'rows> {
    pub(super) fn new(
        context: &TriggerExecContext<'_, 'values, '_>,
        code: CompiledProgram<'values>,
        locals: &'values [TriggerLocalDecl],
        binding: FiringBinding<'values, 'schema, 'rows>,
        environment: FiringEnvironment,
    ) -> Result<Self, SqlError> {
        binding.validate()?;
        if code.kind() != PlpgsqlProgramKind::Trigger {
            return Err(invalid("trigger firing requires a trigger program"));
        }
        Ok(Self {
            execution: execution_frame::ExecutionFrame::new(
                context.arena,
                &code,
                locals,
                0,
                &context.storage().execution_control,
                &context.storage().row_work,
            )?,
            code,
            binding,
            environment,
            owner: FiringOwner {
                identity: super::super::execution_identity(),
                transaction: context.txn.txid,
            },
        })
    }

    pub(super) fn execute(
        &mut self,
        context: &mut TriggerExecContext<'_, 'values, '_>,
    ) -> Result<bool, SqlError> {
        self.require_context(context)?;
        self.execution.initialize(
            context,
            self.binding.invocation,
            TriggerTransition {
                definition: self.binding.definition,
                old: self.binding.old,
                new: self.binding.new.as_deref(),
            },
            &self.code,
            &[],
        )?;
        let result = self.execution.execute_block(
            context,
            self.binding.definition,
            self.binding.invocation,
            self.binding.old,
            &mut self.binding.new,
            self.binding.before,
            self.binding.invocation.materialized_relations,
            &self.code,
        )?;
        match result {
            Some(TriggerFlow::Return(_)) if self.binding.level == TriggerLevel::Statement => {
                Ok(true)
            }
            Some(TriggerFlow::Return(result)) => match result {
                TriggerReturnValue::Null if self.binding.before => Ok(false),
                TriggerReturnValue::Old if self.binding.before => {
                    let Some(old) = self.binding.old else {
                        return Ok(false);
                    };
                    if let Some(new) = self.binding.new.as_deref_mut() {
                        new.copy_from_slice(old);
                    }
                    Ok(true)
                }
                TriggerReturnValue::New if self.binding.before && self.binding.new.is_none() => {
                    Ok(false)
                }
                _ => Ok(true),
            },
            None => Err(sql_err!(
                sqlstate::FUNCTION_EXECUTED_NO_RETURN_STATEMENT,
                "control reached end of trigger procedure without RETURN"
            )),
            Some(TriggerFlow::Exit(_) | TriggerFlow::Continue(_)) => {
                Err(unsupported_trigger_body())
            }
        }
    }

    #[expect(
        clippy::result_large_err,
        reason = "failed capture returns the live firing without allocating"
    )]
    pub(super) fn suspend(
        self,
        context: &TriggerExecContext<'_, 'values, '_>,
    ) -> Result<SuspendedTriggerFiring, (Self, SqlError)> {
        if let Err(error) = self.require_context(context) {
            return Err((self, error));
        }
        let saved = match SavedBinding::capture(&self.binding, &self.environment, context) {
            Ok(saved) => saved,
            Err(error) => return Err((self, error)),
        };
        let Self {
            code,
            execution,
            binding,
            environment,
            owner,
        } = self;
        match execution.suspend() {
            Ok(execution) => {
                drop(environment);
                Ok(SuspendedTriggerFiring {
                    saved,
                    state: FiringState::Paused(execution),
                })
            }
            Err((execution, error)) => Err((
                Self {
                    code,
                    execution,
                    binding,
                    environment,
                    owner,
                },
                error,
            )),
        }
    }

    fn require_context(&self, context: &TriggerExecContext<'_, '_, '_>) -> Result<(), SqlError> {
        if self.owner.identity != super::super::execution_identity()
            || self.owner.transaction != context.txn.txid
            || !std::sync::Arc::ptr_eq(
                self.execution.values.row_pool(),
                &context.storage().row_work,
            )
        {
            return Err(changed());
        }
        self.execution
            .status
            .control_frames
            .as_ref()
            .expect("one procedural frame owner")
            .require_workspace(context.arena)
    }
}

struct SavedBinding {
    metadata: SavedMetadata,
    values: local_values::SavedValues,
    environment: SavedEnvironment,
}

struct SavedMetadata {
    header: TriggerInvocation<'static>,
    definitions: TableDefinitionImages,
    definition: DefinitionImageId,
    old_width: Option<usize>,
    new_width: Option<usize>,
    aliases: [Option<SavedAlias>; 2],
    level: TriggerLevel,
    before: bool,
    identity: super::super::ExecutionIdentity,
    transaction: u32,
}

struct SavedAlias {
    name: SqlName,
    types: SharedSlice<(i32, i16, i32)>,
    rows: transition_rows::SavedTransitionRows,
}

const HEADER_VALUES: usize = 8;

fn capture_values(
    saved: &mut local_values::SavedValues,
    binding: &FiringBinding<'_, '_, '_>,
) -> Result<(), SqlError> {
    let input = binding.invocation;
    let header = [
        Datum::Text(input.name),
        Datum::Text(input.routine_schema),
        Datum::Text(input.routine_name),
        Datum::Text(input.table_schema),
        Datum::Text(input.table_name),
        Datum::Text(input.event),
        Datum::Text(input.tag),
        input.argv,
    ];
    let old = binding.old.unwrap_or(&[]);
    let new = binding.new.as_deref().unwrap_or(&[]);
    saved.replace_indexed(HEADER_VALUES + old.len() + new.len(), |index| {
        if index < HEADER_VALUES {
            header[index]
        } else if index - HEADER_VALUES < old.len() {
            old[index - HEADER_VALUES]
        } else {
            new[index - HEADER_VALUES - old.len()]
        }
    })
}

impl SavedBinding {
    fn capture(
        binding: &FiringBinding<'_, '_, '_>,
        environment: &FiringEnvironment,
        context: &TriggerExecContext<'_, '_, '_>,
    ) -> Result<Self, SqlError> {
        binding.validate()?;
        let storage = context.storage();
        let environment = SavedEnvironment::capture(environment, &storage.execution_control)?;
        let definitions = storage.table_definition_images();
        let definition = definitions.retain_relation(&binding.definition)?;
        let mut aliases = [None, None];
        if let Some(relations) = binding.invocation.materialized_relations {
            if relations.len() > aliases.len() {
                return Err(invalid("invalid trigger transition relation count"));
            }
            for (destination, (name, relation)) in aliases.iter_mut().zip(relations) {
                let columns = binding.definition.columns();
                if relation.column_names.len() != columns.len()
                    || relation.column_types.len() != columns.len()
                    || relation.column_collations.len() != columns.len()
                    || relation.definition.is_none_or(|definition| {
                        definition.columns() != columns
                            || definition.name.as_str() != *name
                            || definition.schema != SqlName::EMPTY
                    })
                    || columns.iter().enumerate().any(|(index, column)| {
                        relation.column_names[index] != column.name.as_str()
                            || relation.column_collations[index] != column.collation
                    })
                {
                    return Err(invalid("trigger transition relation schema changed"));
                }
                let crate::sql::ast::MaterializedCteSource::Inline(rows) = relation.source else {
                    return Err(invalid("trigger transition rows must be materialized"));
                };
                *destination = Some(SavedAlias {
                    name: SqlName::parse(name)?,
                    types: storage
                        .execution_control
                        .copy_slice(relation.column_types)
                        .map_err(control_flow::execution_full)?,
                    rows: transition_rows::SavedTransitionRows::capture(
                        TransitionRows {
                            old: rows,
                            new: &[],
                        },
                        &storage.row_work,
                    )?,
                });
            }
        }
        let input = binding.invocation;
        let header = TriggerInvocation {
            program_kind: input.program_kind,
            name: "",
            routine_schema: "",
            routine_name: "",
            table_schema: "",
            table_name: "",
            relid: input.relid,
            routine_oid: input.routine_oid,
            input_origin: input.input_origin,
            input_owner: input.input_owner,
            input_collation: input.input_collation,
            input_sites: &[],
            materialized_relations: None,
            nargs: input.nargs,
            when: input.when,
            level: input.level,
            operation: input.operation,
            argv: Datum::Null,
            event: "",
            tag: "",
            exception: None,
        };
        let mut values = local_values::SavedValues::new(&storage.row_work)?;
        capture_values(&mut values, binding)?;
        Ok(Self {
            metadata: SavedMetadata {
                header,
                definitions,
                definition,
                old_width: binding.old.map(|row| row.len()),
                new_width: binding.new.as_deref().map(|row| row.len()),
                aliases,
                level: binding.level,
                before: binding.before,
                identity: super::super::execution_identity(),
                transaction: context.txn.txid,
            },
            values,
            environment,
        })
    }
}

impl SavedMetadata {
    fn bind<'run>(
        &'run self,
        values: &'run mut [Datum<'run>],
        sites: &'run [super::super::plpgsql_inputs::Site<'run>],
        arena: &'run Arena,
    ) -> Result<FiringBinding<'run, 'run, 'run>, SqlError> {
        let definition = self
            .definitions
            .retained_relation(self.definition)
            .ok_or_else(changed)?;
        let width = HEADER_VALUES + self.old_width.unwrap_or(0) + self.new_width.unwrap_or(0);
        if values.len() != width {
            return Err(invalid("saved trigger firing value width changed"));
        }
        let (header_values, rows) = values.split_at_mut(HEADER_VALUES);
        let text = |index| match header_values[index] {
            Datum::Text(value) => Ok(value),
            _ => Err(invalid("saved trigger firing text changed type")),
        };
        let relation_count = self.aliases.iter().flatten().count();
        let materialized_relations = if relation_count == 0 {
            None
        } else {
            let relations = arena
                .alloc_slice_with(relation_count, |_| ("", None))
                .map_err(|_| super::super::query::arena_full_pub())?;
            for (output, alias) in relations.iter_mut().zip(self.aliases.iter().flatten()) {
                let name = arena
                    .alloc_str(alias.name.as_str())
                    .map_err(|_| super::super::query::arena_full_pub())?;
                let names = arena
                    .alloc_slice_with(definition.n_columns, |_| "")
                    .map_err(|_| super::super::query::arena_full_pub())?;
                for (name, column) in names.iter_mut().zip(definition.columns()) {
                    *name = arena
                        .alloc_str(column.name.as_str())
                        .map_err(|_| super::super::query::arena_full_pub())?;
                }
                let collations = arena
                    .alloc_slice_with(definition.n_columns, |index| {
                        definition.columns()[index].collation
                    })
                    .map_err(|_| super::super::query::arena_full_pub())?;
                let rows = alias.rows.restore(arena)?;
                let relation = arena
                    .alloc(crate::sql::ast::MaterializedCte {
                        column_names: names,
                        column_types: &alias.types,
                        column_collations: collations,
                        definition: Some(RelationDefinition {
                            schema: SqlName::EMPTY,
                            name: alias.name,
                            ..definition
                        }),
                        source: crate::sql::ast::MaterializedCteSource::Inline(rows.old),
                    })
                    .map_err(|_| super::super::query::arena_full_pub())?;
                *output = (&*name, Some(&*relation));
            }
            let relations = arena
                .alloc_slice_with(relation_count, |index| {
                    let (name, relation) = relations[index];
                    (
                        name,
                        relation.expect("restored trigger transition relation"),
                    )
                })
                .map_err(|_| super::super::query::arena_full_pub())?;
            Some(&*relations)
        };
        let invocation = TriggerInvocation {
            name: text(0)?,
            routine_schema: text(1)?,
            routine_name: text(2)?,
            table_schema: text(3)?,
            table_name: text(4)?,
            event: text(5)?,
            tag: text(6)?,
            argv: header_values[7],
            input_sites: sites,
            materialized_relations,
            ..self.header
        };
        let (old, new) = rows.split_at_mut(self.old_width.unwrap_or(0));
        Ok(FiringBinding {
            invocation,
            definition,
            old: self.old_width.map(|_| &*old),
            new: self.new_width.map(|_| new),
            level: self.level,
            before: self.before,
        })
    }
}

#[expect(
    clippy::large_enum_variant,
    reason = "paused execution stays inline in startup-owned work memory"
)]
enum FiringState {
    Paused(execution_frame::SuspendedExecutionFrame),
    Complete,
    Failed,
}

pub(super) struct SuspendedTriggerFiring {
    saved: SavedBinding,
    state: FiringState,
}

#[derive(Debug, PartialEq, Eq)]
pub(super) enum FiringProgress {
    Waiting,
    Complete { accepted: bool },
}

#[expect(
    clippy::large_enum_variant,
    reason = "failed attachment preserves the original frame without heap allocation"
)]
enum ResumeError {
    Unattached(execution_frame::SuspendedExecutionFrame, SqlError),
    Executed(SqlError),
}

impl SuspendedTriggerFiring {
    pub(super) fn poll(
        &mut self,
        context: &mut TriggerExecContext<'_, '_, '_>,
    ) -> Result<FiringProgress, SqlError> {
        if self.saved.metadata.identity != super::super::execution_identity()
            || self.saved.metadata.transaction != context.txn.txid
            || !std::sync::Arc::ptr_eq(self.saved.values.pool(), &context.storage().row_work)
        {
            return Err(changed());
        }
        let frame = match core::mem::replace(&mut self.state, FiringState::Failed) {
            FiringState::Paused(frame) => frame,
            state => {
                self.state = state;
                return Err(invalid("trigger firing is already finished"));
            }
        };
        let arena = context.arena;
        let mark = arena.mark();
        let result = resume(
            &self.saved.metadata,
            &mut self.saved.values,
            &mut self.saved.environment,
            frame,
            TriggerExecContext {
                host: context.host.reborrow(),
                txn: context.txn,
                arena,
                params: crate::sql::eval::NO_PARAMS,
                responder: context.responder,
            },
        );
        let result = match result {
            Ok((state, progress)) => {
                self.state = state;
                Ok(progress)
            }
            Err(ResumeError::Unattached(frame, error)) => {
                self.state = FiringState::Paused(frame);
                Err(error)
            }
            Err(ResumeError::Executed(error)) => Err(error),
        };
        // SAFETY: the poll returns only encoded owners or a terminal error.
        // Every temporary row, binding and active frame has left scope.
        unsafe { arena.rewind_to(mark) };
        result
    }

    pub(super) fn new_values<'out>(
        &self,
        arena: &'out Arena,
    ) -> Result<Option<&'out [Datum<'out>]>, SqlError> {
        if !matches!(self.state, FiringState::Complete) {
            return Err(invalid("trigger firing has not completed"));
        }
        let Some(width) = self.saved.metadata.new_width else {
            return Ok(None);
        };
        let values = self.saved.values.decode(arena)?;
        let start = HEADER_VALUES + self.saved.metadata.old_width.unwrap_or(0);
        Ok(Some(&values[start..start + width]))
    }
}

#[expect(
    clippy::result_large_err,
    reason = "unattached frames retain their original images without allocating"
)]
fn resume<'run>(
    metadata: &'run SavedMetadata,
    values: &mut local_values::SavedValues,
    environment: &mut SavedEnvironment,
    frame: execution_frame::SuspendedExecutionFrame,
    mut context: TriggerExecContext<'_, 'run, '_>,
) -> Result<(FiringState, FiringProgress), ResumeError> {
    let active_environment = match environment.enter(context.txn) {
        Ok(active) => active,
        Err(error) => return Err(ResumeError::Unattached(frame, error)),
    };
    let (parsed, code) = match frame.source().parse(context.arena) {
        Ok(parsed) => parsed,
        Err(error) => return Err(ResumeError::Unattached(frame, error)),
    };
    let decoded = match values.decode(context.arena) {
        Ok(decoded) => decoded,
        Err(error) => return Err(ResumeError::Unattached(frame, error)),
    };
    let binding = match metadata.bind(decoded, parsed.input_sites, context.arena) {
        Ok(binding) => binding,
        Err(error) => return Err(ResumeError::Unattached(frame, error)),
    };
    let execution = match frame.restore(context.arena, &code) {
        Ok(execution) => execution,
        Err((frame, error)) => return Err(ResumeError::Unattached(frame, error)),
    };
    values.clear();
    let mut firing = TriggerFiring {
        code,
        execution,
        binding,
        environment: active_environment,
        owner: FiringOwner {
            identity: metadata.identity,
            transaction: metadata.transaction,
        },
    };
    match firing.execute(&mut context) {
        Ok(accepted) => {
            capture_values(values, &firing.binding).map_err(ResumeError::Executed)?;
            environment.security = None;
            environment.config = None;
            Ok((FiringState::Complete, FiringProgress::Complete { accepted }))
        }
        Err(error) if control_flow::is_execution_wait(&error) => {
            let saved_environment = SavedEnvironment::capture(
                &firing.environment,
                &context.storage().execution_control,
            )
            .map_err(ResumeError::Executed)?;
            capture_values(values, &firing.binding).map_err(ResumeError::Executed)?;
            let frame = firing
                .execution
                .suspend()
                .map_err(|(_, error)| ResumeError::Executed(error))?;
            *environment = saved_environment;
            Ok((FiringState::Paused(frame), FiringProgress::Waiting))
        }
        Err(error) => Err(ResumeError::Executed(error)),
    }
}

fn changed() -> SqlError {
    sql_err!(
        sqlstate::SERIALIZATION_FAILURE,
        "saved trigger firing belongs to another execution"
    )
}

fn invalid(message: &str) -> SqlError {
    sql_err!(sqlstate::INTERNAL_ERROR, "{}", message)
}

#[cfg(test)]
mod ownership_tests;
