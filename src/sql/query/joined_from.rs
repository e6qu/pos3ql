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
        let columns = scope.total_columns();
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
                let n_columns = scope.defs.get(table).expect("resolved").n_columns;
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
        let columns = scope.total_columns();
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
                let n_columns = scope.defs.get(table).expect("resolved").n_columns;
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
    on_match: &mut FromMatch<'_, 'a>,
) -> Result<(), SqlError> {
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
/// `row`, the scope assignments and `RETURNING` read.
#[expect(clippy::too_many_arguments, reason = "joined DML scope inputs")]
pub(crate) fn with_joined_from_row<'a, R>(
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
    each(&Chained {
        inner: &sources,
        outer: Some(target),
    })
}
