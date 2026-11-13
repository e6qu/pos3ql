//! The FROM items' row joined to an `UPDATE ... FROM` or `DELETE ... USING`
//! target row.
//!
//! PostgreSQL joins the target with the FROM items once. When another
//! transaction changed a selected target row, read committed's EvalPlanQual
//! recheck evaluates the statement's conditions over the target's latest
//! version and the FROM rows that joined it, without joining again; the
//! assignments then read those same FROM rows
//! ([`EvalPlanQual`](https://github.com/postgres/postgres/blob/REL_18_6/src/backend/executor/execMain.c)).
//! pos3ql retains the joined FROM row with each selected target row.

use crate::mem::arena::Arena;
use crate::sql::ast::{Expr, FromClause};
use crate::sql::eval::{ColumnLookup, EvalHooks, SequenceAccess, SqlError};
use crate::sql::types::Datum;
use crate::storage::Storage;

use super::scan::{
    BoundRow, Chained, JoinRow, PaxFullRowReason, PaxReadDemand, pax_column_demand,
    scan_source_recycling_retaining_match_with_pax_columns,
};
use super::{
    ProceduralSubqueries, QueryScope, StorageCatalog, arena_full, collect_table_sample_expressions,
    uncorrelated_subquery_hooks,
};

/// The FROM items' columns of one joined row, encoded in the statement
/// arena's persistent tail so per-row rewinds and later storage changes leave
/// it intact. The encoding holds every source's columns in FROM order, then
/// whether each source joined (an outer join's null-extended side did not),
/// then each base-table source's row identity.
#[derive(Clone, Copy)]
pub(crate) struct JoinedFromRow<'a>(&'a [u8]);

impl<'a> JoinedFromRow<'a> {
    fn encode(row: &JoinRow<'_, '_, '_>, arena: &'a Arena) -> Result<Self, SqlError> {
        let scope = row.scope;
        // Every stored column, a parenthesized join's members included.
        let columns = scope.stored_columns();
        let width = columns + 2 * scope.n;
        let value = |index: usize| {
            if index >= columns + scope.n {
                let table = index - columns - scope.n;
                return row
                    .rowids
                    .get(table)
                    .copied()
                    .flatten()
                    .and_then(|rowid| i64::try_from(rowid).ok())
                    .map_or(Datum::Null, Datum::Int8);
            }
            if index >= columns {
                return Datum::Bool(row.table_present(index - columns) == Some(true));
            }
            let mut column = index;
            for table in 0..scope.n {
                let n_columns = scope.stored_width(table);
                if column < n_columns {
                    return row
                        .table_values(table)
                        .and_then(|values| values.get(column).copied())
                        .unwrap_or(Datum::Null);
                }
                column -= n_columns;
            }
            unreachable!("joined FROM column is within the scope's width")
        };
        let len = crate::sql::exec::projected_row_len_by(width, value)?;
        let output = arena
            .alloc_persistent_slice_with(len, |_| 0u8)
            .map_err(|_| arena_full())?;
        let len = crate::sql::exec::encode_projected_by_into(width, value, output)?;
        Ok(Self(&output[..len]))
    }

    /// The sources of this row, bound for a scope resolved from the same FROM
    /// clause: `None` for a null-extended source.
    fn bind(self, scope: &QueryScope<'_>, arena: &'a Arena) -> Result<BoundSources<'a>, SqlError> {
        let columns = scope.stored_columns();
        let width = columns + 2 * scope.n;
        let decoded = arena
            .alloc_slice_with(width, |_| Datum::Null)
            .map_err(|_| arena_full())?;
        if crate::sql::exec::decode_projected_row_into(self.0, arena, decoded)? != width {
            return Err(crate::sql_err!(
                crate::sql::eval::sqlstate::INTERNAL_ERROR,
                "retained FROM row does not match its FROM clause"
            ));
        }
        let decoded: &'a [Datum<'a>] = decoded;
        let mut offset = 0;
        let bound = arena
            .alloc_slice_with(scope.n, |table| {
                let n_columns = scope.stored_width(table);
                let values = &decoded[offset..offset + n_columns];
                offset += n_columns;
                matches!(decoded[columns + table], Datum::Bool(true))
                    .then_some(BoundRow::Values(values))
            })
            .map_err(|_| arena_full())?;
        let rowids = arena
            .alloc_slice_with(scope.n, |table| match decoded[columns + scope.n + table] {
                Datum::Int8(rowid) => u64::try_from(rowid).ok(),
                _ => None,
            })
            .map_err(|_| arena_full())?;
        Ok(BoundSources { bound, rowids })
    }
}

/// A retained FROM row's sources, as a joined row binds them.
struct BoundSources<'a> {
    bound: &'a [Option<BoundRow<'a>>],
    rowids: &'a [Option<u64>],
}

/// Receives each joined row with its lookup over the target row and the
/// scan's hooks; `false` stops the scan.
type FromMatch<'m, 'a> = dyn FnMut(&JoinRow<'_, 'a, '_>, &dyn ColumnLookup<'a>, &EvalHooks<'_, 'a>) -> Result<bool, SqlError>
    + 'm;

/// Scans `from` with the target row `target` as the outer scope and calls
/// `on_match` for each joined row that satisfies `where_clause`, until it
/// returns `false`. `observed` lists the other expressions that read the
/// joined row; `full_rows` reads every column of each source instead.
#[expect(clippy::too_many_arguments, reason = "joined DML scan inputs")]
pub(super) fn scan_from_matches<'a>(
    storage: &'a Storage,
    from: &'a FromClause<'a>,
    txid: u32,
    where_clause: Option<&'a Expr<'a>>,
    observed: &[&'a Expr<'a>],
    full_rows: bool,
    arena: &'a Arena,
    params: &[Datum<'a>],
    target: &dyn ColumnLookup<'a>,
    sequences: Option<&dyn SequenceAccess>,
    hashed: Option<&'a [u64]>,
    on_match: &mut FromMatch<'_, 'a>,
) -> Result<(), SqlError> {
    let mut scope = QueryScope::resolve_exec_outer(
        storage,
        from,
        where_clause,
        txid,
        arena,
        params,
        sequences,
        Some(target),
    )?;
    scope.target_hash_candidates = hashed;
    let subquery_expressions = arena
        .alloc_slice_with(1 + 2 * scope.n, |_| None)
        .map_err(|_| arena_full())?;
    subquery_expressions[0] = where_clause;
    collect_table_sample_expressions(from, &mut subquery_expressions[1..]);
    // A subquery reading the target or a FROM row runs per joined row.
    let subs = uncorrelated_subquery_hooks(
        subquery_expressions,
        storage,
        txid,
        None,
        arena,
        params,
        Some(target),
        sequences,
    )?;
    let correlated_subqueries = ProceduralSubqueries { storage, txid };
    let catalog = StorageCatalog {
        storage,
        routine_workspace: arena,
        txid,
        invocations: None,
        statement_arena: None,
    };
    let hooks = EvalHooks {
        subs: Some(&subs),
        subquery_executor: Some(&correlated_subqueries),
        catalog: Some(&catalog),
        sequences,
        ..crate::sql::eval::NO_HOOKS
    };
    let pax_columns = if full_rows {
        PaxReadDemand::full_row(PaxFullRowReason::RetainedJoinRow)
    } else {
        let mut expressions = [&Expr::Null; crate::sql::exec::MAX_PROJ + 1];
        let mut expression_count = 0usize;
        for expression in where_clause.into_iter().chain(observed.iter().copied()) {
            expressions[expression_count] = expression;
            expression_count += 1;
        }
        pax_column_demand(&scope, from, &expressions[..expression_count])
    };
    let retain_match = core::cell::Cell::new(false);
    scan_source_recycling_retaining_match_with_pax_columns(
        storage,
        &scope,
        from,
        txid,
        where_clause,
        arena,
        params,
        &hooks,
        Some(target),
        pax_columns,
        &retain_match,
        &mut |row| {
            let joined = Chained {
                inner: row,
                outer: Some(target),
            };
            let more = on_match(row, &joined, &hooks)?;
            retain_match.set(!more);
            Ok(more)
        },
    )
}

/// The first joined row of `from` that satisfies `where_clause` for the
/// target row `target`, retained in `retain`'s persistent tail.
#[expect(clippy::too_many_arguments, reason = "joined DML scan inputs")]
pub(crate) fn joined_from_match<'a, 'r>(
    storage: &'a Storage,
    from: &'a FromClause<'a>,
    txid: u32,
    where_clause: Option<&'a Expr<'a>>,
    arena: &'a Arena,
    params: &[Datum<'a>],
    target: &dyn ColumnLookup<'a>,
    sequences: Option<&dyn SequenceAccess>,
    hashed: Option<&'a [u64]>,
    retain: &'r Arena,
) -> Result<Option<JoinedFromRow<'r>>, SqlError> {
    let mut found = None;
    scan_from_matches(
        storage,
        from,
        txid,
        where_clause,
        &[],
        true,
        arena,
        params,
        target,
        sequences,
        hashed,
        &mut |row, _, _| {
            found = Some(JoinedFromRow::encode(row, retain)?);
            Ok(false)
        },
    )?;
    Ok(found)
}

/// Read committed's recheck of a target row that another transaction
/// changed. PostgreSQL's plan produced every FROM row the version `first`
/// joined, in join order, and tries each with the latest version `latest`
/// until one still meets the statement's conditions; the first FROM row
/// joined is the one this statement retained, unless it fails the recheck.
/// `None` skips the target row.
#[expect(clippy::too_many_arguments, reason = "joined DML scan inputs")]
pub(crate) fn recheck_joined_from_match<'a, 'r>(
    storage: &'a Storage,
    from: &'a FromClause<'a>,
    txid: u32,
    where_clause: Option<&'a Expr<'a>>,
    arena: &'a Arena,
    params: &[Datum<'a>],
    first: &dyn ColumnLookup<'a>,
    latest: &dyn ColumnLookup<'a>,
    sequences: Option<&dyn SequenceAccess>,
    retain: &'r Arena,
) -> Result<Option<JoinedFromRow<'r>>, SqlError> {
    let mut found = None;
    scan_from_matches(
        storage,
        from,
        txid,
        where_clause,
        &[],
        true,
        arena,
        params,
        first,
        sequences,
        None,
        &mut |row, joined_first, hooks| {
            let joined_latest = Chained {
                inner: row,
                outer: Some(latest),
            };
            let passes = match where_clause {
                Some(predicate) => super::locked_row_where_passes(
                    storage,
                    txid,
                    predicate,
                    joined_first,
                    &joined_latest,
                    arena,
                    params,
                    hooks,
                )?,
                None => true,
            };
            if passes {
                found = Some(JoinedFromRow::encode(row, retain)?);
            }
            Ok(!passes)
        },
    )?;
    Ok(found)
}

/// Calls `each` with the target row `target` joined to the retained FROM row
/// `row`, the scope assignments and `RETURNING` read. `matched_row` receives
/// the FROM items' values for `RETURNING`.
#[expect(clippy::too_many_arguments, reason = "joined DML scope inputs")]
pub(crate) fn with_joined_from_row<'a, 'r, R>(
    storage: &'a Storage,
    from: &'a FromClause<'a>,
    txid: u32,
    where_clause: Option<&'a Expr<'a>>,
    arena: &'a Arena,
    params: &[Datum<'a>],
    sequences: Option<&dyn SequenceAccess>,
    target: &dyn ColumnLookup<'a>,
    row: JoinedFromRow<'a>,
    each: impl FnOnce(&dyn ColumnLookup<'a>) -> Result<R, SqlError>,
    matched_row: Option<super::MatchedFromRow<'_, 'r>>,
) -> Result<R, SqlError> {
    let scope = QueryScope::resolve_exec_outer(
        storage,
        from,
        where_clause,
        txid,
        arena,
        params,
        sequences,
        Some(target),
    )?;
    let sources = row.bind(&scope, arena)?;
    let order = arena
        .alloc_slice_with(scope.n, |table| table)
        .map_err(|_| arena_full())?;
    let sources = super::materialize::sources_row(&scope, sources.bound, order, sources.rowids);
    if let Some(matched_row) = matched_row {
        *matched_row.values = super::detached_join_values(&sources, arena, matched_row.arena)?;
    }
    each(&Chained {
        inner: &sources,
        outer: Some(target),
    })
}

/// The outcome of rechecking a pulled-up `LATERAL` subquery that reads tables.
pub(super) enum LateralRowRecheck<'a> {
    /// The subquery row's values for the latest outer row.
    Rerun(&'a [Datum<'a>]),
    /// Its conditions no longer hold for the latest outer row.
    Lost,
    /// Several of its source rows produced the row the statement read and
    /// recheck differently, so the one PostgreSQL kept is unknown.
    Ambiguous,
}

/// Rechecks a simple `LATERAL` subquery with `FROM` that PostgreSQL pulls up
/// into the outer query: its source rows keep the versions first joined, its
/// `WHERE` runs again with the latest outer row, and its output is
/// projected again. The source row is the one whose output, for the outer
/// row as read (`as_read`), equals the values the statement read (`read`).
#[expect(clippy::too_many_arguments, reason = "locked row recheck inputs")]
pub(super) fn recheck_lateral_select<'a>(
    storage: &'a Storage,
    txid: u32,
    select: &'a crate::sql::ast::Select<'a>,
    arena: &'a Arena,
    params: &[Datum<'a>],
    as_read: &dyn ColumnLookup<'a>,
    latest: &dyn ColumnLookup<'a>,
    read: &[Datum<'a>],
    sequences: Option<&dyn SequenceAccess>,
) -> Result<LateralRowRecheck<'a>, SqlError> {
    let from = select.from.as_ref().ok_or_else(|| {
        crate::sql_err!(
            crate::sql::eval::sqlstate::INTERNAL_ERROR,
            "a pulled-up LATERAL recheck has no FROM clause"
        )
    })?;
    let mut found: Option<Option<&'a [u8]>> = None;
    let mut ambiguous = false;
    let mut values = [Datum::Null; crate::sql::exec::MAX_PROJ];
    scan_from_matches(
        storage,
        from,
        txid,
        select.where_clause,
        &[],
        true,
        arena,
        params,
        as_read,
        sequences,
        None,
        &mut |row, joined_as_read, hooks| {
            let width = project_items(
                select,
                row,
                joined_as_read,
                arena,
                params,
                hooks,
                &mut values,
            )?;
            if width != read.len() || values[..width] != *read {
                return Ok(true);
            }
            let joined_latest = Chained {
                inner: row,
                outer: Some(latest),
            };
            let passes = match select.where_clause {
                Some(predicate) => super::locked_row_where_passes(
                    storage,
                    txid,
                    predicate,
                    joined_as_read,
                    &joined_latest,
                    arena,
                    params,
                    hooks,
                )?,
                None => true,
            };
            let outcome = if passes {
                let width = project_items(
                    select,
                    row,
                    &joined_latest,
                    arena,
                    params,
                    hooks,
                    &mut values,
                )?;
                let len = crate::sql::exec::projected_row_len_by(width, |column| values[column])?;
                let output = arena
                    .alloc_persistent_slice_with(len, |_| 0u8)
                    .map_err(|_| arena_full())?;
                let len = crate::sql::exec::encode_projected_by_into(
                    width,
                    |column| values[column],
                    output,
                )?;
                Some(&output[..len])
            } else {
                None
            };
            // Source rows that produced the same values qualify alike unless
            // their rechecks differ.
            match found {
                None => found = Some(outcome),
                Some(earlier) if earlier == outcome => {}
                Some(_) => {
                    ambiguous = true;
                    return Ok(false);
                }
            }
            Ok(true)
        },
    )?;
    if ambiguous {
        return Ok(LateralRowRecheck::Ambiguous);
    }
    match found {
        None => Ok(LateralRowRecheck::Ambiguous),
        Some(None) => Ok(LateralRowRecheck::Lost),
        Some(Some(bytes)) => {
            let width = crate::sql::exec::projected_row_width(bytes);
            let values = arena
                .alloc_slice_with(width, |_| Datum::Null)
                .map_err(|_| arena_full())?;
            crate::sql::exec::decode_projected_row_into(bytes, arena, values)?;
            Ok(LateralRowRecheck::Rerun(values))
        }
    }
}

/// Evaluates `select`'s output list over one joined source row.
fn project_items<'a>(
    select: &'a crate::sql::ast::Select<'a>,
    row: &JoinRow<'_, 'a, '_>,
    lookup: &dyn ColumnLookup<'a>,
    arena: &'a Arena,
    params: &[Datum<'a>],
    hooks: &EvalHooks<'_, 'a>,
    out: &mut [Datum<'a>],
) -> Result<usize, SqlError> {
    use super::scope::ResolvedColumn;
    let scope = row.scope;
    let column = |entry: ResolvedColumn| -> Result<Datum<'a>, SqlError> {
        match entry {
            ResolvedColumn::Table(table, column) => Ok(row
                .table_values(table)
                .and_then(|values| values.get(column).copied())
                .unwrap_or(Datum::Null)),
            ResolvedColumn::Merged(merged) => lookup.lookup(None, scope.merged[merged].name),
        }
    };
    let mut width = 0;
    let mut push = |value: Datum<'a>| -> Result<(), SqlError> {
        *out.get_mut(width).ok_or_else(|| {
            crate::sql_err!(
                crate::sql::eval::sqlstate::PROGRAM_LIMIT_EXCEEDED,
                "target lists can have at most {} entries",
                crate::sql::exec::MAX_PROJ
            )
        })? = value;
        width += 1;
        Ok(())
    };
    for item in select.items {
        match item {
            crate::sql::ast::SelectItem::Expr { expression, .. } => {
                push(crate::sql::eval::eval_full(
                    expression, arena, params, &lookup, hooks,
                )?)?;
            }
            crate::sql::ast::SelectItem::Wildcard(_) => {
                for index in 0..scope.star_columns() {
                    push(column(scope.star_entry(index))?)?;
                }
            }
            crate::sql::ast::SelectItem::TableWildcard(qualifier) => {
                for index in 0..scope.qualified_star_columns(qualifier)? {
                    push(column(scope.qualified_star_entry(qualifier, index)?)?)?;
                }
            }
            crate::sql::ast::SelectItem::RecordStar(_) => {
                return Err(crate::sql_err!(
                    crate::sql::eval::sqlstate::FEATURE_NOT_SUPPORTED,
                    "a concurrently updated row locked with a LATERAL subquery that reads tables cannot be rechecked"
                ));
            }
        }
    }
    Ok(width)
}

/// The hash join of a data-modifying statement's target with its only FROM
/// item: the target probes, and the FROM item, a table without descendants,
/// row security or sampling, is hashed once on its join keys. The SELECT
/// planner's choice decides it, so EXPLAIN and execution agree.
#[derive(Clone, Copy)]
pub(crate) struct TargetHashPlan {
    slot: usize,
    /// Equi-key column pairs (target column, FROM item column).
    keys: [(usize, usize); 8],
    key_count: usize,
    collations: [crate::sql::ast::Collation; 8],
}

impl TargetHashPlan {
    /// The equi-key column pairs (target column, FROM item column).
    pub(crate) fn keys(&self) -> &[(usize, usize)] {
        &self.keys[..self.key_count]
    }
}

/// Selects the hash join of `target` with `from`, when the SELECT planner
/// would hash this join of two relations.
pub(crate) fn target_hash_plan<'a>(
    storage: &'a Storage,
    target: crate::sql::ast::QualName<'a>,
    alias: Option<&'a str>,
    from: &'a FromClause<'a>,
    where_clause: Option<&'a Expr<'a>>,
    txid: u32,
    arena: &'a Arena,
) -> Result<Option<TargetHashPlan>, SqlError> {
    if from.base.sample.is_some() || from.base.lateral {
        return Ok(None);
    }
    // Only the first FROM item is hashed; the conditions decide every
    // joined row afterwards, so later items join its candidates as before.
    // A comma list the planner reorders hashes only when the first item
    // still runs first.
    if from.joins.iter().all(|join| join.kind.is_cross()) && !from.joins.is_empty() {
        let schema = QueryScope::resolve_schema(storage, from, txid, arena, None)?;
        if schema.lateral[..schema.n].iter().any(|&lateral| lateral)
            || super::join_order(storage, &schema, where_clause, arena)?[0] != 0
        {
            return Ok(None);
        }
    }
    let first = &*arena
        .alloc(FromClause {
            base: from.base,
            joins: &[],
        })
        .map_err(|_| arena_full())?;
    let (scope, joined) = super::dependencies::dml_join_scope(
        storage,
        target,
        alias,
        Some(first),
        txid,
        arena,
        None,
    )?;
    if scope.n != 2 || scope.derived[1].is_some() || scope.slots[1] == usize::MAX {
        return Ok(None);
    }
    let slot = scope.slots[1];
    let Some(role) = storage.current_role_slot(txid) else {
        return Ok(None);
    };
    let role = scope.authorization_roles[1].map_or(role, usize::from);
    if storage.table_def(slot, txid).kind == crate::storage::TableKind::Foreign
        || storage.relation_has_descendants(slot, txid)
        || storage.row_security_applies(slot, role, txid)
    {
        return Ok(None);
    }
    // An index on the FROM item's own conditions selects its rows instead.
    let source = QueryScope::resolve_schema(storage, first, txid, arena, None)?;
    if super::index_access_plan(storage, &source, txid, where_clause, arena)?.is_some() {
        return Ok(None);
    }
    let Some(plan) =
        super::select_hash_join_plan(storage, &scope, joined, where_clause, &[0, 1], txid, arena)?
    else {
        return Ok(None);
    };
    let layout = plan.key_layout();
    if layout.probe_table != 0 || layout.build_table != 1 {
        return Ok(None);
    }
    // A key column the conditions equate with a constant makes the join
    // equality implied, as PostgreSQL's equivalence classes do: both sides
    // are filtered by the constant and the join keeps no hash key.
    let mut conjuncts = [&Expr::Null; super::plan::MAX_CONJUNCTS];
    let mut count = 0;
    if let Some(condition) = where_clause
        && super::plan::flatten_and(condition, &mut conjuncts, &mut count)
        && conjuncts[..count].iter().any(|conjunct| {
            layout.keys.iter().any(|&(target_column, source_column)| {
                super::scan::fixes_column(&scope, conjunct, (0, target_column))
                    || super::scan::fixes_column(&scope, conjunct, (1, source_column))
            })
        })
    {
        return Ok(None);
    }
    let mut keys = [(0, 0); 8];
    let mut collations = [crate::sql::ast::Collation::None; 8];
    keys[..layout.keys.len()].copy_from_slice(layout.keys);
    collations[..layout.keys.len()].copy_from_slice(layout.collations);
    Ok(Some(TargetHashPlan {
        slot,
        keys,
        key_count: layout.keys.len(),
        collations,
    }))
}

#[cfg(test)]
std::thread_local! {
    static TARGET_HASH_BUILDS: core::cell::Cell<u64> = const { core::cell::Cell::new(0) };
}

/// Target hash joins this thread built, for tests of the selected plan.
#[cfg(test)]
pub(crate) fn target_hash_builds() -> u64 {
    TARGET_HASH_BUILDS.with(core::cell::Cell::get)
}

/// A built [`TargetHashPlan`]: the FROM item's visible rows in the order its
/// sequential scan reads them, chained by join-key hash.
pub(crate) struct TargetHash<'a> {
    plan: TargetHashPlan,
    rowids: &'a [u64],
    hashes: &'a [u64],
    /// Bucket heads, then each row's next row in its bucket, both
    /// `u32::MAX`-terminated; each chain lists rows in reverse scan order.
    buckets: &'a [u32],
    next: &'a [u32],
}

impl TargetHashPlan {
    /// Reads the FROM item once, as the hash join's build side.
    pub(crate) fn build<'a>(
        self,
        storage: &Storage,
        txid: u32,
        arena: &'a Arena,
    ) -> Result<TargetHash<'a>, SqlError> {
        use crate::storage::MAX_COLUMNS;
        let slot = self.slot;
        let full = |_| arena_full();
        #[cfg(test)]
        TARGET_HASH_BUILDS.with(|builds| builds.set(builds.get() + 1));
        storage.record_relation_scan(txid, slot, None, 0, crate::sql::datetime::now_micros())?;
        let count = storage.visible_row_count(slot, txid)?;
        if count >= u32::MAX as usize {
            return Err(arena_full());
        }
        let table_snapshot = storage.table_row_snapshot(slot, Some(arena))?;
        let ordered = arena
            .alloc_slice_with(count, |_| None::<crate::storage::OrderedRowSnapshot>)
            .map_err(full)?;
        let mut fill = 0usize;
        storage.for_each_row_state(slot, &mut |rowid, state| {
            if let Some(home) = storage.visible_row_home(slot, rowid, state, txid)? {
                ordered[fill] = Some(table_snapshot.retain(rowid, &home)?);
                fill += 1;
            }
            Ok(core::ops::ControlFlow::Continue(()))
        })?;
        // The order the nested loop's sequential scan reads candidates in.
        ordered[..fill].sort_unstable_by_key(|row| row.expect("row snapshot").sort_key());
        let rowids = arena.alloc_slice_with(fill, |_| 0u64).map_err(full)?;
        let hashes = arena.alloc_slice_with(fill, |_| 0u64).map_err(full)?;
        let next = arena.alloc_slice_with(fill, |_| u32::MAX).map_err(full)?;
        let buckets = arena
            .alloc_slice_with(fill.max(1).next_power_of_two(), |_| u32::MAX)
            .map_err(full)?;
        let definition = storage.table_def(slot, txid);
        let mut schema = [crate::sql::types::ColType::Bool; MAX_COLUMNS];
        definition.schema(&mut schema);
        let schema = &schema[..definition.n_columns];
        let hash_columns: [u16; 8] = [0, 1, 2, 3, 4, 5, 6, 7];
        let mask = buckets.len() - 1;
        // As PostgreSQL's hash table, each chain lists later rows first, so
        // a target row joining several FROM rows keeps the last one scanned.
        for index in 0..fill {
            let row = ordered[index].expect("row snapshot");
            let rowid = row.rowid();
            let home = row.read()?;
            storage.record_relation_tuple_read(txid, slot, None)?;
            rowids[index] = rowid;
            let mark = arena.mark();
            let hash = (|| -> Result<Option<u64>, SqlError> {
                let bytes = storage.row_bytes(slot, rowid, home, arena)?;
                let mut values = [Datum::Null; MAX_COLUMNS];
                crate::storage::rowenc::decode(bytes, schema, &mut values)?;
                let mut keys = [Datum::Null; 8];
                for (key, &(_, column)) in keys.iter_mut().zip(self.keys()) {
                    *key = values[column];
                    if key.is_null() {
                        return Ok(None);
                    }
                }
                crate::sql::eval::hash_key_collated_catalog(
                    storage,
                    txid,
                    &keys[..self.key_count],
                    &hash_columns[..self.key_count],
                    &self.collations[..self.key_count],
                )
                .map(Some)
            })();
            // SAFETY: only the key hash outlives the decoded row.
            unsafe { arena.rewind_to(mark) };
            // A NULL key equals nothing, so its row joins no target row.
            let Some(hash) = hash? else { continue };
            hashes[index] = hash;
            let bucket = hash as usize & mask;
            next[index] = buckets[bucket];
            buckets[bucket] = index as u32;
        }
        Ok(TargetHash {
            plan: self,
            rowids,
            hashes,
            buckets,
            next,
        })
    }
}

impl<'a> TargetHash<'a> {
    /// The FROM item's rows whose join keys hash like the target row's,
    /// later rows first. `target` holds the target's logical columns.
    pub(crate) fn candidates(
        &self,
        storage: &Storage,
        txid: u32,
        target: &[Datum<'_>],
        arena: &'a Arena,
    ) -> Result<&'a [u64], SqlError> {
        let plan = &self.plan;
        let mut keys = [Datum::Null; 8];
        for (key, &(column, _)) in keys.iter_mut().zip(plan.keys()) {
            *key = target.get(column).copied().unwrap_or(Datum::Null);
            if key.is_null() {
                return Ok(&[]);
            }
        }
        let hash_columns: [u16; 8] = [0, 1, 2, 3, 4, 5, 6, 7];
        let hash = crate::sql::eval::hash_key_collated_catalog(
            storage,
            txid,
            &keys[..plan.key_count],
            &hash_columns[..plan.key_count],
            &plan.collations[..plan.key_count],
        )?;
        let head = self.buckets[hash as usize & (self.buckets.len() - 1)];
        let chain = || {
            core::iter::successors((head != u32::MAX).then_some(head), |&entry| {
                let next = self.next[entry as usize];
                (next != u32::MAX).then_some(next)
            })
            .map(|entry| entry as usize)
            .filter(move |&entry| self.hashes[entry] == hash)
        };
        let count = chain().count();
        let mut entries = chain();
        arena
            .alloc_slice_with(count, |_| {
                self.rowids[entries.next().expect("chain is stable")]
            })
            .map_err(|_| arena_full())
            .map(|rowids| &*rowids)
    }
}
