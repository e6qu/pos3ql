//! Ordered AFTER work and its single write-completion response.

use super::*;

pub(super) enum WriteCompletion {
    Insert {
        rows: u64,
        conflict_update: Option<ColumnSet>,
    },
    Update {
        rows: u64,
        columns: ColumnSet,
    },
    Delete {
        rows: u64,
    },
    Merge {
        rows: u64,
        insert: bool,
        update: Option<ColumnSet>,
        delete: bool,
    },
}

pub(super) enum CompletionReply {
    Command,
    CountOnly,
}

/// The next event belongs to this statement's trigger depth. Its completion
/// marker is installed by the firing, before recursive trigger SQL can run.
pub(super) struct QueuedFiring {
    pub(super) index: usize,
    pub(super) trigger_slot: u16,
    pub(super) table: u16,
    pub(super) event: u8,
}

pub(super) struct AfterRowCursor {
    depth: u16,
    next: usize,
}

impl AfterRowCursor {
    pub(super) fn new(depth: u16) -> Self {
        Self { depth, next: 0 }
    }

    pub(super) fn next(
        &mut self,
        context: &mut TriggerExecContext<'_, '_, '_>,
    ) -> Result<Option<QueuedFiring>, SqlError> {
        while self.next < context.txn.deferred_trigger_events().len() {
            let index = self.next;
            self.next += 1;
            if context.txn.deferred_trigger_is_complete(index) {
                continue;
            }
            let event = context.txn.deferred_trigger_events()[index];
            let selected = match event.kind {
                crate::sql::txn::DeferredTriggerKind::AfterRow {
                    trigger_slot,
                    effective_table,
                    trigger_depth,
                } if trigger_depth == self.depth => Some((trigger_slot, effective_table)),
                crate::sql::txn::DeferredTriggerKind::Constraint {
                    identity,
                    effective_table,
                    trigger_depth,
                } if trigger_depth == self.depth => {
                    let Some(timing) = constraints::constraint_timing(
                        context.storage(),
                        identity,
                        context.txn.txid,
                    ) else {
                        context.txn.complete_deferred_trigger(index)?;
                        continue;
                    };
                    if context.txn.constraint_mode(identity, timing)
                        != crate::sql::ast::ConstraintMode::Immediate
                    {
                        None
                    } else if let crate::sql::txn::ConstraintIdentity::Trigger { slot, .. } =
                        identity
                    {
                        Some((slot, effective_table))
                    } else {
                        None
                    }
                }
                _ => None,
            };
            if let Some((trigger_slot, table)) = selected {
                return Ok(Some(QueuedFiring {
                    index,
                    trigger_slot,
                    table,
                    event: event.event,
                }));
            }
        }
        Ok(None)
    }
}

pub(super) struct StatementTriggerCursor {
    last_name: Option<SqlName>,
}

impl StatementTriggerCursor {
    pub(super) fn new() -> Self {
        Self { last_name: None }
    }

    pub(super) fn next(
        &mut self,
        context: &TriggerExecContext<'_, '_, '_>,
        target: crate::storage::TriggerTarget,
        event: u8,
        before: bool,
        columns: ColumnSet,
    ) -> Option<crate::storage::TriggerDef> {
        while let Some(trigger) = context
            .storage()
            .triggers_for_target(target, context.txn.txid)
            .filter(|(_, trigger)| {
                self.last_name
                    .is_none_or(|last| trigger.name_to(context.txn.txid).as_str() > last.as_str())
            })
            .min_by(|(_, left), (_, right)| {
                left.name_to(context.txn.txid)
                    .as_str()
                    .cmp(right.name_to(context.txn.txid).as_str())
            })
            .map(|(_, trigger)| trigger)
        {
            self.last_name = Some(trigger.name_to(context.txn.txid));
            if !(if context.txn.replication_apply {
                trigger.enabled_to(context.txn.txid).fires_for_replication()
            } else {
                trigger.enabled_to(context.txn.txid).fires_for_origin()
            }) || !matches!(trigger.level, TriggerLevel::Statement)
                || trigger.timing
                    != if before {
                        TriggerTiming::Before
                    } else {
                        TriggerTiming::After
                    }
                || !trigger.events.contains(event)
                || (event == TriggerEvents::UPDATE
                    && !trigger.update_columns.is_empty()
                    && !trigger.update_columns.intersects(columns))
            {
                continue;
            }
            return Some(trigger);
        }
        None
    }
}

enum AfterPhase {
    Rows(AfterRowCursor),
    Statements {
        event: usize,
        trigger: StatementTriggerCursor,
    },
    Reply,
    Replied,
}

#[expect(
    clippy::large_enum_variant,
    reason = "selected trigger definitions stay inline without heap allocation"
)]
enum AfterFiring {
    Row(QueuedFiring),
    Statement {
        trigger: crate::storage::TriggerDef,
        event: u8,
    },
}

enum SavedAfterFiring {
    Row(QueuedFiring),
    Statement {
        trigger: MutableSlice<crate::storage::TriggerDef>,
        event: u8,
    },
}

impl SavedAfterFiring {
    fn capture(
        firing: AfterFiring,
        controls: &std::sync::Arc<LeasePool>,
    ) -> Result<Self, SqlError> {
        match firing {
            AfterFiring::Row(row) => Ok(Self::Row(row)),
            AfterFiring::Statement { trigger, event } => {
                let mut saved = controls.lease(1).map_err(control_flow::execution_full)?;
                saved
                    .push_reserved(trigger)
                    .map_err(control_flow::execution_full)?;
                Ok(Self::Statement {
                    trigger: saved,
                    event,
                })
            }
        }
    }

    fn into_firing(self) -> AfterFiring {
        match self {
            Self::Row(row) => AfterFiring::Row(row),
            Self::Statement { trigger, event } => AfterFiring::Statement {
                trigger: trigger[0],
                event,
            },
        }
    }
}

struct CompletionCursor {
    completion: WriteCompletion,
    reply: CompletionReply,
    phase: AfterPhase,
}

pub(crate) struct SuspendedWriteCompletion {
    state: MutableSlice<WriteCompletionState>,
}

pub(super) struct PendingNestedWrite {
    command: Option<super::super::txn::SuspendedTriggerSql>,
    completion: SuspendedWriteCompletion,
}

impl PendingNestedWrite {
    pub(super) fn new(
        command: super::super::txn::SuspendedTriggerSql,
        mut completion: SuspendedWriteCompletion,
    ) -> Self {
        completion.state[0].cursor.reply = CompletionReply::CountOnly;
        Self {
            command: Some(command),
            completion,
        }
    }

    pub(super) fn poll(
        &mut self,
        context: &mut TriggerExecContext<'_, '_, '_>,
    ) -> Result<u64, SqlError> {
        let command = self.command.take().ok_or_else(|| {
            sql_err!(sqlstate::INTERNAL_ERROR, "nested write is already finished")
        })?;
        let mut command = match command.resume(context.txn) {
            Ok(command) => command,
            Err((command, error)) => {
                self.command = Some(command);
                return Err(error);
            }
        };
        let prior = command.write_completion_boundary;
        command.write_completion_boundary = Some(super::super::txn::WriteCompletionBoundary {
            mark: self.completion.boundary(),
            command: command.command_id(),
            depth: command.trigger_depth(),
        });
        let result = self.completion.poll(&mut TriggerExecContext {
            host: context.host.reborrow(),
            txn: &mut command,
            arena: context.arena,
            params: crate::sql::eval::NO_PARAMS,
            responder: context.responder,
        });
        command.write_completion_boundary = prior;
        let result = result.map_err(|_| {
            sql_err!(
                sqlstate::PROGRAM_LIMIT_EXCEEDED,
                "nested write response exceeded the output buffer"
            )
        })?;
        match result {
            Ok(()) => {
                drop(command);
                Ok(self.completion.state[0].cursor.completion.rows())
            }
            Err(error) if control_flow::is_execution_wait(&error) => {
                self.command = Some(command.suspend());
                Err(error)
            }
            Err(error) => {
                drop(command);
                if let PlpgsqlExecHost::Routine { engine, .. } = &mut context.host {
                    engine.rollback_waiting_statement(context.txn, self.completion.boundary());
                }
                Err(error)
            }
        }
    }
}

struct WriteCompletionState {
    cursor: CompletionCursor,
    selected: Option<SavedAfterFiring>,
    firing: Option<trigger_firing::SuspendedTriggerFiring>,
    target: crate::storage::TriggerTarget,
    definitions: crate::storage::TableDefinitionImages,
    definition: crate::storage::DefinitionImageId,
    transitions: transition_rows::SavedTransitionRowSets,
    identity: super::super::ExecutionIdentity,
    transaction: u32,
    command: u32,
    depth: u16,
    boundary: super::super::txn::StatementMark,
}

impl SuspendedWriteCompletion {
    fn capture(
        cursor: CompletionCursor,
        selected: AfterFiring,
        context: &mut TriggerExecContext<'_, '_, '_>,
        target: crate::storage::TriggerTarget,
        definition: crate::storage::RelationDefinition<'_>,
        transitions: TransitionRowSets<'_>,
        boundary: super::super::txn::StatementMark,
    ) -> Result<Self, SqlError> {
        let mut state = context
            .storage()
            .execution_control
            .lease(1)
            .map_err(control_flow::execution_full)?;
        let selected = SavedAfterFiring::capture(selected, &context.storage().execution_control)?;
        let definitions = context.storage().table_definition_images();
        let definition = definitions.retain_relation(&definition)?;
        let mut row_cursor = AfterRowCursor::new(context.txn.trigger_depth());
        while let Some(firing) = row_cursor.next(context)? {
            definitions.definition(
                context.storage(),
                usize::from(firing.table),
                context.txn.txid,
            )?;
        }
        let transitions = transition_rows::SavedTransitionRowSets::capture(
            transitions,
            &context.storage().row_work,
        )?;
        state
            .push_reserved(WriteCompletionState {
                cursor,
                selected: Some(selected),
                firing: None,
                target,
                definitions,
                definition,
                transitions,
                identity: super::super::execution_identity(),
                transaction: context.txn.txid,
                command: context.txn.command_id(),
                depth: context.txn.trigger_depth(),
                boundary,
            })
            .map_err(control_flow::execution_full)?;
        Ok(Self { state })
    }

    pub(crate) fn boundary(&self) -> super::super::txn::StatementMark {
        self.state[0].boundary
    }

    pub(super) fn poll(&mut self, context: &mut TriggerExecContext<'_, '_, '_>) -> Outcome {
        self.state[0].poll(context)
    }
}

impl WriteCompletionState {
    fn poll(&mut self, context: &mut TriggerExecContext<'_, '_, '_>) -> Outcome {
        if self.identity != super::super::execution_identity()
            || self.transaction != context.txn.txid
            || self.command != context.txn.command_id()
            || self.depth != context.txn.trigger_depth()
        {
            return sql_fail(sql_err!(
                sqlstate::SERIALIZATION_FAILURE,
                "unfinished write belongs to a different command"
            ));
        }
        let mut execution_context = TriggerExecContext {
            host: context.host.reborrow(),
            txn: context.txn,
            arena: context.arena,
            params: crate::sql::eval::NO_PARAMS,
            responder: context.responder,
        };
        let context = &mut execution_context;
        let arena = context.arena;
        let rows = match self.transitions.restore(arena) {
            Ok(rows) => rows,
            Err(error) => return sql_fail(error),
        };
        let definition = self
            .definitions
            .retained_relation(self.definition)
            .expect("unfinished write retains its selected relation");
        loop {
            if let Some(firing) = &mut self.firing {
                match firing.poll(context) {
                    Ok(trigger_firing::FiringProgress::Waiting { control }) => {
                        return sql_fail(sql_err!(control, "unfinished trigger is waiting"));
                    }
                    Ok(trigger_firing::FiringProgress::Complete { .. }) => self.firing = None,
                    Err(error) => return sql_fail(error),
                }
            }
            let selected = match self.selected.take() {
                Some(selected) => selected.into_firing(),
                None => match self.cursor.next(context, self.target) {
                    Ok(Some(selected)) => selected,
                    Ok(None) => return self.cursor.complete(context.responder),
                    Err(error) => return sql_fail(error),
                },
            };
            let mark = arena.mark();
            let result = run_selected_firing(
                context,
                &self.definitions,
                definition,
                self.target,
                rows,
                selected,
            );
            // Every firing has completed or transferred its values and lexical
            // execution into the returned owner before temporary syntax leaves.
            unsafe { arena.rewind_to(mark) };
            match result {
                Ok(None) => {}
                Ok(Some((firing, wait))) => {
                    self.firing = Some(firing);
                    return sql_fail(wait);
                }
                Err(error) => return sql_fail(error),
            }
        }
    }
}

#[expect(
    clippy::too_many_arguments,
    reason = "write resumption uses the original connection execution services"
)]
pub(in crate::sql) fn resume(
    engine: &mut super::super::Engine,
    txn: &mut TxnState,
    arena: &Arena,
    sqlprep: &mut super::super::prep::SqlPreparedPool,
    cursors: &mut super::super::cursor::CursorPool,
    guc: &super::super::guc::GucState,
    responder: &mut Responder<'_>,
) -> Outcome {
    let Some(mut completion) = txn.pending_write_completion.take() else {
        return sql_fail(sql_err!(
            sqlstate::INTERNAL_ERROR,
            "write resumption has no unfinished command"
        ));
    };
    let prior = txn.write_completion_boundary;
    txn.write_completion_boundary = Some(super::super::txn::WriteCompletionBoundary {
        mark: completion.boundary(),
        command: txn.command_id(),
        depth: txn.trigger_depth(),
    });
    let result = completion.poll(&mut TriggerExecContext {
        host: PlpgsqlExecHost::Routine {
            engine,
            guc,
            sqlprep,
            cursors,
            transaction_context: PlpgsqlTransactionContext::Atomic,
        },
        txn,
        arena,
        params: crate::sql::eval::NO_PARAMS,
        responder,
    });
    txn.write_completion_boundary = prior;
    if result.is_err()
        || matches!(&result, Ok(Err(error)) if control_flow::is_execution_wait(error))
    {
        txn.pending_write_completion = Some(completion);
    } else if matches!(&result, Ok(Err(_))) {
        let boundary = completion.boundary();
        drop(completion);
        engine.rollback_waiting_statement(txn, boundary);
    }
    result
}

fn run_selected_firing<'run>(
    context: &mut TriggerExecContext<'_, 'run, '_>,
    definitions: &'run crate::storage::TableDefinitionImages,
    definition: crate::storage::RelationDefinition<'run>,
    target: crate::storage::TriggerTarget,
    transitions: TransitionRowSets<'run>,
    selected: AfterFiring,
) -> Result<Option<(trigger_firing::SuspendedTriggerFiring, SqlError)>, SqlError> {
    let mut context = context.for_trigger();
    let firing = match selected {
        AfterFiring::Statement { trigger, event } => {
            if !row_trigger_when_passes(&context, &trigger, definition, None, None)? {
                return Ok(None);
            }
            prepare_trigger_firing(
                &context,
                &trigger,
                TriggerRelationIdentity {
                    target,
                    schema: definition.schema,
                    name: definition.name,
                },
                definition,
                event,
                false,
                None,
                None,
                transitions.for_event(event),
            )?
        }
        AfterFiring::Row(selected) => {
            let event = context.txn.deferred_trigger_events()[selected.index];
            let definition = definitions.definition(
                context.storage(),
                usize::from(selected.table),
                context.txn.txid,
            )?;
            let relation = crate::storage::RelationDefinition::from_definition(definition);
            let mut schema = [ColType::Bool; MAX_COLUMNS];
            definition.schema(&mut schema);
            let decode = |tuple| -> Result<Option<&'run mut [Datum<'run>]>, SqlError> {
                let Some(tuple) = tuple else {
                    return Ok(None);
                };
                let bytes = context
                    .arena
                    .alloc_slice_copy(context.txn.deferred_trigger_bytes(tuple))
                    .map_err(|_| super::super::query::arena_full_pub())?;
                let values = context
                    .arena
                    .alloc_slice_with(definition.n_columns, |_| Datum::Null)
                    .map_err(|_| super::super::query::arena_full_pub())?;
                rowenc::decode(bytes, &schema[..definition.n_columns], values)?;
                Ok(Some(values))
            };
            let old = decode(event.old)?;
            let new = decode(event.new)?;
            let trigger = context
                .storage()
                .trigger(usize::from(selected.trigger_slot));
            context.txn.complete_deferred_trigger(selected.index)?;
            prepare_trigger_firing(
                &context,
                &trigger,
                TriggerRelationIdentity {
                    target: crate::storage::TriggerTarget::Table(selected.table),
                    schema: relation.schema,
                    name: relation.name,
                },
                relation,
                event.event,
                false,
                old.map(|row| &*row),
                new,
                transitions.for_event(event.event),
            )?
        }
    };
    let mut firing = firing;
    match firing.execute(&mut context) {
        Ok(_) => Ok(None),
        Err(error) if control_flow::is_execution_wait(&error) => {
            let saved = firing.suspend(&context).map_err(|(_, error)| error)?;
            Ok(Some((saved, error)))
        }
        Err(error) => Err(error),
    }
}

impl CompletionCursor {
    fn new(completion: WriteCompletion, reply: CompletionReply, depth: u16) -> Self {
        Self {
            completion,
            reply,
            phase: AfterPhase::Rows(AfterRowCursor::new(depth)),
        }
    }

    fn next(
        &mut self,
        context: &mut TriggerExecContext<'_, '_, '_>,
        target: crate::storage::TriggerTarget,
    ) -> Result<Option<AfterFiring>, SqlError> {
        let events = self.completion.events();
        loop {
            match &mut self.phase {
                AfterPhase::Rows(cursor) => {
                    if events.iter().any(Option::is_some)
                        && let Some(firing) = cursor.next(context)?
                    {
                        return Ok(Some(AfterFiring::Row(firing)));
                    }
                    self.phase = AfterPhase::Statements {
                        event: 0,
                        trigger: StatementTriggerCursor::new(),
                    };
                }
                AfterPhase::Statements { event, trigger } => {
                    if *event == events.len() {
                        self.phase = AfterPhase::Reply;
                        continue;
                    }
                    if let Some((operation, columns)) = events[*event]
                        && let Some(selected) =
                            trigger.next(context, target, operation, false, columns)
                    {
                        return Ok(Some(AfterFiring::Statement {
                            trigger: selected,
                            event: operation,
                        }));
                    }
                    *event += 1;
                    *trigger = StatementTriggerCursor::new();
                }
                AfterPhase::Reply | AfterPhase::Replied => return Ok(None),
            }
        }
    }

    fn complete(&mut self, responder: &mut Responder<'_>) -> Outcome {
        if !matches!(self.phase, AfterPhase::Reply) {
            return sql_fail(sql_err!(
                sqlstate::INTERNAL_ERROR,
                "write completion is not awaiting its response"
            ));
        }
        let (rows, tag) = match &self.completion {
            WriteCompletion::Insert { rows, .. } => (*rows, stack_format!(48, "INSERT 0 {}", rows)),
            WriteCompletion::Update { rows, .. } => (*rows, stack_format!(48, "UPDATE {}", rows)),
            WriteCompletion::Delete { rows } => (*rows, stack_format!(48, "DELETE {}", rows)),
            WriteCompletion::Merge { rows, .. } => (*rows, stack_format!(48, "MERGE {}", rows)),
        };
        match self.reply {
            CompletionReply::CountOnly => responder.set_affected_rows(rows),
            CompletionReply::Command => responder.command_complete_rows(tag.as_str(), rows)?,
        }
        self.phase = AfterPhase::Replied;
        sql_ok()
    }
}

impl WriteCompletion {
    fn rows(&self) -> u64 {
        match self {
            Self::Insert { rows, .. }
            | Self::Update { rows, .. }
            | Self::Delete { rows }
            | Self::Merge { rows, .. } => *rows,
        }
    }
    fn events(&self) -> [Option<(u8, ColumnSet)>; 3] {
        match self {
            Self::Insert {
                conflict_update, ..
            } => [
                conflict_update.map(|columns| (TriggerEvents::UPDATE, columns)),
                Some((TriggerEvents::INSERT, ColumnSet::EMPTY)),
                None,
            ],
            Self::Update { columns, .. } => [Some((TriggerEvents::UPDATE, *columns)), None, None],
            Self::Delete { .. } => [Some((TriggerEvents::DELETE, ColumnSet::EMPTY)), None, None],
            Self::Merge {
                insert,
                update,
                delete,
                ..
            } => [
                delete.then_some((TriggerEvents::DELETE, ColumnSet::EMPTY)),
                update.map(|columns| (TriggerEvents::UPDATE, columns)),
                insert.then_some((TriggerEvents::INSERT, ColumnSet::EMPTY)),
            ],
        }
    }

    pub(super) fn finish<'values, Definition: crate::storage::DefinitionBacking>(
        self,
        mut context: TriggerExecContext<'_, 'values, '_>,
        target: crate::storage::TriggerTarget,
        definition: &crate::storage::TableDefinition<Definition>,
        transitions: TransitionRowSets<'values>,
        reply: CompletionReply,
    ) -> Outcome {
        let depth = context.txn.trigger_depth();
        let mut cursor = CompletionCursor::new(self, reply, depth);
        if let Some(boundary) = context.txn.write_completion_boundary
            && boundary.command == context.txn.command_id()
            && boundary.depth == depth
        {
            let selected = match cursor.next(&mut context, target) {
                Ok(Some(selected)) => selected,
                Ok(None) => return cursor.complete(context.responder),
                Err(error) => return sql_fail(error),
            };
            if context.txn.pending_write_completion.is_some() {
                return sql_fail(sql_err!(
                    sqlstate::INTERNAL_ERROR,
                    "one unfinished write owns the command completion"
                ));
            }
            let pending = match SuspendedWriteCompletion::capture(
                cursor,
                selected,
                &mut context,
                target,
                crate::storage::RelationDefinition::from_definition(definition),
                transitions,
                boundary.mark,
            ) {
                Ok(pending) => pending,
                Err(error) => return sql_fail(error),
            };
            context.txn.pending_write_completion = Some(pending);
            return sql_fail(sql_err!(
                sqlstate::INTERNAL_ROUTINE_INVOCATION,
                "write completion requires the caller's execution services"
            ));
        }
        loop {
            let firing = match cursor.next(&mut context, target) {
                Ok(Some(firing)) => firing,
                Ok(None) => break,
                Err(error) => return sql_fail(error),
            };
            let result = match firing {
                AfterFiring::Row(firing) => execute_deferred_trigger_event(
                    &mut context,
                    firing.index,
                    firing.trigger_slot,
                    firing.table,
                    transitions.for_event(firing.event),
                ),
                AfterFiring::Statement { trigger, event } => execute_statement_trigger(
                    &mut context,
                    &trigger,
                    target,
                    crate::storage::RelationDefinition::from_definition(definition),
                    event,
                    false,
                    transitions.for_event(event),
                ),
            };
            if let Err(error) = result {
                return sql_fail(error);
            }
        }
        cursor.complete(context.responder)
    }
}

#[cfg(test)]
mod ownership_tests {
    use super::*;

    #[test]
    fn completion_response_retains_count_across_full_buffer_and_move() {
        let mut budget = crate::mem::Budget::new(64);
        let mut buffer = crate::mem::FixedBuf::new(&mut budget, "completion response", 64).unwrap();
        crate::mem::guard::forbid_alloc(|| {
            let mut cursor = CompletionCursor::new(
                WriteCompletion::Insert {
                    rows: u64::MAX,
                    conflict_update: None,
                },
                CompletionReply::Command,
                0,
            );
            let mut responder = Responder::new(&mut buffer);
            assert!(responder.buffer.append(&[0xa5; 50]));
            responder.buffer.consume(20);
            assert!(
                responder
                    .data_row(&[Datum::Text("xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx")])
                    .is_err()
            );
            assert_eq!(responder.buffer.readable(), &[0xa5; 30]);
            responder.buffer.clear();
            responder.set_affected_rows(7);
            assert_eq!(
                cursor
                    .complete(&mut responder)
                    .unwrap()
                    .unwrap_err()
                    .sqlstate,
                sqlstate::INTERNAL_ERROR
            );
            assert!(responder.buffer.is_empty());
            cursor.phase = AfterPhase::Reply;
            assert!(responder.buffer.append(&[0xa5; 50]));
            assert!(cursor.complete(&mut responder).is_err());
            assert_eq!(responder.buffer.readable(), &[0xa5; 50]);
            assert_eq!(responder.take_affected_rows(), Some(7));
            assert!(matches!(cursor.phase, AfterPhase::Reply));
            let mut moved = cursor;
            responder.buffer.clear();
            moved.complete(&mut responder).unwrap().unwrap();
            let tag = b"INSERT 0 18446744073709551615\0";
            let bytes = responder.buffer.readable();
            assert_eq!(bytes[0], b'C');
            assert_eq!(&bytes[1..5], &((tag.len() + 4) as u32).to_be_bytes());
            assert_eq!(&bytes[5..], tag);
            assert_eq!(responder.take_affected_rows(), Some(u64::MAX));
            let completed_bytes = responder.buffer.len();
            assert_eq!(
                moved
                    .complete(&mut responder)
                    .unwrap()
                    .unwrap_err()
                    .sqlstate,
                sqlstate::INTERNAL_ERROR
            );
            assert_eq!(responder.buffer.len(), completed_bytes);
            assert_eq!(responder.take_affected_rows(), None);
            let mut nested = CompletionCursor {
                completion: WriteCompletion::Update {
                    rows: 13,
                    columns: ColumnSet::EMPTY,
                },
                reply: CompletionReply::CountOnly,
                phase: AfterPhase::Reply,
            };
            nested.complete(&mut responder).unwrap().unwrap();
            assert_eq!(responder.buffer.len(), completed_bytes);
            assert_eq!(responder.take_affected_rows(), Some(13));
            assert_eq!(
                nested
                    .complete(&mut responder)
                    .unwrap()
                    .unwrap_err()
                    .sqlstate,
                sqlstate::INTERNAL_ERROR
            );
            assert_eq!(responder.buffer.len(), completed_bytes);
        });
    }
}
