//! `MERGE` into a view through its `INSTEAD OF` row triggers, as PostgreSQL
//! runs a MERGE whose every action has such a trigger: the source joins the
//! view's rows, each candidate chooses its first applicable `WHEN` clause,
//! and the chosen action fires the view's trigger instead of writing a
//! relation. The rows the triggers return are the changed rows (`RETURNING`
//! reads them, and the count includes only them); a trigger returning NULL
//! skips its row. A view row can be acted on more than once: PostgreSQL has
//! no row identity to apply MERGE's cardinality check to.

use super::*;

/// Whether PostgreSQL's `view_has_instead_trigger` makes the view the MERGE's
/// result relation: every action that writes has an `INSTEAD OF` row trigger.
pub(super) fn merge_is_trigger_updatable(
    storage: &Storage,
    view: usize,
    statement: &crate::sql::ast::Merge<'_>,
    txid: u32,
) -> bool {
    statement.whens.iter().all(|when| {
        merge_action_event(when.action())
            .is_none_or(|event| view_has_instead_trigger(storage, view, event, txid))
    })
}

/// PostgreSQL's `rewriteTargetView` refuses a MERGE into an automatically
/// updatable view with `INSTEAD OF` triggers for some of its actions only.
pub(super) fn reject_partial_merge_triggers(
    storage: &Storage,
    view: usize,
    statement: &crate::sql::ast::Merge<'_>,
    txid: u32,
) -> Result<(), SqlError> {
    let partial = statement.whens.iter().any(|when| {
        merge_action_event(when.action())
            .is_some_and(|event| view_has_instead_trigger(storage, view, event, txid))
    });
    if !partial {
        return Ok(());
    }
    crate::sql::eval::stash_full_diagnostic(crate::sql::eval::Diagnostic {
        detail: Some(crate::util::StackStr::from_str(
            "MERGE is not supported for views with INSTEAD OF triggers for some actions but not all.",
        )),
        hint: Some(crate::util::StackStr::from_str(
            "To enable merging into the view, either provide a full set of INSTEAD OF triggers or drop the existing INSTEAD OF triggers.",
        )),
        objects: crate::sql::eval::ErrorObjects::default(),
    });
    Err(sql_err!(
        sqlstate::FEATURE_NOT_SUPPORTED,
        "cannot merge into view \"{}\"",
        storage.view(view).name_for(txid).as_str()
    ))
}

pub(super) fn view_has_instead_trigger(
    storage: &Storage,
    view: usize,
    event: u8,
    txid: u32,
) -> bool {
    storage.triggers_for_view(view, txid).any(|(_, trigger)| {
        matches!(trigger.level, TriggerLevel::Row)
            && matches!(trigger.timing, TriggerTiming::InsteadOf)
            && trigger.events.contains(event)
    })
}

/// The trigger event a MERGE action writes with, or `None` for `DO NOTHING`.
pub(super) fn merge_action_event(action: crate::sql::ast::MergeActionRef<'_>) -> Option<u8> {
    use crate::sql::ast::MergeActionRef as MergeAction;
    match action {
        MergeAction::Insert { .. } => Some(TriggerEvents::INSERT),
        MergeAction::Update(_) => Some(TriggerEvents::UPDATE),
        MergeAction::Delete => Some(TriggerEvents::DELETE),
        MergeAction::DoNothing => None,
    }
}

/// A growing list of encoded rows in the statement arena.
struct ArenaRows<'a> {
    rows: &'a mut [&'a [u8]],
    len: usize,
}

impl<'a> ArenaRows<'a> {
    fn new() -> Self {
        Self {
            rows: &mut [],
            len: 0,
        }
    }

    fn push(&mut self, arena: &'a Arena, row: &'a [u8]) -> Result<(), SqlError> {
        if self.len == self.rows.len() {
            let capacity = if self.len == 0 {
                8
            } else {
                self.len
                    .checked_mul(2)
                    .ok_or_else(crate::sql::query::arena_full_pub)?
            };
            let fresh = arena
                .alloc_slice_with(capacity, |_| &[][..])
                .map_err(|_| crate::sql::query::arena_full_pub())?;
            fresh[..self.len].copy_from_slice(&self.rows[..self.len]);
            self.rows = fresh;
        }
        self.rows[self.len] = row;
        self.len += 1;
        Ok(())
    }

    fn into_slice(self) -> &'a [&'a [u8]] {
        let rows: &'a [&'a [u8]] = self.rows;
        &rows[..self.len]
    }
}

/// The view a MERGE writes through its triggers, with what the views above
/// it (when it was reached through them) add: their filter over its rows,
/// their check options and defaults, and the authorization they write it
/// with.
#[derive(Clone, Copy)]
pub(super) struct TriggerMergeTarget<'a> {
    pub(super) view: usize,
    pub(super) above: Option<MergeViewTarget<'a>>,
}

#[allow(clippy::too_many_arguments, clippy::type_complexity)]
pub(super) fn merge_through_instead_triggers<'a>(
    storage: &mut Storage,
    txn: &mut TxnState,
    statement: &'a crate::sql::ast::Merge<'a>,
    target: TriggerMergeTarget<'a>,
    arena: &'a Arena,
    params: &[Datum<'a>],
    services: &mut TriggerServices<'_>,
    responder: &mut Responder,
    mut capture: Option<&mut crate::sql::ReturningCapture<'_>>,
    mut progress: Option<&mut RowProgress>,
    outer: Option<&dyn ColumnLookup<'a>>,
) -> Outcome {
    use crate::sql::ast::MergeActionRef as MergeAction;
    use crate::sql::ast::MergeMatchKind;
    let seq_session = services.seq_session;
    let view_slot = target.view;
    let trigger_target = crate::storage::TriggerTarget::View(view_slot as u16);
    let capturing = capture.is_some();
    let def = match view_trigger_definition(storage, view_slot, txn.txid, arena) {
        Ok(definition) => definition,
        Err(error) => return sql_fail(error),
    };
    let (update_columns, insert_columns) = match merge_written_columns(statement, &def) {
        Ok(columns) => columns,
        Err(error) => return sql_fail(error),
    };
    let authorization = target
        .above
        .map_or(DmlAuthorization::Invoker, |above| above.authorization);
    let role = match authorization.role(storage, txn.txid) {
        Ok(role) => role,
        Err(error) => return sql_fail(error),
    };
    let mut events = 0u8;
    for when in statement.whens {
        events |= merge_action_event(when.action()).unwrap_or(0);
    }
    let privileges = (|| -> Result<(), SqlError> {
        if events & TriggerEvents::INSERT != 0 {
            require_view_column_privilege_as(
                storage,
                view_slot,
                crate::storage::PrivilegeSet::INSERT,
                insert_columns,
                role,
                txn.txid,
            )?;
        }
        if events & TriggerEvents::UPDATE != 0 {
            require_view_column_privilege_as(
                storage,
                view_slot,
                crate::storage::PrivilegeSet::UPDATE,
                update_columns,
                role,
                txn.txid,
            )?;
        }
        if events & TriggerEvents::DELETE != 0 {
            require_view_privilege_as(
                storage,
                view_slot,
                crate::storage::PrivilegeSet::DELETE,
                role,
                txn.txid,
            )?;
        }
        // The views above the written one check what the statement reads.
        if target.above.is_none() {
            let reads = view_dml_reads(
                storage,
                &Stmt::Merge(*statement),
                view_slot,
                txn.txid,
                arena,
            )?;
            if !reads.is_empty() {
                require_view_column_privilege_as(
                    storage,
                    view_slot,
                    crate::storage::PrivilegeSet::SELECT,
                    reads,
                    role,
                    txn.txid,
                )?;
            }
        }
        crate::sql::query::require_dml_builtin_execute(
            &Stmt::Merge(*statement),
            storage,
            txn.txid,
            arena,
            outer,
        )
    })();
    if let Err(error) = privileges {
        return sql_fail(error);
    }
    let assignment_casts = match prepare_dml_assignment_casts(
        &Stmt::Merge(*statement),
        storage,
        txn.txid,
        arena,
        outer,
    ) {
        Ok(casts) => casts,
        Err(error) => return sql_fail(error),
    };
    // The check options of the views above apply to the rows the triggers
    // return; their defaults, then the view's own, fill unwritten columns.
    let checks = target.above.map(|above| ViewCheck {
        checks: above.check.checks,
        defaults: &[],
    });
    let defaults = match instead_of_view_defaults(
        storage,
        view_slot,
        &def,
        target.above.map_or(&[], |above| above.check.defaults),
        txn.txid,
        arena,
    ) {
        Ok(defaults) => defaults,
        Err(error) => return sql_fail(error),
    };
    let default_exprs = match parse_view_defaults(Some(defaults), &def, arena, storage, txn.txid) {
        Ok(expressions) => expressions,
        Err(error) => return sql_fail(error),
    };
    // A resumed attempt continues a row phase whose statement triggers ran.
    let resuming = progress
        .as_ref()
        .is_some_and(|progress| progress.resuming());
    for event in [
        TriggerEvents::INSERT,
        TriggerEvents::UPDATE,
        TriggerEvents::DELETE,
    ] {
        if events & event != 0
            && !resuming
            && let Err(error) = fire_statement_triggers_with_rows(
                TriggerExecContext {
                    host: PlpgsqlExecHost::Atomic {
                        storage,
                        services: services.reborrow(),
                    },
                    txn,
                    arena,
                    params: crate::sql::eval::NO_PARAMS,
                    responder,
                },
                trigger_target,
                &def,
                event,
                true,
                if event == TriggerEvents::UPDATE {
                    update_columns
                } else {
                    ColumnSet::EMPTY
                },
                None,
            )
        {
            return sql_fail(error);
        }
    }
    if let Some(progress) = progress.as_deref_mut()
        && let Err(error) = progress.enter_selection()
    {
        return sql_fail(error);
    }
    let target_alias = statement.target_alias.unwrap_or(statement.target.name);
    let source = match merge_source(storage, statement, txn.txid, arena, outer) {
        Ok(source) => source,
        Err(error) => return sql_fail(error),
    };
    if !statement.returning.is_empty() && !capturing && !resuming {
        let mark = arena.mark();
        let mut columns = [ColDesc::new("", 0, 0); MAX_PROJ];
        let described = describe_merge_returning(
            storage,
            txn.txid,
            statement,
            arena,
            &mut columns,
            crate::sql::exec::DescriptionTarget::Wire,
        )
        .map_err(Ok)
        .and_then(|count| responder.row_description(&columns[..count]).map_err(Err));
        // RowDescription has copied every descriptor.
        unsafe { arena.rewind_to(mark) };
        match described {
            Ok(()) => {}
            Err(Ok(error)) => return sql_fail(error),
            Err(Err(wire)) => return Err(wire),
        }
    }
    // The source and view rows are those before this command's changes,
    // also when a resumed attempt selects them again.
    let selection_snapshot = storage.dml_expression_snapshot(txn.command_id());
    let source_rows = match merge_source_rows(
        storage,
        txn.txid,
        &source,
        arena,
        params,
        outer,
        seq_session,
    ) {
        Ok(rows) => rows,
        Err(error) => return sql_fail(error),
    };
    let view_rows = match select_merge_view_rows(
        storage,
        txn.txid,
        view_slot,
        &def,
        target.above,
        arena,
        params,
        seq_session,
    ) {
        Ok(rows) => rows,
        Err(error) => return sql_fail(error),
    };
    drop(selection_snapshot);
    let n_view = view_rows.len();
    let matched_by_source = match arena.alloc_slice_with(n_view, |_| false) {
        Ok(flags) => flags,
        Err(_) => return sql_fail(arena_full()),
    };
    // The saved state's per-row action flags, which no view row uses.
    let unacted = match arena.alloc_slice_with(n_view, |_| false) {
        Ok(flags) => flags,
        Err(_) => return sql_fail(arena_full()),
    };
    row_progress::enter_rows(progress.as_deref_mut());
    // Row progress counts, per source row, one position for each view row
    // pair and one for its NOT MATCHED stage; the last source position is
    // the NOT MATCHED BY SOURCE pass.
    let stride = n_view + 1;
    let positions = (source_rows.len() + 1).saturating_mul(stride);
    let no_transitions = row_progress::capture_lengths([None; row_progress::CAPTURES]);
    let (first_position, mut affected_count, mut resumed_match) = match progress.as_deref() {
        Some(progress) => {
            let resumed =
                match progress.resume_rows(|position| (position < positions).then_some(None)) {
                    Ok(resumed) => resumed,
                    Err(error) => return sql_fail(error),
                };
            let matched = match progress.resumed_state(arena) {
                Ok(Some(state)) => match restore_merge_state(state, unacted, matched_by_source) {
                    Ok(matched) => matched,
                    Err(error) => return sql_fail(error),
                },
                Ok(None) => SourceRowMatch::default(),
                Err(error) => return sql_fail(error),
            };
            (resumed.first, resumed.changed, matched)
        }
        None => (0, 0, SourceRowMatch::default()),
    };
    let has_not_matched_by_source = statement
        .whens
        .iter()
        .any(|when| when.kind() == MergeMatchKind::NotMatchedBySource);
    let mut schema = [ColType::Bool; MAX_COLUMNS];
    def.schema(&mut schema);
    let schema = &schema[..def.n_columns];
    let mut source_index = first_position / stride;
    let mut first_pair = first_position % stride;
    let mut matched = false;
    let source_def = &source.def;
    let rows_outcome = (|| -> Outcome {
        'rows: loop {
            if super::returning_satisfied(&capture, affected_count) {
                break;
            }
            let by_source = source_index == source_rows.len();
            if by_source && !has_not_matched_by_source {
                break;
            }
            let mut sv = [Datum::Null; MAX_COLUMNS];
            let width = if by_source {
                source_def.n_columns
            } else {
                let bytes = source_rows[source_index];
                let width = projected_row_width(bytes);
                for (column, slot) in sv.iter_mut().enumerate().take(width) {
                    *slot = decode_projected_pub(bytes, column);
                }
                width
            };
            let sv = &sv[..source_def.n_columns.min(width)];
            let source_values = (!by_source).then_some(sv);
            matched = by_source || core::mem::take(&mut resumed_match).matched;
            for j in core::mem::take(&mut first_pair)..=n_view {
                if super::returning_satisfied(&capture, affected_count) {
                    break 'rows;
                }
                let position = source_index * stride + j;
                let not_matched_stage = j == n_view;
                if not_matched_stage && (matched || by_source) {
                    continue;
                }
                if by_source && matched_by_source[j] {
                    continue;
                }
                match row_progress::start_row(
                    progress.as_deref_mut(),
                    position,
                    None,
                    affected_count,
                    no_transitions,
                    txn,
                    responder,
                    arena,
                ) {
                    Ok(None) => {}
                    Ok(Some(saved)) => {
                        // The pair fired its trigger in an earlier attempt;
                        // only its RETURNING values remain.
                        let action = match (saved.old, saved.new) {
                            (Some(_), Some(_)) => "UPDATE",
                            (Some(_), None) => "DELETE",
                            (None, Some(_)) => "INSERT",
                            (None, None) => {
                                return sql_fail(sql_err!(
                                    sqlstate::INTERNAL_ERROR,
                                    "a resumed MERGE row lost its action"
                                ));
                            }
                        };
                        if !not_matched_stage {
                            matched = true;
                        }
                        match emit_merge_progress_returning(
                            progress.as_deref_mut(),
                            ChangedRow {
                                index: position,
                                row: None,
                                changed: affected_count,
                                transitions: no_transitions,
                            },
                            storage,
                            txn,
                            &def,
                            target_alias,
                            source_def,
                            source.alias,
                            source.members,
                            source_values,
                            statement.returning,
                            saved.old,
                            saved.new,
                            action,
                            arena,
                            params,
                            responder,
                            &mut capture,
                            seq_session,
                        )? {
                            Ok(()) => {}
                            Err(error) => return sql_fail(error),
                        }
                        affected_count += 1;
                        continue;
                    }
                    Err(error) => return sql_fail(error),
                }
                if not_matched_stage {
                    // A source row that joined no view row: its first
                    // applicable NOT MATCHED [BY TARGET] clause.
                    let source_row = RowCtx {
                        table_oid: None,
                        def: *source_def,
                        values: sv,
                        alias: None,
                    };
                    let scope = ProceduralMergeScope {
                        row: &source_row,
                        outer,
                    };
                    let mut chosen = None;
                    for when in statement
                        .whens
                        .iter()
                        .filter(|when| when.kind() == MergeMatchKind::NotMatchedByTarget)
                    {
                        match merge_condition_holds(
                            when,
                            storage,
                            txn.txid,
                            seq_session,
                            arena,
                            params,
                            &scope,
                        ) {
                            Ok(true) => {
                                chosen = Some(when.action());
                                break;
                            }
                            Ok(false) => {}
                            Err(error) => return sql_fail(error),
                        }
                    }
                    let Some(MergeAction::Insert {
                        columns,
                        values,
                        default_values,
                        overriding,
                    }) = chosen
                    else {
                        continue;
                    };
                    let (mut row, explicit) = match merge_insert_values(
                        &def,
                        columns,
                        values,
                        default_values,
                        overriding,
                        &scope,
                        assignment_casts,
                        storage,
                        txn.txid,
                        seq_session,
                        arena,
                        params,
                    ) {
                        Ok(row) => row,
                        Err(error) => return sql_fail(error),
                    };
                    if let Err(error) = fill_instead_of_view_defaults(
                        &mut row,
                        &explicit,
                        storage,
                        txn.txid,
                        seq_session,
                        defaults,
                        &def,
                        &default_exprs,
                        arena,
                    ) {
                        return sql_fail(error);
                    }
                    let new = match detached_values(&row[..def.n_columns], arena) {
                        Ok(values) => values,
                        Err(error) => return sql_fail(error),
                    };
                    match fire_trigger_merge_row(
                        storage,
                        txn,
                        arena,
                        services,
                        responder,
                        trigger_target,
                        &def,
                        TriggerEvents::INSERT,
                        None,
                        Some(&mut *new),
                        checks,
                        params,
                    ) {
                        Ok(true) => {}
                        Ok(false) => continue,
                        Err(error) => return sql_fail(error),
                    }
                    if !statement.returning.is_empty() {
                        match emit_merge_progress_returning(
                            progress.as_deref_mut(),
                            ChangedRow {
                                index: position,
                                row: None,
                                changed: affected_count,
                                transitions: no_transitions,
                            },
                            storage,
                            txn,
                            &def,
                            target_alias,
                            source_def,
                            source.alias,
                            source.members,
                            source_values,
                            statement.returning,
                            None,
                            Some(&*new),
                            "INSERT",
                            arena,
                            params,
                            responder,
                            &mut capture,
                            seq_session,
                        )? {
                            Ok(()) => {}
                            Err(error) => return sql_fail(error),
                        }
                    }
                    affected_count += 1;
                    continue;
                }
                let mut old = [Datum::Null; MAX_COLUMNS];
                if let Err(error) = rowenc::decode(view_rows[j], schema, &mut old) {
                    return sql_fail(error);
                }
                let old = &old[..def.n_columns];
                let lookup = MergeLookup {
                    target_def: crate::storage::RelationDefinition::from_definition(&def),
                    target_alias,
                    target: old,
                    source_def,
                    source_alias: source.alias,
                    source_members: source.members,
                    source: source_values,
                    source_namespace_when_absent: false,
                    old_name: None,
                    old: None,
                    new_name: None,
                    new: None,
                };
                let scope = ProceduralMergeScope {
                    row: &lookup,
                    outer,
                };
                if !by_source {
                    match eval_merge_expression(
                        statement.on,
                        storage,
                        txn.txid,
                        seq_session,
                        arena,
                        params,
                        &scope,
                        None,
                    ) {
                        Ok(Datum::Bool(true)) => {}
                        Ok(_) => continue,
                        Err(error) => return sql_fail(error),
                    }
                    matched_by_source[j] = true;
                    matched = true;
                }
                let candidate = if by_source {
                    MergeMatchKind::NotMatchedBySource
                } else {
                    MergeMatchKind::Matched
                };
                let mut chosen = None;
                for when in statement
                    .whens
                    .iter()
                    .filter(|when| when.kind() == candidate)
                {
                    match merge_condition_holds(
                        when,
                        storage,
                        txn.txid,
                        seq_session,
                        arena,
                        params,
                        &scope,
                    ) {
                        Ok(true) => {
                            chosen = Some(when.action());
                            break;
                        }
                        Ok(false) => {}
                        Err(error) => return sql_fail(error),
                    }
                }
                let (event, new, action) = match chosen {
                    None | Some(MergeAction::DoNothing) => continue,
                    Some(MergeAction::Delete) => (TriggerEvents::DELETE, None, "DELETE"),
                    Some(MergeAction::Update(assignments)) => {
                        let mut new = [Datum::Null; MAX_COLUMNS];
                        new[..def.n_columns].copy_from_slice(old);
                        for (name, expression) in assignments {
                            let Some(column) = def.column_index(name) else {
                                return sql_fail(undefined_column(name));
                            };
                            let value = if matches!(expression, Expr::DefaultMarker) {
                                view_column_default(
                                    storage,
                                    txn.txid,
                                    seq_session,
                                    defaults,
                                    &def,
                                    column,
                                    &default_exprs,
                                    arena,
                                )
                            } else {
                                assignment_casts
                                    .selected(expression, &def.columns()[column], storage, txn.txid)
                                    .and_then(|cast| {
                                        eval_merge_expression(
                                            expression,
                                            storage,
                                            txn.txid,
                                            seq_session,
                                            arena,
                                            params,
                                            &scope,
                                            Some((&def.columns()[column], cast)),
                                        )
                                    })
                            };
                            new[column] = match value {
                                Ok(value) => value,
                                Err(error) => return sql_fail(error),
                            };
                        }
                        // Detach the NEW image before the trigger body
                        // changes storage the source rows may read.
                        match detached_values(&new[..def.n_columns], arena) {
                            Ok(new) => (TriggerEvents::UPDATE, Some(new), "UPDATE"),
                            Err(error) => return sql_fail(error),
                        }
                    }
                    Some(MergeAction::Insert { .. }) => {
                        return sql_fail(sql_err!(
                            sqlstate::SYNTAX_ERROR,
                            "INSERT is not allowed in a target-backed MERGE candidate"
                        ));
                    }
                };
                let mut new = new;
                let fired = fire_trigger_merge_row(
                    storage,
                    txn,
                    arena,
                    services,
                    responder,
                    trigger_target,
                    &def,
                    event,
                    Some(old),
                    new.as_deref_mut(),
                    checks,
                    params,
                );
                match fired {
                    Ok(true) => {}
                    Ok(false) => continue,
                    Err(error) => return sql_fail(error),
                }
                if !statement.returning.is_empty() {
                    match emit_merge_progress_returning(
                        progress.as_deref_mut(),
                        ChangedRow {
                            index: position,
                            row: None,
                            changed: affected_count,
                            transitions: no_transitions,
                        },
                        storage,
                        txn,
                        &def,
                        target_alias,
                        source_def,
                        source.alias,
                        source.members,
                        source_values,
                        statement.returning,
                        Some(old),
                        new.as_deref(),
                        action,
                        arena,
                        params,
                        responder,
                        &mut capture,
                        seq_session,
                    )? {
                        Ok(()) => {}
                        Err(error) => return sql_fail(error),
                    }
                }
                affected_count += 1;
            }
            if by_source {
                break;
            }
            source_index += 1;
        }
        sql_ok()
    })();
    if let Some(progress) = progress {
        if matches!(&rows_outcome, Ok(Err(error)) if progress.stops_for(error))
            && let Err(error) = save_merge_state(
                progress,
                unacted,
                matched_by_source,
                SourceRowMatch { matched },
                arena,
                &storage.row_work,
            )
        {
            return sql_fail(error);
        }
        if let Err(error) = row_progress::settle_rows(
            Some(progress),
            &rows_outcome,
            txn,
            [None; row_progress::CAPTURES],
            &storage.row_work,
        ) {
            return sql_fail(error);
        }
    }
    match rows_outcome {
        Ok(Ok(())) => {}
        outcome => return outcome,
    }
    // A write its RETURNING destination stopped never reaches the end of its
    // rows, so its AFTER STATEMENT triggers do not fire.
    let stopped = super::returning_satisfied(&capture, affected_count);
    for event in [
        TriggerEvents::DELETE,
        TriggerEvents::UPDATE,
        TriggerEvents::INSERT,
    ] {
        if events & event != 0
            && !stopped
            && let Err(error) = fire_statement_triggers_with_rows(
                TriggerExecContext {
                    host: PlpgsqlExecHost::Atomic {
                        storage,
                        services: services.reborrow(),
                    },
                    txn,
                    arena,
                    params: crate::sql::eval::NO_PARAMS,
                    responder,
                },
                trigger_target,
                &def,
                event,
                false,
                if event == TriggerEvents::UPDATE {
                    update_columns
                } else {
                    ColumnSet::EMPTY
                },
                None,
            )
        {
            return sql_fail(error);
        }
    }
    if capturing {
        responder.set_affected_rows(affected_count);
    } else {
        let tag = stack_format!(48, "MERGE {}", affected_count);
        responder.command_complete_rows(tag.as_str(), affected_count)?;
    }
    sql_ok()
}

/// Fires the view's `INSTEAD OF` row trigger for one action and checks the
/// row it returns against the check options of the views above. `false`
/// when the trigger returned NULL.
#[allow(clippy::too_many_arguments)]
fn fire_trigger_merge_row<'a>(
    storage: &mut Storage,
    txn: &mut TxnState,
    arena: &'a Arena,
    services: &mut TriggerServices<'_>,
    responder: &mut Responder,
    target: crate::storage::TriggerTarget,
    def: &crate::storage::TableDefView<'a>,
    event: u8,
    old: Option<&[Datum<'a>]>,
    new: Option<&mut [Datum<'a>]>,
    checks: Option<ViewCheck<'a>>,
    params: &[Datum<'a>],
) -> Result<bool, SqlError> {
    let seq_session = services.seq_session;
    let mut new = new;
    if !fire_view_row_trigger(
        storage,
        txn,
        arena,
        services,
        responder,
        target,
        def,
        event,
        old,
        new.as_deref_mut(),
    )? {
        return Ok(false);
    }
    if let Some(new) = new {
        enforce_view_check(
            checks,
            def,
            new,
            ViewCheckContext {
                storage,
                txid: txn.txid,
                arena,
                params,
                seq_session,
            },
        )?;
    }
    Ok(true)
}

/// A view column's default for `SET column = DEFAULT`: the first default
/// the views supply, else NULL, as PostgreSQL's rewriter substitutes it.
#[allow(clippy::too_many_arguments)]
fn view_column_default<'a>(
    storage: &Storage,
    txid: u32,
    seq_session: &crate::sql::guc::SeqSession,
    defaults: ViewCheck<'_>,
    def: &'a crate::storage::TableDefView<'_>,
    column: usize,
    expressions: &constraints::ParsedDefaults<'a>,
    arena: &'a Arena,
) -> Result<Datum<'a>, SqlError> {
    let sequences = crate::sql::sequence::SeqEval::new(storage, seq_session, txid);
    let catalog = crate::sql::query::storage_catalog(storage, arena, txid);
    let hooks = EvalHooks {
        catalog: Some(&catalog),
        sequences: Some(&sequences),
        ..NO_HOOKS
    };
    insert_default_value(
        storage,
        txid,
        Some(defaults),
        def,
        column,
        None,
        expressions[column],
        arena,
        &hooks,
    )
}

/// The view's rows as of the statement's command, kept to those the views
/// above it show.
#[allow(clippy::too_many_arguments)]
fn select_merge_view_rows<'a>(
    storage: &Storage,
    txid: u32,
    view_slot: usize,
    def: &crate::storage::TableDefView<'a>,
    above: Option<MergeViewTarget<'a>>,
    arena: &'a Arena,
    params: &[Datum<'a>],
    seq_session: &crate::sql::guc::SeqSession,
) -> Result<&'a [&'a [u8]], SqlError> {
    let mut rows = ArenaRows::new();
    let mut schema = [ColType::Bool; MAX_COLUMNS];
    def.schema(&mut schema);
    let schema = &schema[..def.n_columns];
    select_view_rows(
        storage,
        txid,
        view_slot,
        ViewRowSelection {
            definition: crate::storage::RelationDefinition::from_definition(def),
            predicate: None,
            alias: None,
        },
        arena,
        TriggerQueryExecution {
            params,
            seq_session,
        },
        &mut |encoded| {
            if above.is_some_and(|above| above.filter.is_some()) {
                let mut values = [Datum::Null; MAX_COLUMNS];
                rowenc::decode(encoded, schema, &mut values)?;
                if !merge_view_row_visible(
                    above,
                    def,
                    &values[..def.n_columns],
                    storage,
                    txid,
                    seq_session,
                    arena,
                    params,
                )? {
                    return Ok(());
                }
            }
            rows.push(arena, encoded)
        },
    )?;
    Ok(rows.into_slice())
}
