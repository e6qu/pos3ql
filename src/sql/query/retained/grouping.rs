//! A retained grouping: `GROUP BY`, `HAVING` and aggregates computed by the
//! reader itself in `plan_build_bytes`.
//!
//! At the first row the reader reads its whole input (one table, generator
//! calls, a literal list or a retained join), filters it, and saves each
//! passing row's grouping keys and the input columns its aggregates read. A
//! storage wait keeps the rows saved so far and the input position, so input
//! is never read twice. The saved rows are then ordered by key, equal keys
//! keeping input order, and each group's aggregates fold with the general
//! executor's aggregate states over that group's rows in input order. Results,
//! the order of groups (keys ascending, NULLs last) and the key a group
//! reports (its first input row's, as PostgreSQL's hashed grouping keeps)
//! match the general executor. Folding reads only saved rows and calls no
//! routine body, so no transition state lives across a wait. `HAVING` and the
//! output list run per group as its row is returned, so a routine call there
//! hands off on its row.

use super::*;
use crate::mem::lease_pool::{LeaseError, MutableSlice};
use crate::sql::ast::{Collation, GroupingSet};
use crate::sql::query::aggregate::AggState;

/// Aggregate calls by address in the output list, `HAVING` and `ORDER BY`.
pub(super) type AggregateNodes<'a> = &'a [(*const Expr<'a>, &'a Expr<'a>)];

/// How a retained reader treats a statement's grouping.
pub(super) enum Grouping<'a> {
    /// No `GROUP BY`, `HAVING` or aggregate call.
    Plain,
    /// The reader groups and aggregates itself: the statement with its
    /// `GROUP BY` positions resolved, and its aggregate calls.
    Grouped {
        statement: &'a Select<'a>,
        aggregates: AggregateNodes<'a>,
    },
}

fn group_full(error: LeaseError) -> SqlError {
    sql_err!(
        sqlstate::PROGRAM_LIMIT_EXCEEDED,
        "retained grouping exceeds plan_build_bytes: {}",
        error
    )
    .plan_build_exhausted()
}

/// Whether `expression` may run while the input is read: as a sort key,
/// it is non-volatile, hands no call off to a routine and casts only
/// through builtin paths.
fn before_input_end(
    expression: &Expr<'_>,
    columns: &CatalogScopeCols<'_, '_, '_, '_>,
    catalog: &dyn crate::sql::eval::CatalogAccess,
) -> Result<bool, SqlError> {
    let storage = columns.storage;
    let mut safe_casts = true;
    validate_statement_casts(expression, columns, &mut safe_casts)?;
    Ok(safe_casts
        && retained_expression(expression, catalog, Some((storage, columns.txid)))
        && retained_scalar_calls(expression, columns)?
        && !crate::sql::eval::routine_input_handoff(expression, Some(catalog))
        && !super::predicate_safety::expression_properties(
            expression,
            columns,
            storage,
            columns.txid,
            super::predicate_safety::AnalysisStage::RuntimeProjection,
        )?
        .volatile)
}

/// Whether an aggregate's state folds without a stored routine body: a
/// builtin aggregate, or a custom one whose support functions are builtins.
/// Object-store mode combines partial custom states the general executor's
/// way, which only it reproduces.
fn aggregate_folds_here<'a>(
    node: &'a Expr<'a>,
    storage: &Storage,
    txid: u32,
    scope: &QueryScope<'a>,
    arena: &'a Arena,
) -> bool {
    let mark = arena.mark();
    let mut state = AggState::default();
    let admitted = state
        .init(node, storage, txid, &super::ScopeCols(scope), arena)
        .is_ok()
        && state
            .custom_support_oids()
            .all(|oid| storage.routine_slot_by_oid(oid, txid).is_none())
        && !(storage.spill_attached() && state.supports_partial());
    // SAFETY: the probe state and its allocations end here.
    unsafe { arena.rewind_to(mark) };
    admitted
}

/// Decides, before the statement runs, whether a retained reader computes
/// its grouping itself. `None` leaves the statement to the general executor,
/// which also reports any error in it.
pub(super) fn admit_grouping<'a>(
    statement: &'a Select<'a>,
    columns: &CatalogScopeCols<'_, 'a, '_, '_>,
    catalog: &dyn crate::sql::eval::CatalogAccess,
    arena: &'a Arena,
) -> Result<Option<Grouping<'a>>, SqlError> {
    let (scope, storage, txid) = (columns.scope, columns.storage, columns.txid);
    let mut aggregates = crate::mem::arena::ArenaList::new(arena);
    for expression in statement
        .items
        .iter()
        .filter_map(|item| match item {
            SelectItem::Expr { expression, .. } | SelectItem::RecordStar(expression) => {
                Some(*expression)
            }
            SelectItem::Wildcard | SelectItem::TableWildcard(_) => None,
        })
        .chain(statement.having)
        .chain(statement.order_by.iter().map(|order| order.expression))
    {
        if super::collect_aggs(expression, &mut aggregates, storage, txid, columns).is_err() {
            return Ok(None);
        }
    }
    if aggregates.is_empty() && statement.group_by.is_empty() && statement.having.is_none() {
        return Ok(Some(Grouping::Plain));
    }
    if !statement.grouping_sets.is_empty()
        || statement.distinct
        || !statement.distinct_on.is_empty()
        || statement.with_ties
        || !statement.locking.is_empty()
        || statement.group_by.len() > crate::sql::parser::MAX_GROUP_TERMS
        || statement
            .items
            .iter()
            .any(|item| !matches!(item, SelectItem::Expr { .. }))
    {
        return Ok(None);
    }
    let Ok(statement) = super::resolve_group_ordinals(statement, Some(scope), arena, storage, txid)
    else {
        return Ok(None);
    };
    if super::check_key_types(statement, scope, arena, storage, txid).is_err() {
        return Ok(None);
    }
    let aggregates = aggregates.as_slice();
    let mut pointers = [core::ptr::null::<Expr<'a>>(); MAX_PROJ];
    if aggregates.len() > pointers.len() {
        return Ok(None);
    }
    for (pointer, (node, _)) in pointers.iter_mut().zip(aggregates) {
        *pointer = *node;
    }
    let pointers = &pointers[..aggregates.len()];
    // The grouped output: every column reference grouped, and only
    // expressions the reader evaluates per group.
    for expression in statement
        .items
        .iter()
        .filter_map(|item| match item {
            SelectItem::Expr { expression, .. } => Some(*expression),
            _ => None,
        })
        .chain(statement.having)
    {
        let mut safe_casts = true;
        validate_statement_casts(expression, columns, &mut safe_casts)?;
        if !safe_casts
            || crate::sql::query::group::ungrouped_column(
                expression,
                statement.group_by,
                scope,
                aggregates,
            )
            .is_some()
            || !retained_expression_with(
                expression,
                catalog,
                Some((storage, txid)),
                Leaves::Aggregates(pointers),
            )
            || !retained_scalar_calls_except(expression, columns, pointers)?
        {
            return Ok(None);
        }
    }
    // WHERE is checked conjunct by conjunct, as the general executor
    // flattens it.
    let mut conjuncts = [&Expr::Null; crate::sql::query::plan::MAX_CONJUNCTS];
    let mut count = 0;
    if let Some(predicate) = statement.where_clause
        && !crate::sql::query::plan::flatten_and(predicate, &mut conjuncts, &mut count)
    {
        return Ok(None);
    }
    for expression in statement
        .group_by
        .iter()
        .copied()
        .chain(conjuncts[..count].iter().copied())
    {
        if !before_input_end(expression, columns, catalog)? {
            return Ok(None);
        }
    }
    for (_, node) in aggregates {
        let Expr::Call {
            args,
            order_by,
            filter,
            over: None,
            ..
        } = node
        else {
            return Ok(None);
        };
        for expression in args
            .iter()
            .copied()
            .chain(order_by.iter().map(|order| order.expression))
            .chain(*filter)
        {
            if !before_input_end(expression, columns, catalog)? {
                return Ok(None);
            }
        }
        if !aggregate_folds_here(node, storage, txid, scope, arena) {
            return Ok(None);
        }
    }
    // ORDER BY over groups: only a prefix of the keys, ascending with NULLs
    // last, which is the order groups are formed in.
    for (index, order) in statement.order_by.iter().enumerate() {
        let Ok(target) = super::resolve_order_target(
            order.expression,
            statement.items,
            scope,
            arena,
            storage,
            txid,
        ) else {
            return Ok(None);
        };
        let key = statement.group_by.get(index);
        if order.descending
            || order.nulls_first
            || !key.is_some_and(|key| {
                **key == *target || crate::sql::query::group::same_scope_column(scope, key, target)
            })
        {
            return Ok(None);
        }
    }
    Ok(Some(Grouping::Grouped {
        statement,
        aggregates,
    }))
}

/// The source columns a grouped reader reads: its output list, filter,
/// `HAVING`, keys and `ORDER BY`.
pub(super) fn input_demand<'a>(
    scope: &QueryScope<'a>,
    from: &'a FromClause<'a>,
    statement: &'a Select<'a>,
    arena: &'a Arena,
) -> Result<PaxReadDemand, SqlError> {
    let mut expressions = crate::mem::arena::ArenaList::new(arena);
    for expression in statement
        .items
        .iter()
        .filter_map(|item| match item {
            SelectItem::Expr { expression, .. } => Some(*expression),
            _ => None,
        })
        .chain(statement.where_clause)
        .chain(statement.having)
        .chain(statement.group_by.iter().copied())
        .chain(statement.order_by.iter().map(|order| order.expression))
    {
        expressions.push(expression).map_err(|_| arena_full())?;
    }
    Ok(pax_column_demand(scope, from, expressions.as_slice()))
}

#[derive(Clone, Copy)]
enum Phase {
    /// Reading input: saved rows grow.
    Filling,
    /// Input is ordered; groups before entry `next` are folded.
    Folding { next: usize },
    /// Groups are folded; group `next` is returned next.
    Output { next: usize },
}

pub(super) struct GroupState<'a> {
    keys: &'a [&'a Expr<'a>],
    collations: &'a [Collation],
    set: GroupingSet<'a>,
    aggregates: AggregateNodes<'a>,
    pointers: &'a [*const Expr<'a>],
    having: Option<&'a Expr<'a>>,
    /// The input columns the aggregates read; other columns save as NULL.
    demand: PaxReadDemand,
    /// One record per input row: its keys, then per source a presence flag
    /// and its row's columns.
    inputs: MutableSlice<(u32, u32)>,
    input_bytes: MutableSlice<u8>,
    /// One record per group: its keys, then its aggregate results.
    groups: MutableSlice<(u32, u32)>,
    group_bytes: MutableSlice<u8>,
    phase: Phase,
    current: Option<usize>,
}

fn push_record(
    entries: &mut MutableSlice<(u32, u32)>,
    bytes: &mut MutableSlice<u8>,
    record: &[u8],
) -> Result<(), SqlError> {
    let start = bytes.len();
    let end = start + record.len();
    let (Ok(start32), Ok(length32), Ok(_)) = (
        u32::try_from(start),
        u32::try_from(record.len()),
        u32::try_from(end),
    ) else {
        return Err(sql_err!(
            sqlstate::PROGRAM_LIMIT_EXCEEDED,
            "retained grouping exceeds 4 GiB of rows"
        )
        .plan_build_exhausted());
    };
    if end > bytes.capacity_entries() {
        // Grow geometrically: each move copies the records saved so far.
        let grown = bytes
            .capacity_entries()
            .saturating_mul(2)
            .min(bytes.max_entries())
            .max(end);
        bytes.reserve_exact(grown).map_err(group_full)?;
    }
    bytes.extend_from_slice(record).map_err(group_full)?;
    if let Err(error) = entries.push((start32, length32)) {
        bytes.truncate(start);
        return Err(group_full(error));
    }
    Ok(())
}

fn record<'b>(entries: &MutableSlice<(u32, u32)>, bytes: &'b [u8], index: usize) -> &'b [u8] {
    let (start, length) = entries.as_slice()[index];
    &bytes[start as usize..(start + length) as usize]
}

fn compare_keys(
    storage: &Storage,
    txid: u32,
    collations: &[Collation],
    left: &[u8],
    right: &[u8],
) -> Result<core::cmp::Ordering, SqlError> {
    for (column, &collation) in collations.iter().enumerate() {
        let left = crate::sql::exec::decode_projected_pub(left, column);
        let right = crate::sql::exec::decode_projected_pub(right, column);
        let ordering = match (left.is_null(), right.is_null()) {
            (true, true) => core::cmp::Ordering::Equal,
            (true, false) => core::cmp::Ordering::Greater,
            (false, true) => core::cmp::Ordering::Less,
            (false, false) => {
                crate::sql::eval::compare_datums_collated(storage, txid, collation, &left, &right)?
            }
        };
        if !ordering.is_eq() {
            return Ok(ordering);
        }
    }
    Ok(core::cmp::Ordering::Equal)
}

impl<'a> GroupState<'a> {
    pub(super) fn new(
        storage: &Storage,
        statement: &'a Select<'a>,
        from: &'a FromClause<'a>,
        aggregates: AggregateNodes<'a>,
        scope: &QueryScope<'a>,
        arena: &'a Arena,
    ) -> Result<Self, SqlError> {
        let keys = statement.group_by;
        let collations = arena
            .alloc_slice_with(keys.len(), |_| Collation::None)
            .map_err(|_| arena_full())?;
        for (collation, key) in collations.iter_mut().zip(keys) {
            *collation = scope.expression_collation(storage, arena, key)?;
        }
        let words = keys.len().div_ceil(u64::BITS as usize);
        let set = GroupingSet {
            words: arena
                .alloc_slice_with(words, |index| {
                    let tail = keys.len() % u64::BITS as usize;
                    if index + 1 == words && tail != 0 {
                        (1u64 << tail) - 1
                    } else {
                        u64::MAX
                    }
                })
                .map_err(|_| arena_full())?,
        };
        let pointers = arena
            .alloc_slice_with(aggregates.len(), |index| aggregates[index].0)
            .map_err(|_| arena_full())?;
        let expressions = arena
            .alloc_slice_with(aggregates.len(), |index| aggregates[index].1)
            .map_err(|_| arena_full())?;
        let demand = pax_column_demand(scope, from, expressions);
        let pool = storage.plan_build();
        Ok(Self {
            keys,
            collations,
            set,
            aggregates,
            pointers,
            having: statement.having,
            demand,
            inputs: pool.lease(0).map_err(group_full)?,
            input_bytes: pool.lease(0).map_err(group_full)?,
            groups: pool.lease(0).map_err(group_full)?,
            group_bytes: pool.lease(0).map_err(group_full)?,
            phase: Phase::Filling,
            current: None,
        })
    }

    pub(super) fn filling(&self) -> bool {
        matches!(self.phase, Phase::Filling)
    }

    pub(super) fn has_current(&self) -> bool {
        self.current.is_some()
    }

    pub(super) fn finish_row(&mut self) {
        self.current = None;
    }

    /// Encodes one passing input row: its keys, then its sources' columns
    /// the aggregates read.
    pub(super) fn encode_input<'work>(
        &self,
        scope: &QueryScope<'_>,
        storage: &Storage,
        row: &scan::JoinRow<'_, 'work, '_>,
        work: &'work Arena,
        params: &[Datum<'work>],
        hooks: &EvalHooks<'_, 'work>,
    ) -> Result<&'work [u8], SqlError> {
        let mut values = crate::mem::arena::ArenaList::new(work);
        for (key, &collation) in self.keys.iter().zip(self.collations) {
            let value = crate::sql::eval::text_view(eval_full(key, work, params, row, hooks)?);
            storage.validate_text_collation(collation, &value)?;
            values.push(value).map_err(|_| arena_full())?;
        }
        for table in 0..scope.n {
            let width = scope.row_width(table);
            let saved = row
                .table_values(table)
                .filter(|_| row.table_present(table) == Some(true));
            values
                .push(Datum::Bool(saved.is_some()))
                .map_err(|_| arena_full())?;
            let n_columns = scope.defs.get(table).expect("resolved").n_columns;
            for column in 0..width {
                let value = saved
                    .filter(|_| self.demand.references_column(table, column, n_columns))
                    .and_then(|columns| columns.get(column).copied())
                    .unwrap_or(Datum::Null);
                values.push(value).map_err(|_| arena_full())?;
            }
        }
        let values = values.as_slice();
        let length = crate::sql::exec::projected_row_len(values)?;
        let bytes = work
            .alloc_slice_with(length, |_| 0u8)
            .map_err(|_| arena_full())?;
        crate::sql::exec::encode_projected_into(values, bytes)?;
        Ok(bytes)
    }

    pub(super) fn push_input(&mut self, record: &[u8]) -> Result<(), SqlError> {
        push_record(&mut self.inputs, &mut self.input_bytes, record)
    }

    /// Orders the saved input by key; equal keys keep input order.
    pub(super) fn finish_input(&mut self, storage: &Storage, txid: u32) -> Result<(), SqlError> {
        let collations = self.collations;
        let bytes = self.input_bytes.as_slice();
        heap_sort(self.inputs.as_mut_slice(), |left, right| {
            let left_row = &bytes[left.0 as usize..(left.0 + left.1) as usize];
            let right_row = &bytes[right.0 as usize..(right.0 + right.1) as usize];
            Ok(
                match compare_keys(storage, txid, collations, left_row, right_row)? {
                    core::cmp::Ordering::Equal => left.0 < right.0,
                    ordering => ordering.is_lt(),
                },
            )
        })?;
        self.phase = Phase::Folding { next: 0 };
        Ok(())
    }

    /// Folds every group not yet folded; a failure keeps the groups folded
    /// so far. The saved input is released once every group is folded.
    pub(super) fn fold(
        &mut self,
        scope: &QueryScope<'_>,
        storage: &Storage,
        txid: u32,
        work: &Arena,
        params: &[Datum<'_>],
    ) -> Result<(), SqlError> {
        let Phase::Folding { mut next } = self.phase else {
            return Ok(());
        };
        let total = self.inputs.len();
        // A query without GROUP BY has one group, even over no rows.
        let global = self.keys.is_empty();
        while next < total || global && self.groups.is_empty() {
            check_timeout()?;
            let bytes = self.input_bytes.as_slice();
            let mut end = next;
            while end < total {
                if end > next
                    && !compare_keys(
                        storage,
                        txid,
                        self.collations,
                        record(&self.inputs, bytes, next),
                        record(&self.inputs, bytes, end),
                    )?
                    .is_eq()
                {
                    break;
                }
                end += 1;
            }
            let mark = work.mark();
            let folded = fold_group(
                self.keys,
                self.set,
                self.aggregates,
                scope,
                storage,
                txid,
                (next..end).map(|index| record(&self.inputs, bytes, index)),
                work,
                params,
            )
            .and_then(|group| push_record(&mut self.groups, &mut self.group_bytes, group));
            // SAFETY: the group's record was copied into its lease.
            unsafe { work.rewind_to(mark) };
            folded?;
            next = end;
            self.phase = Phase::Folding { next };
        }
        let pool = storage.plan_build();
        self.inputs = pool.lease(0).map_err(group_full)?;
        self.input_bytes = pool.lease(0).map_err(group_full)?;
        self.phase = Phase::Output { next: 0 };
        Ok(())
    }

    /// Makes the next group current; `false` at the end.
    pub(super) fn advance(&mut self) -> bool {
        let Phase::Output { next } = &mut self.phase else {
            return false;
        };
        if *next < self.groups.len() {
            self.current = Some(*next);
            *next += 1;
            true
        } else {
            self.current = None;
            false
        }
    }

    /// Runs `each` over the current group: its key values and aggregate
    /// results bound as `HAVING` and the output list read them.
    pub(super) fn with_group<'work, R>(
        &'work self,
        scope: &QueryScope<'_>,
        work: &'work Arena,
        hooks: &EvalHooks<'_, 'work>,
        each: impl FnOnce(
            &crate::sql::query::materialize::ScopeSchema<'_, '_>,
            &EvalHooks<'_, 'work>,
        ) -> Result<R, SqlError>,
    ) -> Result<R, SqlError>
    where
        'a: 'work,
    {
        let index = self.current.ok_or_else(|| {
            sql_err!(
                sqlstate::INTERNAL_ERROR,
                "retained grouping has no current group"
            )
        })?;
        let group = record(&self.groups, self.group_bytes.as_slice(), index);
        let values = work
            .alloc_slice_with(self.keys.len() + self.aggregates.len(), |_| Datum::Null)
            .map_err(|_| arena_full())?;
        crate::sql::exec::decode_projected_row_into(group, work, values)?;
        let (keys, results) = values.split_at(self.keys.len());
        let group_hooks = EvalHooks {
            group: Some((self.keys, keys, self.set)),
            aggs: Some((self.pointers, results)),
            ..*hooks
        };
        each(
            &crate::sql::query::materialize::ScopeSchema(scope),
            &group_hooks,
        )
    }

    /// Whether the current group passes `HAVING`.
    pub(super) fn having_passes<'work>(
        &'work self,
        scope: &QueryScope<'_>,
        work: &'work Arena,
        params: &[Datum<'work>],
        hooks: &EvalHooks<'_, 'work>,
    ) -> Result<bool, SqlError>
    where
        'a: 'work,
    {
        let Some(having) = self.having else {
            return Ok(true);
        };
        self.with_group(scope, work, hooks, |row, hooks| {
            match eval_full(having, work, params, row, hooks)? {
                Datum::Bool(true) => Ok(true),
                Datum::Bool(false) | Datum::Null => Ok(false),
                _ => Err(sql_err!(
                    sqlstate::DATATYPE_MISMATCH,
                    "argument of HAVING must be type boolean"
                )),
            }
        })
    }
}

/// Folds one group's aggregates over its saved input rows, in input order,
/// and encodes its keys (its first row's) and results.
#[allow(clippy::too_many_arguments)]
fn fold_group<'w>(
    keys: &'w [&'w Expr<'w>],
    set: GroupingSet<'w>,
    aggregates: AggregateNodes<'w>,
    scope: &QueryScope<'_>,
    storage: &Storage,
    txid: u32,
    mut rows: impl Iterator<Item = &'w [u8]>,
    work: &'w Arena,
    params: &[Datum<'w>],
) -> Result<&'w [u8], SqlError> {
    let catalog = StorageCatalog {
        storage,
        routine_workspace: work,
        txid,
        invocations: None,
        statement_arena: None,
    };
    let hooks = EvalHooks {
        catalog: Some(&catalog),
        ..crate::sql::eval::NO_HOOKS
    };
    let states = work
        .alloc_slice_with(aggregates.len(), |_| AggState::default())
        .map_err(|_| arena_full())?;
    for (state, (_, node)) in states.iter_mut().zip(aggregates) {
        state.init(node, storage, txid, &super::ScopeCols(scope), work)?;
    }
    let mut offsets = [0usize; MAX_RETAINED_SOURCES];
    let mut width = keys.len();
    for (table, offset) in offsets.iter_mut().enumerate().take(scope.n) {
        *offset = width + 1;
        width += 1 + scope.row_width(table);
    }
    let input = work
        .alloc_slice_with(width, |_| Datum::Null)
        .map_err(|_| arena_full())?;
    let output = work
        .alloc_slice_with(keys.len() + aggregates.len(), |_| Datum::Null)
        .map_err(|_| arena_full())?;
    let mut first = true;
    for row in rows.by_ref() {
        crate::sql::exec::decode_projected_row_into(row, work, input)?;
        if first {
            output[..keys.len()].copy_from_slice(&input[..keys.len()]);
            first = false;
        }
        let row = scan::JoinRow {
            scope,
            values: scan::JoinRowValues::Flat {
                values: input,
                offsets: &offsets[..scope.n],
            },
            rowids: &[None; MAX_RETAINED_SOURCES][..scope.n],
        };
        for (state, (_, node)) in states.iter_mut().zip(aggregates) {
            state.update(node, work, params, &row, &hooks)?;
        }
    }
    let (key_values, results) = output.split_at_mut(keys.len());
    let direct_hooks = EvalHooks {
        group: Some((keys, &*key_values, set)),
        ..hooks
    };
    for (result, (state, (_, node))) in results.iter_mut().zip(states.iter_mut().zip(aggregates)) {
        state.initialize_direct_inputs(
            node,
            work,
            params,
            &crate::sql::query::materialize::ScopeSchema(scope),
            &direct_hooks,
        )?;
        *result = state.finish(work, hooks.catalog)?;
    }
    let length = crate::sql::exec::projected_row_len(output)?;
    let bytes = work
        .alloc_slice_with(length, |_| 0u8)
        .map_err(|_| arena_full())?;
    crate::sql::exec::encode_projected_into(output, bytes)?;
    Ok(bytes)
}
