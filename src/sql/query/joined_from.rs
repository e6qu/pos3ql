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
/// then each base-table source's row identity, then each source's
/// `tableoid` (the leaf table its row came from).
#[derive(Clone, Copy)]
pub(crate) struct JoinedFromRow<'a>(&'a [u8]);

impl<'a> JoinedFromRow<'a> {
    /// A joined row saved with a write's progress, as [`Self::bytes`] gave it.
    pub(crate) fn from_bytes(bytes: &'a [u8]) -> Self {
        Self(bytes)
    }

    pub(crate) fn bytes(self) -> &'a [u8] {
        self.0
    }

    fn encode(row: &JoinRow<'_, '_, '_>, arena: &'a Arena) -> Result<Self, SqlError> {
        let scope = row.scope;
        // Every stored column, a parenthesized join's members included.
        let columns = scope.stored_columns();
        let width = columns + 3 * scope.n;
        for table in (0..scope.n).filter(|&table| scope.stores_rows(table)) {
            row.table_oid(table)?;
        }
        let value = |index: usize| {
            if index >= columns + 2 * scope.n {
                let table = index - columns - 2 * scope.n;
                // Checked above: a stored source's oid reads without error.
                return if scope.stores_rows(table) {
                    row.table_oid(table).unwrap_or(Datum::Null)
                } else {
                    Datum::Null
                };
            }
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
        let width = columns + 3 * scope.n;
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
            .alloc_slice_with(scope.n, |_| None)
            .map_err(|_| arena_full())?;
        for (table, slot) in bound.iter_mut().enumerate() {
            let n_columns = scope.stored_width(table);
            let start = offset;
            offset += n_columns;
            if !matches!(decoded[columns + table], Datum::Bool(true)) {
                continue;
            }
            // A bound row's `tableoid` follows its columns.
            let values = arena
                .alloc_slice_with(n_columns + 1, |index| {
                    if index < n_columns {
                        decoded[start + index]
                    } else {
                        decoded[columns + 2 * scope.n + table]
                    }
                })
                .map_err(|_| arena_full())?;
            let values: &'a [Datum<'a>] = values;
            *slot = Some(BoundRow::Values(if values[n_columns].is_null() {
                &values[..n_columns]
            } else {
                values
            }));
        }
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
            ResolvedColumn::TableOid(table) => row.table_oid(table),
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
/// item: the target, or each member of a partitioned or inheritance target
/// in turn, probes, and the FROM item, a table without descendants,
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
    let Some(plan) = super::select_hash_join_plan(
        storage,
        &scope,
        joined,
        where_clause,
        &[0, 1],
        Some(0),
        false,
        txid,
        arena,
    )?
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

/// How a `MERGE` joins its target and source, as PostgreSQL's costs choose
/// for relations of different sizes: a hash join builds on the side with
/// fewer planned rows and the other side probes it, and without an
/// equi-key a nested loop rescans the smaller side for each row of the
/// larger. A join that keeps the unmatched rows of one side only reads that
/// side as its outer relation when sizes tie (`Hash Left Join`), and always
/// in a nested loop. EXPLAIN and execution share the decision.
#[derive(Clone, Copy)]
pub(crate) struct MergeJoinPlan<'a> {
    /// Unmatched source rows reach a `NOT MATCHED [BY TARGET]` action.
    pub(crate) preserves_source: bool,
    /// Unmatched target rows reach a `NOT MATCHED BY SOURCE` action.
    pub(crate) preserves_target: bool,
    /// The hash join's keys: (target column, source column name).
    keys: [(usize, &'a str); 8],
    key_count: usize,
    collations: [crate::sql::ast::Collation; 8],
    /// The `ON` conjuncts each part of the plan checks: the scan of the
    /// target and of the source (each relation's own conditions, unless the
    /// join keeps its unmatched rows), the hash join's key equalities, and
    /// the join's filter (the rest), each the `AND` of its conjuncts.
    pub(crate) conditions: MergeConditions<'a>,
}

/// Where a `MERGE` checks the conjuncts of its `ON` condition.
#[derive(Clone, Copy, Default)]
pub(crate) struct MergeConditions<'a> {
    pub(crate) target: Option<&'a Expr<'a>>,
    pub(crate) source: Option<&'a Expr<'a>>,
    pub(crate) hash: Option<&'a Expr<'a>>,
    pub(crate) join: Option<&'a Expr<'a>>,
}

impl<'a> MergeJoinPlan<'a> {
    /// The equi-key pairs (target column, source column name) a hash join
    /// builds on; empty for a nested loop.
    pub(crate) fn keys(&self) -> &[(usize, &'a str)] {
        &self.keys[..self.key_count]
    }

    pub(crate) fn collations(&self) -> &[crate::sql::ast::Collation] {
        &self.collations[..self.key_count]
    }

    /// Whether the join is a hash join.
    pub(crate) fn hashed(&self) -> bool {
        self.key_count != 0
    }

    /// The join without its hash keys: the nested loop over the source rows
    /// a `MERGE` of relations without statistics runs.
    pub(crate) fn unplanned(self) -> Self {
        Self {
            key_count: 0,
            ..self
        }
    }

    /// Whether the source rows are the outer relation, the target's being
    /// hashed or rescanned, for `target_rows` and `source_rows` planned rows.
    pub(crate) fn source_outer(&self, target_rows: u64, source_rows: u64) -> bool {
        match (self.preserves_source, self.preserves_target) {
            // A nested loop reads the side it keeps as its outer relation.
            (true, false) if !self.hashed() => true,
            (false, true) | (true, true) if !self.hashed() => false,
            // The smaller side is hashed; a tie hashes the source, unless
            // only unmatched source rows are kept.
            (true, false) => source_rows >= target_rows,
            // The smaller side is hashed, or rescanned by an inner nested
            // loop.
            _ => source_rows > target_rows,
        }
    }
}

/// Plans `statement`'s join of its target and source. A join that keeps
/// both sides' unmatched rows needs an equality between them, as
/// PostgreSQL's full join does (`0A000`).
pub(crate) fn merge_join_plan<'a>(
    storage: &Storage,
    statement: &'a crate::sql::ast::Merge<'a>,
    txid: u32,
    arena: &'a Arena,
) -> Result<MergeJoinPlan<'a>, SqlError> {
    use crate::sql::ast::{MergeActionRef, MergeMatchKind};
    // `DO NOTHING` needs no row, so only other actions keep unmatched rows.
    let preserves = |kind: MergeMatchKind| {
        statement
            .whens
            .iter()
            .any(|when| when.kind() == kind && !matches!(when.action(), MergeActionRef::DoNothing))
    };
    let mut plan = MergeJoinPlan {
        preserves_source: preserves(MergeMatchKind::NotMatchedByTarget),
        preserves_target: preserves(MergeMatchKind::NotMatchedBySource),
        keys: [(0, ""); 8],
        key_count: 0,
        collations: [crate::sql::ast::Collation::None; 8],
        conditions: MergeConditions {
            join: Some(statement.on),
            ..MergeConditions::default()
        },
    };
    let source_from = &*arena
        .alloc(FromClause {
            base: statement.source,
            joins: &[],
        })
        .map_err(|_| arena_full())?;
    let (scope, _) = super::dependencies::dml_join_scope(
        storage,
        statement.target,
        statement.target_alias,
        Some(source_from),
        txid,
        arena,
        None,
    )?;
    if scope.n != 2 {
        return Ok(plan);
    }
    if plan.preserves_source && plan.preserves_target {
        // An equality whose sides each read one relation hashes or merges.
        let mut conjuncts = [statement.on; super::plan::MAX_CONJUNCTS];
        let mut count = 0;
        let conjuncts: &[&Expr<'_>] =
            if super::plan::flatten_and(statement.on, &mut conjuncts, &mut count) {
                &conjuncts[..count]
            } else {
                core::slice::from_ref(&statement.on)
            };
        let joinable = conjuncts.iter().any(|conjunct| {
            let Expr::Binary {
                operator: crate::sql::ast::BinaryOp::Eq,
                left,
                right,
                ..
            } = conjunct
            else {
                return false;
            };
            matches!(
                (
                    super::plan::expr_tables(left, &scope),
                    super::plan::expr_tables(right, &scope)
                ),
                (Some(1), Some(2)) | (Some(2), Some(1))
            )
        });
        if !joinable {
            return Err(crate::sql_err!(
                crate::sql::eval::sqlstate::FEATURE_NOT_SUPPORTED,
                "FULL JOIN is only supported with merge-joinable or hash-joinable join conditions"
            ));
        }
    }
    if crate::sql::guc::planner_methods().hashjoin
        && let Some((pairs, count)) =
            super::scan::hash_join_keys(&scope, Some(statement.on), None, 0, 1)?
    {
        let source = scope.defs.get(1).expect("resolved source");
        for (index, &(target_column, source_column)) in pairs[..count].iter().enumerate() {
            let name = arena
                .alloc_str(source.columns[source_column].name.as_str())
                .map_err(|_| arena_full())?;
            plan.keys[index] = (target_column, name);
            plan.collations[index] =
                super::scan::join_key_collation(&scope, (0, target_column), (1, source_column))
                    .expect("hash keys have a collation");
        }
        plan.key_count = count;
    }
    plan.conditions = merge_conditions(&scope, statement.on, &plan, arena)?;
    Ok(plan)
}

/// Splits a `MERGE`'s `ON` condition among the parts of its plan, as
/// PostgreSQL distributes its conjuncts: one reading only one relation
/// filters that relation's scan unless the join keeps that relation's
/// unmatched rows, an equality the hash join keys on is its hash
/// condition, and the rest is the join's filter.
fn merge_conditions<'a>(
    scope: &QueryScope<'_>,
    on: &'a Expr<'a>,
    plan: &MergeJoinPlan<'_>,
    arena: &'a Arena,
) -> Result<MergeConditions<'a>, SqlError> {
    let mut conjuncts = [on; super::plan::MAX_CONJUNCTS];
    let mut count = 0;
    if !super::plan::flatten_and(on, &mut conjuncts, &mut count) {
        return Ok(MergeConditions {
            join: Some(on),
            ..MergeConditions::default()
        });
    }
    let column = |side: &Expr<'_>| match side {
        Expr::Column { qualifier, name } => match scope.find_column(*qualifier, name) {
            Ok(super::ResolvedColumn::Table(table, column)) => Some((table, column)),
            _ => None,
        },
        _ => None,
    };
    let key = |conjunct: &Expr<'_>| {
        let Expr::Binary {
            operator: crate::sql::ast::BinaryOp::Eq,
            left,
            right,
            ..
        } = conjunct
        else {
            return false;
        };
        let pair = match (column(left), column(right)) {
            (Some((0, target)), Some((1, source))) | (Some((1, source)), Some((0, target))) => {
                (target, source)
            }
            _ => return false,
        };
        let source = scope.defs.get(1).expect("resolved source");
        plan.keys()
            .iter()
            .any(|&(target, name)| target == pair.0 && source.columns[pair.1].name.as_str() == name)
    };
    let mut conditions = MergeConditions::default();
    for &conjunct in &conjuncts[..count] {
        let slot = if plan.hashed() && key(conjunct) {
            &mut conditions.hash
        } else {
            match super::plan::expr_tables(conjunct, scope) {
                Some(1) if !plan.preserves_target => &mut conditions.target,
                Some(2) if !plan.preserves_source => &mut conditions.source,
                _ => &mut conditions.join,
            }
        };
        *slot = super::and_where(*slot, Some(conjunct), arena)?;
    }
    Ok(conditions)
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

/// The join of an `UPDATE ... FROM` or `DELETE ... USING` target with its
/// one FROM table, planned as PostgreSQL plans two relations with
/// statistics: a hash join on the conditions' equalities between them,
/// building on the relation [`crate::sql::explain::joined_write_source_outer`]
/// picks, or without an equality a nested loop rescanning the other
/// relation from a `Materialize`. Each `WHERE` conjunct reading only one
/// relation filters that relation's scan, the key equalities are the hash
/// join's condition and the rest its join filter. Execution walks the
/// join's rows in its plan's order and changes each target row on its first
/// join row; EXPLAIN shows the same plan and counts it.
#[derive(Clone, Copy)]
pub(crate) struct JoinedWriteJoin<'a> {
    /// The FROM table's storage slot and exposed name.
    pub(crate) source_slot: usize,
    pub(crate) source_name: &'a str,
    /// The hash join's keys, each an equality between the relations.
    keys: [JoinedWriteKey<'a>; 8],
    key_count: usize,
    collations: [crate::sql::ast::Collation; 8],
    /// Whether the FROM table is the outer relation, the target being
    /// hashed or rescanned.
    pub(crate) source_outer: bool,
    /// How the join reads its inner relation.
    pub(crate) method: JoinedWriteMethod,
    /// A unique index on the inner relation's hash keys (`Inner Unique`).
    pub(crate) inner_unique: bool,
    /// Where the plan checks the `WHERE` conjuncts: `target` and `source`
    /// filter each relation's scan, `hash` holds the key equalities and
    /// `join` the join's filter.
    pub(crate) conditions: MergeConditions<'a>,
}

/// One hash key of a joined write: the expressions an equality between the
/// target and its FROM table compares, each reading one relation (and
/// routine variables or parameters), and the column each is when it is a
/// plain column. PostgreSQL hashes on any such equality
/// (`hash_inner_and_outer`).
#[derive(Clone, Copy)]
pub(crate) struct JoinedWriteKey<'a> {
    pub(crate) target: &'a Expr<'a>,
    pub(crate) source: &'a Expr<'a>,
    pub(crate) target_column: Option<usize>,
    pub(crate) source_column: Option<usize>,
}

impl JoinedWriteKey<'_> {
    const NONE: Self = Self {
        target: &Expr::Null,
        source: &Expr::Null,
        target_column: None,
        source_column: None,
    };
}

impl<'a> JoinedWriteJoin<'a> {
    /// The hash join's keys; empty for a nested loop.
    pub(crate) fn keys(&self) -> &[JoinedWriteKey<'a>] {
        &self.keys[..self.key_count]
    }

    pub(crate) fn collations(&self) -> &[crate::sql::ast::Collation] {
        &self.collations[..self.key_count]
    }

    pub(crate) fn hashed(&self) -> bool {
        self.method == JoinedWriteMethod::Hash
    }
}

/// How a joined write's join reads its inner relation.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum JoinedWriteMethod {
    /// A hash join building on it.
    Hash,
    /// A nested loop rescanning a `Materialize` copy of it.
    Materialized,
    /// A nested loop scanning it again for each outer row.
    Rescanned,
}

/// What a relation's indexes offer a two-relation join over `key` columns:
/// an index on a column only the hash keys read (`keys`), one on another
/// column the conditions read or a partial or expression index (`other`),
/// and a unique one on key columns only (`unique`).
#[derive(Clone, Copy, Default)]
struct JoinIndexReach {
    keys: bool,
    other: bool,
    unique: bool,
}

impl JoinIndexReach {
    fn of(
        storage: &Storage,
        slot: usize,
        txid: u32,
        reads: &dyn Fn(usize) -> bool,
        key: &dyn Fn(usize) -> bool,
    ) -> Self {
        let definition = storage.table_def(slot, txid);
        let columns = definition.columns();
        let mut reach = Self {
            unique: columns
                .iter()
                .enumerate()
                .any(|(column, meta)| (meta.unique || meta.primary) && key(column)),
            ..Self::default()
        };
        for index in storage.indexes_for_slot(slot, txid) {
            let plain = index.predicate.is_none()
                && index.expressions[..index.n_cols]
                    .iter()
                    .all(Option::is_none);
            if !plain {
                reach.other = true;
                continue;
            }
            reach.unique |= index.unique
                && index.columns[..index.n_cols]
                    .iter()
                    .all(|&column| key(usize::from(column)));
        }
        for binding in 0..storage.value_binding_count(slot) {
            let (keys, count) = storage.value_binding_columns(slot, binding);
            for &column in &keys[..count] {
                let column = usize::from(column);
                if !reads(column) {
                    continue;
                }
                if key(column) {
                    reach.keys = true;
                } else {
                    reach.other = true;
                }
            }
        }
        reach
    }

    /// A tree's members together, which PostgreSQL does not prove unique.
    fn with(self, member: Self) -> Self {
        Self {
            keys: self.keys || member.keys,
            other: self.other || member.other,
            unique: false,
        }
    }
}

/// Plans the join of the target `target` (its members `leaves`, those
/// pruning keeps) with `from` under `where_clause`, when it is a join of
/// two relations PostgreSQL plans from their statistics: one plain FROM
/// table, every relation analyzed, local, without row security, and no
/// index PostgreSQL could scan, join or merge by. A column neither relation
/// has that `routine` answers for (a routine variable or parameter) is a
/// constant of the statement. `None` otherwise.
#[expect(clippy::too_many_arguments, reason = "the joined write's parts")]
pub(crate) fn joined_write_join<'a>(
    storage: &Storage,
    target: crate::sql::ast::QualName<'a>,
    alias: Option<&'a str>,
    from: &'a FromClause<'a>,
    where_clause: Option<&'a Expr<'a>>,
    leaves: &[usize],
    params: &[Datum<'a>],
    routine: Option<crate::sql::exec::RoutineScope<'_, 'a>>,
    txid: u32,
    arena: &'a Arena,
) -> Result<Option<JoinedWriteJoin<'a>>, SqlError> {
    let base = &from.base;
    if !from.joins.is_empty()
        || base.subquery.is_some()
        || base.cte.is_some()
        || base.func_args.is_some()
        || base.rows_from.is_some()
        || base.join_members.is_some()
        || base.sample.is_some()
        || base.col_alias.is_some()
        || base.lateral
        || leaves.is_empty()
    {
        return Ok(None);
    }
    let (mut scope, _) =
        super::dependencies::dml_join_scope(storage, target, alias, Some(from), txid, arena, None)?;
    if scope.n != 2 || scope.derived[1].is_some() || scope.slots[1] == usize::MAX {
        return Ok(None);
    }
    let target_slot = scope.slots[0];
    let source_slot = scope.slots[1];
    let Some(role) = storage.current_role_slot(txid) else {
        return Ok(None);
    };
    let source_role = scope.authorization_roles[1].map_or(role, usize::from);
    let local = |slot: usize| {
        storage.table_def(slot, txid).kind == crate::storage::TableKind::Local
            && storage.table_statistics(slot, txid).valid
    };
    if !local(source_slot)
        || storage.relation_has_descendants(source_slot, txid)
        || storage.row_security_applies(source_slot, source_role, txid)
        || storage.row_security_applies(target_slot, role, txid)
        || !leaves.iter().all(|&leaf| local(leaf))
    {
        return Ok(None);
    }
    // A routine variable or parameter: a name the relations lack that the
    // routine's scope answers for.
    let constant = |expression: &Expr<'_>| match (expression, routine) {
        (Expr::Column { qualifier, name }, Some(routine)) => {
            routine.lookup.lookup_unsuggested(*qualifier, name).is_ok()
        }
        (Expr::RoutineParam { .. }, _) => true,
        _ => false,
    };
    let tables =
        |expression: &Expr<'_>| super::plan::expr_tables_with(expression, &scope, &constant);
    // Every conjunct reads a relation and no subquery: PostgreSQL gates the
    // join on the others, or runs them as subplans.
    let mut conjuncts = [&Expr::Null; super::plan::MAX_CONJUNCTS];
    let mut count = 0;
    if let Some(condition) = where_clause {
        if !super::plan::flatten_and(condition, &mut conjuncts, &mut count) {
            return Ok(None);
        }
        for &conjunct in &conjuncts[..count] {
            if conjunct.contains_subquery() || !matches!(tables(conjunct), Some(1..=3)) {
                return Ok(None);
            }
        }
    }
    let conjuncts = &conjuncts[..count];
    let methods = crate::sql::guc::planner_methods();
    let mut plan = JoinedWriteJoin {
        source_slot,
        source_name: arena.alloc_str(scope.names[1]).map_err(|_| arena_full())?,
        keys: [JoinedWriteKey::NONE; 8],
        key_count: 0,
        collations: [crate::sql::ast::Collation::None; 8],
        source_outer: false,
        method: JoinedWriteMethod::Rescanned,
        inner_unique: false,
        conditions: MergeConditions::default(),
    };
    let column = |side: &Expr<'_>| match side {
        Expr::Column { qualifier, name } => match scope.find_column(*qualifier, name) {
            Ok(super::ResolvedColumn::Table(table, column)) => Some((table, column)),
            _ => None,
        },
        _ => None,
    };
    // The hash keys: each equality between an expression of the target and
    // one of the FROM table, of one hashable type.
    let mut is_key = [false; super::plan::MAX_CONJUNCTS];
    for (index, &conjunct) in conjuncts.iter().enumerate() {
        let Expr::Binary {
            operator: crate::sql::ast::BinaryOp::Eq,
            left,
            right,
            ..
        } = conjunct
        else {
            continue;
        };
        let (target_side, source_side) = match (tables(left), tables(right)) {
            (Some(1), Some(2)) => (*left, *right),
            (Some(2), Some(1)) => (*right, *left),
            _ => continue,
        };
        // PostgreSQL hashes on any equality between the two relations; one
        // this plan cannot hash leaves the join to the general executor.
        let side_type = |side: &Expr<'_>| {
            super::infer_scope_type(side, &scope)
                .ok()
                .and_then(|(oid, _)| crate::sql::types::ColType::from_oid(oid))
        };
        let (Some(target_type), Some(source_type)) =
            (side_type(target_side), side_type(source_side))
        else {
            return Ok(None);
        };
        if !super::scan::join_key_types_compatible(target_type, source_type)
            || plan.key_count == plan.keys.len()
        {
            return Ok(None);
        }
        let target_column = column(target_side).map(|(_, column)| column);
        let source_column = column(source_side).map(|(_, column)| column);
        let collation = match (target_column, source_column) {
            (Some(target_column), Some(source_column)) => {
                match super::scan::join_key_collation(
                    &scope,
                    (0, target_column),
                    (1, source_column),
                ) {
                    Some(collation) => collation,
                    None => return Ok(None),
                }
            }
            _ if target_type.is_collatable() || source_type.is_collatable() => return Ok(None),
            _ => crate::sql::ast::Collation::None,
        };
        // A key the conditions equate with a constant or another expression
        // makes the join's equality implied, as PostgreSQL's equivalence
        // classes do.
        let equated = |side: &Expr<'_>| {
            conjuncts.iter().enumerate().any(|(other, conjunct)| {
                other != index
                    && matches!(
                        conjunct,
                        Expr::Binary {
                            operator: crate::sql::ast::BinaryOp::Eq,
                            left,
                            right,
                            ..
                        } if **left == *side || **right == *side
                    )
            })
        };
        if equated(target_side)
            || equated(source_side)
            || target_column.is_some_and(|column| {
                conjuncts
                    .iter()
                    .any(|conjunct| super::scan::fixes_column(&scope, conjunct, (0, column)))
            })
            || source_column.is_some_and(|column| {
                conjuncts
                    .iter()
                    .any(|conjunct| super::scan::fixes_column(&scope, conjunct, (1, column)))
            })
        {
            return Ok(None);
        }
        plan.keys[plan.key_count] = JoinedWriteKey {
            target: target_side,
            source: source_side,
            target_column,
            source_column,
        };
        plan.collations[plan.key_count] = collation;
        plan.key_count += 1;
        is_key[index] = true;
    }
    if plan.key_count != 0 {
        // PostgreSQL merges by the equalities when it may not hash them.
        if !methods.hashjoin {
            return Ok(None);
        }
    } else if !methods.nestloop {
        return Ok(None);
    }
    // Each conjunct's place in the plan.
    let mut conditions = MergeConditions::default();
    for (index, &conjunct) in conjuncts.iter().enumerate() {
        let slot = if is_key[index] {
            &mut conditions.hash
        } else {
            match tables(conjunct) {
                Some(1) => &mut conditions.target,
                Some(2) => &mut conditions.source,
                _ => &mut conditions.join,
            }
        };
        *slot = super::and_where(*slot, Some(conjunct), arena)?;
    }
    plan.conditions = conditions;
    // The columns each relation's conditions read, and whether only as a
    // hash key.
    let reads = |table: usize, column_index: usize| {
        conjuncts.iter().any(|conjunct| {
            let mut found = false;
            conjunct.for_each_column_reference(&mut |qualifier, name| {
                found |= matches!(
                    scope.find_column(qualifier, name),
                    Ok(super::ResolvedColumn::Table(owner, at)) if owner == table && at == column_index
                );
            });
            found
        })
    };
    let key_only = |table: usize, column_index: usize| {
        let reads_column = |condition: &Expr<'_>| {
            let mut found = false;
            condition.for_each_column_reference(&mut |qualifier, name| {
                found |= matches!(
                    scope.find_column(qualifier, name),
                    Ok(super::ResolvedColumn::Table(owner, at)) if owner == table && at == column_index
                );
            });
            found
        };
        [conditions.target, conditions.source, conditions.join]
            .into_iter()
            .flatten()
            .all(|condition| !reads_column(condition))
            && plan.keys().iter().all(|key| {
                let (side, side_column) = if table == 0 {
                    (key.target, key.target_column)
                } else {
                    (key.source, key.source_column)
                };
                side_column == Some(column_index) || !reads_column(side)
            })
            && plan.keys().iter().any(|key| {
                if table == 0 {
                    key.target_column == Some(column_index)
                } else {
                    key.source_column == Some(column_index)
                }
            })
    };
    let target_member = |leaf: usize| {
        JoinIndexReach::of(storage, leaf, txid, &|column| reads(0, column), &|column| {
            key_only(0, column)
        })
    };
    let target_reach = if leaves == [target_slot] {
        target_member(target_slot)
    } else {
        leaves
            .iter()
            .fold(JoinIndexReach::default(), |reach, &leaf| {
                reach.with(target_member(leaf))
            })
    };
    let source_reach = JoinIndexReach::of(
        storage,
        source_slot,
        txid,
        &|column| reads(1, column),
        &|column| key_only(1, column),
    );
    let Some((source_outer, method)) = crate::sql::explain::joined_write_shape(
        storage, &mut scope, leaves, &plan, params, routine, txid, arena,
    )?
    else {
        return Ok(None);
    };
    // A nested loop over key equalities checks them as its join filter,
    // which this plan does not model.
    if plan.key_count != 0 && method != JoinedWriteMethod::Hash {
        return Ok(None);
    }
    plan.source_outer = source_outer;
    plan.method = method;
    let (outer_reach, inner_reach) = if plan.source_outer {
        (source_reach, target_reach)
    } else {
        (target_reach, source_reach)
    };
    // PostgreSQL may scan the outer relation by such an index, or rescan
    // the inner one by it in a nested loop.
    if outer_reach.keys
        || outer_reach.other
        || inner_reach.other
        || (inner_reach.keys && !plan.hashed())
    {
        return Ok(None);
    }
    plan.inner_unique = inner_reach.unique;
    Ok(Some(plan))
}

/// The join rows one target row made with its candidate FROM rows: those
/// the join returned, and those its join filter removed.
#[derive(Clone, Copy, Default)]
pub(crate) struct JoinedWritePairs {
    pub(crate) joined: u64,
    pub(crate) filtered: u64,
}

/// Joins the target row `target` with the FROM rows `candidates`, in that
/// order: each whose `hash` equalities hold is a candidate pair, which the
/// join returns when `filter` holds. Counts the pairs in `counts` and
/// returns the first pair the join returned, its FROM row retained in
/// `retain` when given.
#[expect(clippy::too_many_arguments, reason = "joined DML scan inputs")]
pub(crate) fn joined_write_pairs<'a, 'r>(
    storage: &'a Storage,
    from: &'a FromClause<'a>,
    txid: u32,
    hash: Option<&'a Expr<'a>>,
    filter: Option<&'a Expr<'a>>,
    arena: &'a Arena,
    params: &[Datum<'a>],
    target: &dyn ColumnLookup<'a>,
    sequences: Option<&dyn SequenceAccess>,
    candidates: &'a [u64],
    retain: Option<&'r Arena>,
    counts: &mut JoinedWritePairs,
) -> Result<Option<(u64, Option<JoinedFromRow<'r>>)>, SqlError> {
    if candidates.is_empty() {
        return Ok(None);
    }
    let mut first = None;
    let observed: &[&'a Expr<'a>] = match filter {
        Some(filter) => core::slice::from_ref(arena.alloc(filter).map_err(|_| arena_full())?),
        None => &[],
    };
    scan_from_matches(
        storage,
        from,
        txid,
        hash,
        observed,
        retain.is_some(),
        arena,
        params,
        target,
        sequences,
        Some(candidates),
        &mut |row, joined, hooks| {
            let passes = match filter {
                Some(filter) => super::plan::where_passes(
                    filter,
                    arena,
                    params,
                    &Chained {
                        inner: joined,
                        outer: None,
                    },
                    hooks,
                )?,
                None => true,
            };
            if !passes {
                counts.filtered += 1;
                return Ok(true);
            }
            counts.joined += 1;
            if first.is_none() {
                let rowid = row.rowids.first().copied().flatten().ok_or_else(|| {
                    crate::sql_err!(
                        crate::sql::eval::sqlstate::INTERNAL_ERROR,
                        "a joined FROM row has no row identity"
                    )
                })?;
                let retained = match retain {
                    Some(retain) => Some(JoinedFromRow::encode(row, retain)?),
                    None => None,
                };
                first = Some((rowid, retained));
            }
            Ok(true)
        },
    )?;
    Ok(first)
}
