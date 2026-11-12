//! Ordered AFTER work and its single write-completion response.

use super::*;
use crate::sql::{query, sequence};

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
#[derive(Clone, Copy)]
pub(super) struct QueuedFiring {
    pub(super) index: u64,
    /// The trigger to run, or `None` for a foreign key check.
    pub(super) trigger_slot: Option<u16>,
    pub(super) table: u16,
    pub(super) event: u8,
    /// An AFTER STATEMENT trigger rather than a row event.
    pub(super) statement: bool,
}

pub(super) struct AfterRowCursor {
    depth: u16,
    command: u32,
    next: Option<u64>,
}

impl AfterRowCursor {
    /// The events of the statement at `depth` running as `command`.
    pub(super) fn new(depth: u16, command: u32) -> Self {
        Self {
            depth,
            command,
            next: None,
        }
    }

    /// Yields the event at `index` again next.
    fn revisit(&mut self, index: u64) {
        self.next = Some(index);
    }

    pub(super) fn next(
        &mut self,
        context: &mut TriggerExecContext<'_, '_, '_>,
    ) -> Result<Option<QueuedFiring>, SqlError> {
        context.txn.queue_all_pending_row_events()?;
        let mut cursor = self
            .next
            .unwrap_or_else(|| context.txn.trigger_events_from(self.command));
        while cursor < context.txn.trigger_events_end() {
            let index = cursor;
            let (event, next, fired) = context.txn.trigger_event(index)?;
            cursor = next;
            self.next = Some(next);
            if fired {
                continue;
            }
            let selected = match event.kind {
                crate::sql::txn::DeferredTriggerKind::AfterRow {
                    trigger_slot,
                    effective_table,
                    trigger_depth,
                    command,
                } if trigger_depth == self.depth && command == self.command => {
                    Some((Some(trigger_slot), effective_table, false))
                }
                crate::sql::txn::DeferredTriggerKind::AfterStatement {
                    trigger_slot,
                    effective_table,
                    trigger_depth,
                    command,
                } if trigger_depth == self.depth && command == self.command => {
                    Some((Some(trigger_slot), effective_table, true))
                }
                crate::sql::txn::DeferredTriggerKind::ForeignKey {
                    identity,
                    check,
                    effective_table,
                    trigger_depth,
                    command,
                    ..
                } if trigger_depth == self.depth
                    && command == self.command
                    && context.txn.with_sub_statement != Some((self.depth, self.command)) =>
                {
                    match constraints::foreign_key_event_mode(
                        context.host.storage(),
                        context.txn,
                        identity,
                        check,
                    ) {
                        None => {
                            context.txn.fire_trigger_event(index)?;
                            continue;
                        }
                        Some(crate::sql::ast::ConstraintMode::Deferred) => None,
                        Some(crate::sql::ast::ConstraintMode::Immediate) => {
                            Some((None, effective_table, false))
                        }
                    }
                }
                crate::sql::txn::DeferredTriggerKind::Constraint {
                    identity,
                    effective_table,
                    trigger_depth,
                    command,
                } if trigger_depth == self.depth && command == self.command => {
                    let Some(timing) = constraints::constraint_timing(
                        context.storage(),
                        identity,
                        context.txn.txid,
                    ) else {
                        context.txn.fire_trigger_event(index)?;
                        continue;
                    };
                    if context.txn.constraint_mode(identity, timing)
                        != crate::sql::ast::ConstraintMode::Immediate
                    {
                        None
                    } else if let crate::sql::txn::ConstraintIdentity::Trigger { slot, .. } =
                        identity
                    {
                        Some((Some(slot), effective_table, false))
                    } else {
                        None
                    }
                }
                _ => None,
            };
            if let Some((trigger_slot, table, statement)) = selected {
                return Ok(Some(QueuedFiring {
                    index,
                    trigger_slot,
                    table,
                    event: event.event,
                    statement,
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
    ) -> Result<Option<(u16, crate::storage::TriggerDef)>, SqlError> {
        let replication_role = crate::sql::guc::active_session_replication_role()?;
        while let Some((slot, trigger)) = context
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
        {
            self.last_name = Some(trigger.name_to(context.txn.txid));
            if !trigger
                .enabled_to(context.txn.txid)
                .fires_in(replication_role)
                || !matches!(trigger.level, TriggerLevel::Statement)
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
            return Ok(Some((slot as u16, trigger)));
        }
        Ok(None)
    }
}

enum AfterPhase {
    /// The statement's queued events in order: row triggers, foreign key
    /// checks and actions, AFTER STATEMENT triggers, and those its
    /// referential actions queued after them.
    Rows(AfterRowCursor),
    /// A top-level statement's last immediate constraint checks.
    Constraints,
    Reply,
    Replied,
}

#[derive(Clone, Copy)]
enum AfterFiring {
    Row(QueuedFiring),
    /// A top-level statement's immediate constraint checks, after the row
    /// events before its first AFTER STATEMENT trigger, as PostgreSQL
    /// queues them, and again after any events queued later.
    Constraints,
}

struct CompletionCursor {
    completion: WriteCompletion,
    reply: CompletionReply,
    phase: AfterPhase,
    /// The completion ends a top-level statement: it runs the immediate
    /// constraint checks.
    statement_end: bool,
    /// Its owner keeps it across a wait, so its reads yield again at each
    /// event: a top-level statement, or a write in a `CALL` or `DO` body.
    reopen_reads: bool,
    /// Row events ran since the constraint checks last ran.
    constraints_pending: bool,
}

pub(crate) struct SuspendedWriteCompletion {
    state: MutableSlice<WriteCompletionState>,
}

pub(super) struct PendingNestedWrite {
    command: Option<super::super::txn::SuspendedTriggerSql>,
    completion: SuspendedWriteCompletion,
    output: Option<NestedWriteOutput>,
}

pub(super) enum NestedWriteOutput {
    Rows {
        values: local_values::SavedValues,
        rows: u64,
        /// A dynamic statement's `USING` values, which a `STRICT` failure
        /// lists after the write resumes.
        arguments: local_values::SavedValues,
    },
    NoResult,
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
            output: None,
        }
    }

    pub(super) fn save_output(
        &mut self,
        values: Option<&[Datum<'_>]>,
        rows: u64,
        arguments: &[Datum<'_>],
        pool: &std::sync::Arc<LeasePool>,
    ) -> Result<(), SqlError> {
        if self.output.is_some() {
            return Err(sql_err!(
                sqlstate::INTERNAL_ERROR,
                "nested write output is already saved"
            ));
        }
        self.output = Some(match values {
            Some(values) => {
                let mut saved = local_values::SavedValues::new(pool)?;
                saved.replace(values)?;
                let mut saved_arguments = local_values::SavedValues::new(pool)?;
                saved_arguments.replace(arguments)?;
                NestedWriteOutput::Rows {
                    values: saved,
                    rows,
                    arguments: saved_arguments,
                }
            }
            None => NestedWriteOutput::NoResult,
        });
        Ok(())
    }

    pub(super) fn take_output(&mut self) -> Option<NestedWriteOutput> {
        self.output.take()
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
            retained: prior.is_some_and(|boundary| boundary.retained),
            statement_end: false,
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
    selected: Option<AfterFiring>,
    firing: Option<trigger_firing::SuspendedTriggerFiring>,
    target: crate::storage::TriggerTarget,
    definitions: crate::storage::TableDefinitionImages,
    transitions: transition_rows::SavedTransitionRowSets,
    identity: super::super::ExecutionIdentity,
    transaction: u32,
    command: u32,
    depth: u16,
    boundary: super::super::txn::StatementMark,
    /// The statement's sequence replay position where the selected event
    /// started: an event that runs again replays its own calls from here.
    event_replay: Option<sequence::StatementReplayStart>,
}

impl SuspendedWriteCompletion {
    fn capture(
        cursor: CompletionCursor,
        selected: AfterFiring,
        context: &mut TriggerExecContext<'_, '_, '_>,
        target: crate::storage::TriggerTarget,
        transitions: TransitionRowSets<'_>,
        boundary: super::super::txn::StatementMark,
    ) -> Result<Self, SqlError> {
        let mut state = context
            .storage()
            .execution_control
            .lease(1)
            .map_err(control_flow::execution_full)?;
        let definitions = context.storage().table_definition_images();
        let mut row_cursor =
            AfterRowCursor::new(context.txn.trigger_depth(), context.txn.command_id());
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
                transitions,
                identity: super::super::execution_identity(),
                transaction: context.txn.txid,
                command: context.txn.command_id(),
                depth: context.txn.trigger_depth(),
                boundary,
                event_replay: sequence::begin_replay_cursor()?,
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
        if self.firing.is_none()
            && self.selected.is_some()
            && let Err(error) = sequence::rewind_replay_cursor(self.event_replay)
        {
            return sql_fail(error);
        }
        loop {
            if let Some(firing) = &mut self.firing {
                if self.cursor.reopen_reads {
                    query::reopen_reads_after_kept_work();
                }
                match firing.poll(context) {
                    Ok(trigger_firing::FiringProgress::Waiting { control }) => {
                        return sql_fail(sql_err!(control, "unfinished trigger is waiting"));
                    }
                    Ok(trigger_firing::FiringProgress::Complete { .. }) => {
                        self.firing = None;
                        self.selected = None;
                    }
                    Err(error) => return sql_fail(error),
                }
            }
            // The selected event stays saved until it completes, so a wait
            // anywhere in it runs or resumes that same event again.
            let selected = match self.selected {
                Some(selected) => selected,
                None => match self.cursor.next(context) {
                    Ok(Some(selected)) => {
                        match sequence::begin_replay_cursor() {
                            Ok(replay) => {
                                self.selected = Some(selected);
                                self.event_replay = replay;
                            }
                            Err(error) => return sql_fail(error),
                        }
                        selected
                    }
                    Ok(None) => return self.cursor.complete(context.responder),
                    Err(error) => return sql_fail(error),
                },
            };
            if self.cursor.reopen_reads {
                query::reopen_reads_after_kept_work();
            }
            let mark = arena.mark();
            let result =
                run_selected_firing(context, &self.definitions, self.target, rows, selected);
            // Every firing has completed or transferred its values and lexical
            // execution into the returned owner before temporary syntax leaves.
            unsafe { arena.rewind_to(mark) };
            match result {
                Ok(None) => self.selected = None,
                Ok(Some((firing, wait))) => {
                    self.firing = Some(firing);
                    return sql_fail(wait);
                }
                // A wait before the firing could keep its own progress runs
                // the same event again on resume.
                Err(error) => return sql_fail(error),
            }
        }
    }
}

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
        retained: true,
        statement_end: true,
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
    target: crate::storage::TriggerTarget,
    transitions: TransitionRowSets<'run>,
    selected: AfterFiring,
) -> Result<Option<(trigger_firing::SuspendedTriggerFiring, SqlError)>, SqlError> {
    let mut context = context.for_trigger();
    let selected = match selected {
        AfterFiring::Constraints => {
            constraints::validate_deferred_constraints(
                context.host.storage(),
                context.txn,
                true,
                context.arena,
            )?;
            return Ok(None);
        }
        AfterFiring::Row(QueuedFiring {
            index,
            trigger_slot: None,
            ..
        }) => {
            constraints::fire_queued_foreign_key_event(&mut context, index)?;
            return Ok(None);
        }
        AfterFiring::Row(selected) => selected,
    };
    let (event, _, _) = context.txn.trigger_event(selected.index)?;
    let definition = definitions.definition(
        context.storage(),
        usize::from(selected.table),
        context.txn.txid,
    )?;
    let relation = crate::storage::RelationDefinition::from_definition(definition);
    let trigger_slot = selected
        .trigger_slot
        .ok_or_else(|| sql_err!(sqlstate::INTERNAL_ERROR, "a trigger event lost its trigger"))?;
    let trigger = context.storage().trigger(usize::from(trigger_slot));
    let own = (target == crate::storage::TriggerTarget::Table(selected.table))
        .then(|| transitions.for_event(event.event))
        .flatten();
    let rows = queued_transition_rows(&mut context, &trigger, selected, own)?;
    let (old, new) = if selected.statement {
        (None, None)
    } else {
        let mut schema = [ColType::Bool; MAX_COLUMNS];
        definition.schema(&mut schema);
        let mut decode = |tuple| -> Result<Option<&'run mut [Datum<'run>]>, SqlError> {
            let Some(tuple) = tuple else {
                return Ok(None);
            };
            let bytes = context.txn.trigger_event_tuple(tuple, context.arena)?;
            let values = context
                .arena
                .alloc_slice_with(definition.n_columns, |_| Datum::Null)
                .map_err(|_| super::super::query::arena_full_pub())?;
            rowenc::decode(bytes, &schema[..definition.n_columns], values)?;
            Ok(Some(values))
        };
        (decode(event.old)?, decode(event.new)?)
    };
    context.txn.fire_trigger_event(selected.index)?;
    if let crate::storage::TriggerFunction::Builtin(builtin) = trigger.function {
        super::fire_builtin_trigger(
            builtin,
            &trigger,
            event.event,
            old.as_deref(),
            new.as_deref(),
        )?;
        return Ok(None);
    }
    let mut firing = prepare_trigger_firing(
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
        rows,
    )?;
    match firing.execute(&mut context) {
        Ok(_) => Ok(None),
        Err(error) if control_flow::is_execution_wait(&error) => {
            let saved = firing
                .suspend(&context, definitions)
                .map_err(|(_, error)| error)?;
            Ok(Some((saved, error)))
        }
        Err(error) => Err(error),
    }
}

/// The transition rows the queued trigger `firing` sees: `own`, the rows of
/// the statement's own target, and the rows its referential actions and
/// data-modifying `WITH` sub-statements queued for the same table and
/// operation, as PostgreSQL keeps one transition table per table and
/// operation in a statement.
pub(super) fn queued_transition_rows<'a>(
    context: &mut TriggerExecContext<'_, 'a, '_>,
    trigger: &crate::storage::TriggerDef,
    firing: QueuedFiring,
    own: Option<TransitionRows<'a>>,
) -> Result<Option<TransitionRows<'a>>, SqlError> {
    if matches!(
        trigger.transition_tables,
        crate::storage::TriggerTransitionTables::None
    ) {
        return Ok(own);
    }
    // A trigger declaring transition tables sees them even when no row of
    // its table changed (a referential action that matched nothing).
    let empty = TransitionRows { old: &[], new: &[] };
    let (depth, command) = (context.txn.trigger_depth(), context.txn.command_id());
    let start = context.txn.trigger_events_from(command);
    if !context.txn.has_queued_transition_rows(start) {
        return Ok(Some(own.unwrap_or(empty)));
    }
    let matches = |kind: crate::sql::txn::DeferredTriggerKind, event: u8| {
        matches!(
            kind,
            crate::sql::txn::DeferredTriggerKind::Transition {
                effective_table,
                trigger_depth,
                command: queued_by,
            } if effective_table == firing.table
                && trigger_depth == depth
                && queued_by == command
        ) && event == firing.event
    };
    let (mut old_count, mut new_count) = (0usize, 0usize);
    let mut id = start;
    while id < context.txn.trigger_events_end() {
        let (event, next, _) = context.txn.trigger_event(id)?;
        if matches(event.kind, event.event) {
            old_count += usize::from(event.old.is_some());
            new_count += usize::from(event.new.is_some());
        }
        id = next;
    }
    let own = own.unwrap_or(empty);
    if old_count == 0 && new_count == 0 {
        return Ok(Some(own));
    }
    let arena = context.arena;
    let old = arena
        .alloc_slice_with(own.old.len() + old_count, |_| &[][..])
        .map_err(|_| super::super::query::arena_full_pub())?;
    let new = arena
        .alloc_slice_with(own.new.len() + new_count, |_| &[][..])
        .map_err(|_| super::super::query::arena_full_pub())?;
    old[..own.old.len()].copy_from_slice(own.old);
    new[..own.new.len()].copy_from_slice(own.new);
    let (mut old_at, mut new_at) = (own.old.len(), own.new.len());
    let mut id = start;
    while id < context.txn.trigger_events_end() {
        let (event, next, _) = context.txn.trigger_event(id)?;
        if matches(event.kind, event.event) {
            if let Some(tuple) = event.old {
                old[old_at] = context.txn.trigger_event_tuple(tuple, arena)?;
                old_at += 1;
            }
            if let Some(tuple) = event.new {
                new[new_at] = context.txn.trigger_event_tuple(tuple, arena)?;
                new_at += 1;
            }
        }
        id = next;
    }
    Ok(Some(TransitionRows { old, new }))
}

impl CompletionCursor {
    fn new(
        completion: WriteCompletion,
        reply: CompletionReply,
        depth: u16,
        command: u32,
        boundary: Option<super::super::txn::WriteCompletionBoundary>,
    ) -> Self {
        let statement_end = boundary.is_some_and(|boundary| boundary.statement_end);
        Self {
            completion,
            reply,
            phase: AfterPhase::Rows(AfterRowCursor::new(depth, command)),
            statement_end,
            reopen_reads: boundary
                .is_some_and(|boundary| boundary.statement_end || boundary.retained),
            constraints_pending: statement_end,
        }
    }

    fn next(
        &mut self,
        context: &mut TriggerExecContext<'_, '_, '_>,
    ) -> Result<Option<AfterFiring>, SqlError> {
        loop {
            match &mut self.phase {
                AfterPhase::Rows(cursor) => {
                    if let Some(firing) = cursor.next(context)? {
                        if firing.statement && self.constraints_pending {
                            cursor.revisit(firing.index);
                            self.constraints_pending = false;
                            return Ok(Some(AfterFiring::Constraints));
                        }
                        if !firing.statement && self.statement_end {
                            self.constraints_pending = true;
                        }
                        return Ok(Some(AfterFiring::Row(firing)));
                    }
                    self.phase = if self.constraints_pending {
                        AfterPhase::Constraints
                    } else {
                        AfterPhase::Reply
                    };
                }
                AfterPhase::Constraints => {
                    self.phase = AfterPhase::Reply;
                    self.constraints_pending = false;
                    return Ok(Some(AfterFiring::Constraints));
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

    /// Queues the AFTER STATEMENT triggers of `table` for this write's
    /// operations after its row events, with their `WHEN` conditions
    /// evaluated now, as PostgreSQL's `AfterTriggerSaveEvent` does. A write
    /// whose AFTER events another statement fires also queues its transition
    /// rows there.
    fn queue_statement_events<Definition: crate::storage::DefinitionBacking>(
        &self,
        context: &mut TriggerExecContext<'_, '_, '_>,
        table: u16,
        definition: &crate::storage::TableDefinition<Definition>,
        transitions: TransitionRowSets<'_>,
        deferred: bool,
    ) -> Result<(), SqlError> {
        context.txn.queue_all_pending_row_events()?;
        let target = crate::storage::TriggerTarget::Table(table);
        let relation = crate::storage::RelationDefinition::from_definition(definition);
        for (event, columns) in self.events().into_iter().flatten() {
            if deferred && let Some(rows) = transitions.for_event(event) {
                for index in 0..rows.old.len().max(rows.new.len()) {
                    context.txn.queue_transition_row(
                        table,
                        event,
                        rows.old.get(index).copied(),
                        rows.new.get(index).copied(),
                    )?;
                }
            }
            if !has_statement_triggers(context, target, event, false)? {
                continue;
            }
            let start = context.txn.begin_after_statement_triggers(table, event)?;
            let mut cursor = StatementTriggerCursor::new();
            while let Some((slot, trigger)) = cursor.next(context, target, event, false, columns)? {
                if row_trigger_when_passes(context, &trigger, relation, None, None)? {
                    context
                        .txn
                        .queue_after_statement_trigger(slot, table, event, columns)?;
                }
            }
            context
                .txn
                .finish_after_statement_triggers(table, event, start)?;
        }
        Ok(())
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
        let command = context.txn.command_id();
        // A referential action's write and a data-modifying `WITH`
        // sub-statement leave their AFTER events to the statement they
        // belong to.
        let deferred = context.txn.defers_after_events()
            || context.txn.with_sub_statement == Some((depth, command));
        let crate::storage::TriggerTarget::Table(table) = target else {
            return sql_fail(sql_err!(
                sqlstate::INTERNAL_ERROR,
                "a write completion targets a view"
            ));
        };
        if let Err(error) =
            self.queue_statement_events(&mut context, table, definition, transitions, deferred)
        {
            return sql_fail(error);
        }
        let boundary = context
            .txn
            .write_completion_boundary
            .filter(|boundary| boundary.command == command && boundary.depth == depth);
        let mut cursor = CompletionCursor::new(self, reply, depth, command, boundary);
        if deferred {
            cursor.phase = AfterPhase::Reply;
            return cursor.complete(context.responder);
        }
        if let Some(boundary) = boundary {
            let selected = match cursor.next(&mut context) {
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
            let firing = match cursor.next(&mut context) {
                Ok(Some(firing)) => firing,
                Ok(None) => break,
                Err(error) => return sql_fail(error),
            };
            let result = match firing {
                AfterFiring::Row(firing) => {
                    let own = (target == crate::storage::TriggerTarget::Table(firing.table))
                        .then(|| transitions.for_event(firing.event))
                        .flatten();
                    execute_queued_firing(&mut context, firing, own)
                }
                AfterFiring::Constraints => Err(sql_err!(
                    sqlstate::INTERNAL_ERROR,
                    "an inline write completion runs no constraint checks"
                )),
            };
            if let Err(error) = result {
                return sql_fail(error);
            }
        }
        cursor.complete(context.responder)
    }
}

/// Whether `target` has statement triggers for `event` at all, whatever
/// their column lists: PostgreSQL keeps their once-per-statement state then.
pub(super) fn has_statement_triggers(
    context: &TriggerExecContext<'_, '_, '_>,
    target: crate::storage::TriggerTarget,
    event: u8,
    before: bool,
) -> Result<bool, SqlError> {
    Ok(StatementTriggerCursor::new()
        .next(context, target, event, before, ColumnSet::all(MAX_COLUMNS))?
        .is_some())
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
                0,
                None,
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
                statement_end: false,
                reopen_reads: false,
                constraints_pending: false,
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
