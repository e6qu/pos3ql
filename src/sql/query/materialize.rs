//! Materializing a SELECT's rows before they can be returned.
//!
//! Streaming a row straight to the wire is only possible when nothing has to
//! see the whole result first. `GROUP BY`, `DISTINCT` and `ORDER BY` all do, so
//! their rows are projected into the statement arena as self-describing byte
//! strings and sorted or deduplicated there. ORDER BY keys ride along as hidden
//! columns after the visible ones, which is how an arbitrary key expression can
//! order a result. Volatile or expensive output can be evaluated after sorting,
//! in the order and row window requested by LIMIT and OFFSET.

use crate::mem::arena::{Arena, ArenaList};
use crate::pg::respond::Responder;
use crate::sql::ast::{Collation, Expr, FromClause, Select, SelectItem};
use crate::sql::eval::{ColumnLookup, EvalHooks, SqlError, SubqueryValues, eval_full, sqlstate};
use crate::sql::exec::MAX_PROJ;
use crate::sql::types::{ColType, Datum};
use crate::storage::Storage;
use crate::{sql_err, stack_format};

use super::{
    Chained, JoinRow, Outcome, QueryScope, ResolvedColumn, arena_full, correlated_in_expression,
    correlated_scan_conjuncts, correlated_where_passes, has_project_set, instrument,
    pax_column_demand, postpone_cost, prepare_project_set, project_row_skipping, record_star_width,
    resolve_order_target, scan_source_recycling_with_indexed_candidates,
    scan_source_recycling_with_pax_columns, sql_fail, sql_ok,
};

/// A flat decoded source row (every column of every scope table, in scope
/// order) resolvable by name, for evaluating postponed projection items after
/// the sort.
struct EncodedRawRow<'s, 'd, 'a> {
    scope: &'s QueryScope<'d>,
    bytes: &'a [u8],
    raw_at: usize,
    arena: &'a Arena,
}

impl<'a> ColumnLookup<'a> for EncodedRawRow<'_, '_, 'a> {
    fn whole_row_field_metadata(
        &self,
        table: &str,
        index: usize,
        catalog: Option<&dyn super::super::eval::CatalogAccess>,
    ) -> Option<(crate::util::StackStr<64>, crate::sql::exec::StaticTypeMeta)> {
        ScopeSchema(self.scope).whole_row_field_metadata(table, index, catalog)
    }

    fn column_type_mod(&self, qualifier: Option<&str>, name: &str) -> i32 {
        ScopeSchema(self.scope).column_type_mod(qualifier, name)
    }

    fn resolve_column_type(
        &self,
        qualifier: Option<&str>,
        name: &str,
    ) -> Result<ColType, SqlError> {
        ScopeSchema(self.scope).resolve_column_type(qualifier, name)
    }

    fn resolve_column_type_unsuggested(
        &self,
        qualifier: Option<&str>,
        name: &str,
    ) -> Result<ColType, SqlError> {
        ColumnLookup::<'a>::resolve_column_type_unsuggested(
            &ScopeSchema(self.scope),
            qualifier,
            name,
        )
    }

    fn suggest_columns(
        &self,
        suggestion: &mut crate::sql::column_suggestion::ColumnSuggestion<'_>,
    ) {
        self.scope.suggest_columns(suggestion);
    }

    fn is_empty_scope(&self) -> bool {
        self.scope.n == 0
    }

    fn lookup(&self, qualifier: Option<&str>, name: &str) -> Result<Datum<'a>, SqlError> {
        self.lookup_unsuggested(qualifier, name)
            .map_err(|error| self.scope.suggested(error, qualifier, name))
    }

    fn lookup_unsuggested(
        &self,
        qualifier: Option<&str>,
        name: &str,
    ) -> Result<Datum<'a>, SqlError> {
        let flat_of = |t: usize, c: usize| -> usize {
            (0..t).map(|i| self.scope.stored_width(i)).sum::<usize>() + c
        };
        match self.scope.find_column_unsuggested(qualifier, name)? {
            ResolvedColumn::Table(t, c) => crate::sql::exec::decode_projected_col_record(
                self.bytes,
                self.raw_at + flat_of(t, c),
                self.arena,
            ),
            // Merged USING/NATURAL column: the first non-null contributor.
            ResolvedColumn::Merged(m) => {
                let mc = &self.scope.merged[m];
                for &(t, c) in mc.parts {
                    let v = crate::sql::exec::decode_projected_col_record(
                        self.bytes,
                        self.raw_at + flat_of(t, c),
                        self.arena,
                    )?;
                    if !v.is_null() {
                        return Ok(v);
                    }
                }
                Ok(Datum::Null)
            }
            ResolvedColumn::TableOid(t) => self.scope.flat_relation_oid(t),
        }
    }

    fn col_type(&self, qualifier: Option<&str>, name: &str) -> Option<ColType> {
        match self.scope.find_column_unsuggested(qualifier, name) {
            Ok(entry) => Some(self.scope.output_type(entry)),
            Err(error)
                if error.sqlstate == sqlstate::UNDEFINED_COLUMN
                    && qualifier.is_none()
                    && (self.scope.using_alias(name).is_some()
                        || self.scope.source(name).is_ok()) =>
            {
                self.scope.whole_row_type(name)
            }
            Err(_) => None,
        }
    }

    fn collation(&self, qualifier: Option<&str>, name: &str) -> Collation {
        self.scope
            .find_column_unsuggested(qualifier, name)
            .ok()
            .map(|entry| self.scope.output_collation(entry))
            .unwrap_or(Collation::None)
    }

    fn record_field_collation(&self, base: &Expr<'a>, field: &str) -> Collation {
        crate::sql::exec::record_field_metadata(base, field, &super::ScopeCols(self.scope))
            .map_or(Collation::None, |meta| meta.collation)
    }

    fn column_user_type(
        &self,
        qualifier: Option<&str>,
        name: &str,
    ) -> Option<crate::storage::UserTypeName> {
        match self.scope.find_column_unsuggested(qualifier, name).ok()? {
            ResolvedColumn::Table(table, column) => {
                self.scope.defs.get(table)?.columns.get(column)?.user_type
            }
            ResolvedColumn::Merged(_) | ResolvedColumn::TableOid(_) => None,
        }
    }

    fn column_identity(&self, qualifier: Option<&str>, name: &str) -> Option<(u32, u32)> {
        match self.scope.find_column_unsuggested(qualifier, name).ok()? {
            ResolvedColumn::Table(table, column) => Some((table as u32, column as u32)),
            ResolvedColumn::Merged(merged) => Some((u32::MAX, merged as u32)),
            ResolvedColumn::TableOid(table) => Some((table as u32, u32::MAX)),
        }
    }

    fn whole_row_is_scalar(&self, table: &str) -> bool {
        self.scope.func_scalar_type(table).is_some()
    }

    fn whole_row_type(&self, table: &str) -> Option<ColType> {
        self.scope.whole_row_type(table)
    }

    fn whole_row_present(&self, table: &str) -> Result<bool, SqlError> {
        if self.scope.using_alias(table).is_some() {
            return Ok(true);
        }
        let source = self.scope.source(table)?;
        let value = crate::sql::exec::decode_projected_col_record(
            self.bytes,
            self.raw_at + self.scope.stored_columns() + source.table,
            self.arena,
        )?;
        let present = match value {
            Datum::Bool(present) => present,
            _ => {
                return Err(sql_err!(
                    sqlstate::DATA_EXCEPTION,
                    "saved projection source-presence flag is not boolean"
                ));
            }
        };
        let Some(column) = source.presence.filter(|_| present) else {
            return Ok(present);
        };
        let offset = (0..source.table)
            .map(|table| self.scope.stored_width(table))
            .sum::<usize>()
            + column;
        super::scope::member_present(crate::sql::exec::decode_projected_col_record(
            self.bytes,
            self.raw_at + offset,
            self.arena,
        )?)
    }

    fn whole_row_fields(
        &self,
        table: &str,
        arena: &'a Arena,
    ) -> Result<Option<&'a [crate::sql::types::RecordField<'a>]>, SqlError> {
        if !self.whole_row_present(table)? {
            return Ok(None);
        }
        self.whole_row_expansion_fields(table, arena).map(Some)
    }

    fn whole_row_expansion_fields(
        &self,
        table: &str,
        arena: &'a Arena,
    ) -> Result<&'a [crate::sql::types::RecordField<'a>], SqlError> {
        let width = self.scope.qualified_star_columns(table)?;
        let fields = arena
            .alloc_slice_with(width, |_| crate::sql::types::RecordField {
                name: "",
                type_oid: 0,
                type_mod: -1,
                value: Datum::Null,
            })
            .map_err(|_| arena_full())?;
        for (index, field) in fields.iter_mut().enumerate() {
            let entry = self.scope.qualified_star_entry(table, index)?;
            let name = self.scope.output_name(entry);
            field.name = arena.alloc_str(name).map_err(|_| arena_full())?;
            field.type_oid = self.scope.output_type(entry).oid();
            field.value = self.lookup(Some(table), name)?;
        }
        Ok(fields)
    }
}

/// Projects each source row before deriving its sort keys. Keys that name
/// output columns reuse their saved values, including volatile expressions.
#[expect(clippy::too_many_arguments, reason = "query pipeline plumbing")]
fn for_each_materialized_projection<'a>(
    storage: &'a Storage,
    scope: &QueryScope<'a>,
    statement: &'a Select<'a>,
    row: &JoinRow<'_, 'a, '_>,
    txid: u32,
    arena: &'a Arena,
    params: &[Datum<'a>],
    hooks: &EvalHooks<'_, 'a>,
    base: &SubqueryValues<'a, 'a>,
    where_correlated: &[&'a Expr<'a>],
    order_exprs: &[Option<super::ResolvedSortKey<'a>>; MAX_PROJ],
    n_keys: usize,
    postponed: Option<&[bool; MAX_PROJ]>,
    has_srf: bool,
    outer: Option<&dyn ColumnLookup<'a>>,
    consume: &mut impl FnMut(&JoinRow<'_, 'a, '_>, &[Datum<'a>], &[Datum<'a>]) -> Result<(), SqlError>,
) -> Result<(), SqlError> {
    if !where_correlated.is_empty()
        && !correlated_where_passes(
            base,
            statement.where_clause,
            row,
            outer,
            arena,
            params,
            hooks,
        )?
    {
        return Ok(());
    }
    // The row's correlated subqueries run as its expressions reach them.
    let _row_binding = super::subquery::bind_correlated_row(base, row, outer);
    let row_hooks = hooks;
    let project_set = if has_srf {
        prepare_project_set(
            statement.items,
            storage,
            txid,
            arena,
            params,
            row,
            row_hooks,
        )?
    } else {
        super::srf::ProjectSet::single()
    };
    for expansion in 1..=project_set.count {
        let srf_hooks;
        let use_hooks: &EvalHooks = if project_set.any || has_srf {
            srf_hooks = EvalHooks {
                srf_index: Some(expansion),
                project_sets: Some(project_set.values),
                ..*row_hooks
            };
            &srf_hooks
        } else {
            row_hooks
        };
        let mut projected = [Datum::Null; MAX_PROJ];
        let width = project_row_skipping(
            statement.items,
            postponed,
            scope,
            row,
            arena,
            params,
            use_hooks,
            &mut projected,
            outer,
        )?;
        let mut keys = [Datum::Null; MAX_PROJ];
        let chained_row = Chained { inner: row, outer };
        for (key, expression) in order_exprs.iter().take(n_keys).enumerate() {
            let value = expression.expect("resolved").value(
                &projected[..width],
                &keys[..key],
                |expression| eval_full(expression, arena, params, &chained_row, use_hooks),
            )?;
            keys[key] = value;
        }
        consume(row, &projected[..width], &keys[..n_keys])?;
    }
    Ok(())
}

#[expect(clippy::too_many_arguments, reason = "materialized row layout")]
fn materialized_value_at<'a>(
    scope: &QueryScope<'a>,
    row: &JoinRow<'_, 'a, '_>,
    projected: &[Datum<'a>],
    keys: &[Datum<'a>],
    width: usize,
    n_keys: usize,
    identities_at: usize,
    raw_columns: Option<&super::scan::PaxReadDemand>,
    index: usize,
) -> Datum<'a> {
    if index < width {
        return projected[index];
    }
    if index < width + n_keys {
        return keys[index - width];
    }
    if index >= identities_at {
        return row.rowids[index - identities_at]
            .and_then(|rowid| i64::try_from(rowid).ok())
            .map(Datum::Int8)
            .unwrap_or(Datum::Null);
    }
    let mut raw_index = index - width - n_keys;
    let source_columns = scope.stored_columns();
    if raw_index >= source_columns {
        let table = raw_index - source_columns;
        let present = row
            .table_present(table)
            .expect("materialized source is joined");
        return Datum::Bool(present);
    }
    for table in 0..scope.n {
        let column_count = scope.stored_width(table);
        if raw_index < column_count {
            if !raw_columns
                .is_some_and(|columns| columns.references_column(table, raw_index, column_count))
            {
                return Datum::Null;
            }
            let values = row.table_values(table).expect("bound");
            return if values.is_empty() {
                Datum::Null
            } else {
                values[raw_index]
            };
        }
        raw_index -= column_count;
    }
    unreachable!("raw projected column is within scope width")
}

/// A materialized row after its locks.
pub(super) enum LockedOutput<'a> {
    /// The row leaves the result.
    Skip,
    /// The row is returned as read.
    Unchanged,
    /// A locked source changed; the row evaluated again.
    Rebuilt(&'a [u8]),
}

impl<'a> LockedOutput<'a> {
    fn row<'b>(self, read: &'b [u8]) -> Option<&'b [u8]>
    where
        'a: 'b,
    {
        match self {
            LockedOutput::Skip => None,
            LockedOutput::Unchanged => Some(read),
            LockedOutput::Rebuilt(row) => Some(row),
        }
    }
}

/// A source row assembled from per-source values in scope order.
pub(super) fn sources_row<'s, 'a>(
    scope: &'s QueryScope<'a>,
    bound: &'s [Option<super::scan::BoundRow<'a>>],
    order: &'s [usize],
    rowids: &'s [Option<u64>],
) -> JoinRow<'s, 'a, 'a> {
    JoinRow {
        scope,
        values: super::scan::JoinRowValues::Bound {
            bound,
            order,
            count: scope.n,
            buffers: &[],
            buffer_offsets: &[],
        },
        rowids: &rowids[..scope.n],
        visible: scope.n,
    }
}

/// Where a materialized row stores each kind of column.
struct MaterializedLayout<'p> {
    width: usize,
    n_keys: usize,
    identities_at: usize,
    stored_width: usize,
    raw_columns: Option<&'p super::scan::PaxReadDemand>,
}

/// Encodes one projected source row into the statement arena's persistent
/// tail, which survives the scan's per-row rewinds.
fn encode_materialized_row<'a>(
    scope: &QueryScope<'a>,
    row: &JoinRow<'_, 'a, '_>,
    projected: &[Datum<'a>],
    keys: &[Datum<'a>],
    layout: &MaterializedLayout<'_>,
    arena: &'a Arena,
) -> Result<&'a [u8], SqlError> {
    let value = |index| {
        materialized_value_at(
            scope,
            row,
            projected,
            keys,
            layout.width,
            layout.n_keys,
            layout.identities_at,
            layout.raw_columns,
            index,
        )
    };
    let row_bytes = crate::sql::exec::projected_row_len_by(layout.stored_width, value)?;
    let output = arena
        .alloc_persistent_slice_with(row_bytes, |_| 0u8)
        .map_err(|_| arena_full())?;
    let len = crate::sql::exec::encode_projected_by_into(layout.stored_width, value, output)?;
    Ok(&output[..len])
}

/// Evaluates a locked result row again when another transaction replaced
/// one of its locked source rows, as PostgreSQL's EvalPlanQual does under
/// read committed: replaced sources contribute their latest versions, the
/// others keep the values read, and the join conditions, WHERE clause, and
/// projection run again. PostgreSQL's LockRows node sits above Sort, so the
/// row keeps its place in the result.
pub(super) struct LockedRowRecheck<'r, 'a> {
    plan: MaterializationPlan<'a>,
    statement: &'a Select<'a>,
    scope: &'r QueryScope<'a>,
    from: &'a FromClause<'a>,
    base: &'r SubqueryValues<'a, 'a>,
    params: &'r [Datum<'a>],
    hooks: &'r EvalHooks<'r, 'a>,
    outer: Option<&'r dyn ColumnLookup<'a>>,
    changed: &'a mut [Option<usize>],
}

/// Rechecks `predicate` for a locked row's latest version, as PostgreSQL's
/// EvalPlanQual runs the query's plan again over it. A top-level `IN
/// (subquery)` without outer references and a top-level correlated `EXISTS`
/// are semi-joins in PostgreSQL's plan, whose recheck keeps the subquery row
/// first matched: the `IN` operand must still equal its first value, and the
/// `EXISTS` condition runs over the subquery row the version `first` matched
/// first. Every other condition runs again over `latest`, with correlated
/// subqueries evaluated for it.
#[allow(clippy::too_many_arguments)]
pub(crate) fn locked_row_where_passes<'a>(
    storage: &Storage,
    txid: u32,
    predicate: &'a Expr<'a>,
    first: &dyn ColumnLookup<'a>,
    latest: &dyn ColumnLookup<'a>,
    arena: &'a Arena,
    params: &[Datum<'a>],
    hooks: &EvalHooks<'_, 'a>,
) -> Result<bool, SqlError> {
    // SAFETY: Storage outlives this call and stays immutable during it; only
    // a Boolean escapes, so no value borrowing relation bytes outlives it.
    let storage: &'a Storage = unsafe { &*(storage as *const Storage) };
    let correlated_subqueries = super::ProceduralSubqueries { storage, txid };
    let hooks = EvalHooks {
        subquery_executor: Some(&correlated_subqueries),
        ..*hooks
    };
    let first = Chained {
        inner: first,
        outer: None,
    };
    let latest = Chained {
        inner: latest,
        outer: None,
    };
    let mut conjuncts = [predicate; super::plan::MAX_CONJUNCTS];
    let mut count = 0;
    if !super::plan::flatten_and(predicate, &mut conjuncts, &mut count) {
        conjuncts[0] = predicate;
        count = 1;
    }
    for conjunct in &conjuncts[..count] {
        let passes = match conjunct {
            Expr::InSubquery {
                operand,
                negated: false,
                ..
            } if !super::subquery::subquery_node_correlated(conjunct, storage, txid, arena)? => {
                let before = eval_full(operand, arena, params, &first, &hooks)?;
                let after = eval_full(operand, arena, params, &latest, &hooks)?;
                let catalog = super::storage_catalog(storage, arena, txid);
                !before.is_null()
                    && !after.is_null()
                    && crate::sql::eval::compare_datums_collated(
                        storage,
                        txid,
                        crate::sql::eval::resolved_expression_collation(
                            operand,
                            &latest,
                            Some(&catalog),
                        )?,
                        &before,
                        &after,
                    )?
                    .is_eq()
            }
            Expr::Exists(select)
                if semi_join_exists(select, storage, txid)
                    && super::subquery::subquery_node_correlated(
                        conjunct, storage, txid, arena,
                    )? =>
            {
                super::semi_join_first_match_passes(
                    storage,
                    select,
                    txid,
                    arena,
                    params,
                    &first,
                    &latest,
                    hooks.sequences,
                )?
            }
            _ => super::plan::conjunct_passes(conjunct, arena, params, &latest, &hooks)?,
        };
        if !passes {
            return Ok(false);
        }
    }
    Ok(true)
}

/// Whether PostgreSQL turns `EXISTS (select)` into a semi-join
/// (`convert_EXISTS_sublink_to_join` after `simplify_EXISTS_query`): a plain
/// query over ordinary relations without grouping, aggregates, window
/// functions, set operations, `OFFSET` or a non-constant `LIMIT`, whose
/// `WHERE` calls no volatile function.
fn semi_join_exists(select: &Select<'_>, storage: &Storage, txid: u32) -> bool {
    let Some(from) = select.from.as_ref() else {
        return false;
    };
    let plain = |table: &crate::sql::ast::TableRef<'_>| {
        table.subquery.is_none() && !table.is_function_source() && table.cte.is_none()
    };
    let limit_removable = match select.limit {
        None => true,
        Some(Expr::Int(limit)) => *limit > 0,
        Some(_) => false,
    };
    plain(&from.base)
        && from.joins.iter().all(|join| plain(&join.table))
        && select.set_body.is_none()
        && select.with.is_empty()
        && select.locking.is_empty()
        && select.group_by.is_empty()
        && select.grouping_sets.is_empty()
        && select.having.is_none()
        && select.offset.is_none()
        && limit_removable
        && !select.items.iter().any(|item| match item {
            SelectItem::Expr { expression, .. } | SelectItem::RecordStar(expression) => {
                super::expr_has_window(expression)
                    || expression_has_aggregate_use(expression, storage, txid)
            }
            SelectItem::Wildcard(_) | SelectItem::TableWildcard(_) => false,
        })
        && !select.where_clause.is_some_and(|predicate| {
            super::cte::expression_contains_function(predicate, &|name, arguments| {
                crate::sql::ast::is_volatile_function(name)
                    || storage.has_volatile_function_routine_candidate(name, arguments, txid)
            })
        })
}

/// How a locked row's recheck treats a `LATERAL` source, following the plan
/// PostgreSQL's EvalPlanQual re-runs. A simple subquery without `FROM` is
/// pulled up into the outer query, so its expressions and `WHERE` run again
/// over the latest row. A function, a `VALUES` list of several rows and a
/// subquery the planner keeps separate (aggregates, `GROUP BY`, `DISTINCT`,
/// `LIMIT`/`OFFSET`, set operations, window or set-returning functions,
/// volatile output expressions) keep the row the statement first read.
enum LateralRecheck<'a> {
    Keep,
    Rerun,
    /// A simple subquery with `FROM`, pulled up with its source rows.
    RerunJoined(&'a Select<'a>),
}

fn lateral_recheck<'a>(
    storage: &Storage,
    txid: u32,
    tref: &crate::sql::ast::TableRef<'a>,
) -> Result<LateralRecheck<'a>, SqlError> {
    let Some(select) = tref.subquery else {
        return Ok(LateralRecheck::Keep);
    };
    if let Some(body) = select.set_body {
        return Ok(match body {
            crate::sql::ast::SetTree::Select(select) if select.from.is_none() => {
                lateral_recheck_select(storage, txid, select)
            }
            crate::sql::ast::SetTree::Select(_) => {
                return Err(lateral_recheck_unsupported());
            }
            // PostgreSQL flattens a `UNION ALL` of simple branches into an
            // append whose recheck returns the first branch's row again.
            crate::sql::ast::SetTree::Op {
                operator: crate::sql::ast::SetOp::Union,
                all: true,
                syntax: crate::sql::ast::SetOperationSyntax::Explicit,
                ..
            } => {
                if !union_all_branches_rerun(storage, txid, body) {
                    return Err(lateral_recheck_unsupported());
                }
                LateralRecheck::Rerun
            }
            crate::sql::ast::SetTree::Op { .. } => LateralRecheck::Keep,
        });
    }
    let rerun = lateral_recheck_select(storage, txid, select);
    if matches!(rerun, LateralRecheck::Rerun) && select.from.is_some() {
        return Ok(LateralRecheck::RerunJoined(select));
    }
    Ok(rerun)
}

fn lateral_recheck_select<'a>(
    storage: &Storage,
    txid: u32,
    select: &Select<'_>,
) -> LateralRecheck<'a> {
    if select.distinct
        || !select.distinct_on.is_empty()
        || !select.group_by.is_empty()
        || !select.grouping_sets.is_empty()
        || select.having.is_some()
        || select.limit.is_some()
        || select.offset.is_some()
        || !select.with.is_empty()
        || !select.locking.is_empty()
        || super::srf::has_project_set(select.items, storage, txid)
    {
        return LateralRecheck::Keep;
    }
    for item in select.items {
        let (SelectItem::Expr { expression, .. } | SelectItem::RecordStar(expression)) = item
        else {
            continue;
        };
        if super::expr_has_window(expression)
            || expression_has_aggregate_use(expression, storage, txid)
            || super::cte::expression_contains_function(expression, &|name, arguments| {
                crate::sql::ast::is_volatile_function(name)
                    || storage.has_volatile_function_routine_candidate(name, arguments, txid)
            })
        {
            return LateralRecheck::Keep;
        }
    }
    LateralRecheck::Rerun
}

/// Whether every branch of a `UNION ALL` tree is a simple query without
/// `FROM`, so running the tree again reproduces PostgreSQL's append recheck.
fn union_all_branches_rerun(
    storage: &Storage,
    txid: u32,
    tree: &crate::sql::ast::SetTree<'_>,
) -> bool {
    match tree {
        crate::sql::ast::SetTree::Select(select) => {
            select.from.is_none()
                && select.set_body.is_none()
                && matches!(
                    lateral_recheck_select(storage, txid, select),
                    LateralRecheck::Rerun
                )
        }
        crate::sql::ast::SetTree::Op {
            operator: crate::sql::ast::SetOp::Union,
            all: true,
            syntax: crate::sql::ast::SetOperationSyntax::Explicit,
            left,
            right,
        } => {
            union_all_branches_rerun(storage, txid, left)
                && union_all_branches_rerun(storage, txid, right)
        }
        crate::sql::ast::SetTree::Op { .. } => false,
    }
}

/// Whether `expression` calls an aggregate outside a nested subquery. The
/// subquery's own columns are not in scope here, so a call is classified by
/// name: a built-in aggregate or any visible user-defined aggregate.
pub(super) fn expression_has_aggregate_use(
    expression: &Expr<'_>,
    storage: &Storage,
    txid: u32,
) -> bool {
    if let Expr::Call {
        name,
        args,
        over: None,
        ..
    } = expression
        && (expression.is_aggregate()
            || storage.has_aggregate_routine_candidate(name, args.len(), txid))
    {
        return true;
    }
    let mut found = false;
    super::walk_children(expression, &mut |child| {
        found |= expression_has_aggregate_use(child, storage, txid);
        Ok(())
    })
    .expect("expression visitor cannot fail");
    found
}

/// A pulled-up `LATERAL` subquery with `FROM` keeps the rows its tables
/// first matched. pos3ql finds them again from the values the statement read,
/// which a `UNION ALL` branch with `FROM` or several source rows producing
/// the same values leave ambiguous.
fn lateral_recheck_unsupported() -> SqlError {
    sql_err!(
        sqlstate::FEATURE_NOT_SUPPORTED,
        "a concurrently updated row locked with a LATERAL subquery that reads tables cannot be rechecked"
    )
}

impl<'r, 'a> LockedRowRecheck<'r, 'a> {
    #[expect(clippy::too_many_arguments, reason = "query pipeline plumbing")]
    pub(super) fn new(
        storage: &Storage,
        txid: u32,
        statement: &'a Select<'a>,
        scope: &'r QueryScope<'a>,
        from: &'a FromClause<'a>,
        correlated: &'a [&'a Expr<'a>],
        base: &'r SubqueryValues<'a, 'a>,
        params: &'r [Datum<'a>],
        hooks: &'r EvalHooks<'r, 'a>,
        outer: Option<&'r dyn ColumnLookup<'a>>,
        arena: &'a Arena,
    ) -> Result<Self, SqlError> {
        let plan =
            prepare_materialization(storage, txid, statement, scope, correlated, outer, arena)?;
        let changed = arena
            .alloc_slice_with(scope.n, |_| None)
            .map_err(|_| arena_full())?;
        Ok(Self {
            plan,
            statement,
            scope,
            from,
            base,
            params,
            hooks,
            outer,
            changed,
        })
    }

    /// Locks the sources of materialized `row`.
    pub(super) fn lock(
        &mut self,
        storage: &'a Storage,
        txid: u32,
        row: &[u8],
        rowids: &[Option<u64>],
        arena: &'a Arena,
    ) -> Result<LockedOutput<'a>, SqlError> {
        match super::lock_result_row(
            storage,
            txid,
            self.statement,
            self.scope,
            rowids,
            self.changed,
        )? {
            super::ResultRowLock::Skip => Ok(LockedOutput::Skip),
            super::ResultRowLock::Current => Ok(LockedOutput::Unchanged),
            super::ResultRowLock::Changed => {
                let mark = arena.mark();
                let rebuilt = arena
                    .alloc_slice_copy(row)
                    .map_err(|_| arena_full())
                    .and_then(|row| self.rebuild(storage, txid, row, rowids, arena));
                // Only the rebuilt row, in the persistent tail, outlives the
                // recheck.
                unsafe { arena.rewind_to(mark) };
                Ok(match rebuilt? {
                    Some(row) => LockedOutput::Rebuilt(row),
                    None => LockedOutput::Skip,
                })
            }
        }
    }

    fn rebuild(
        &mut self,
        storage: &'a Storage,
        txid: u32,
        row: &'a [u8],
        rowids: &[Option<u64>],
        arena: &'a Arena,
    ) -> Result<Option<&'a [u8]>, SqlError> {
        use super::scan::BoundRow;
        let scope = self.scope;
        let raw_at = self.plan.width + self.plan.n_keys;
        let presence_at = raw_at + scope.stored_columns();
        // `read` holds every source as the statement read it; `bound` swaps in
        // the latest versions of the changed sources.
        let read = arena
            .alloc_slice_with(scope.n, |_| None::<BoundRow<'a>>)
            .map_err(|_| arena_full())?;
        let bound = arena
            .alloc_slice_with(scope.n, |_| None::<BoundRow<'a>>)
            .map_err(|_| arena_full())?;
        let mut column_at = raw_at;
        for table in 0..scope.n {
            // Raw rows lay each source out at its stored width.
            let n_columns = scope.stored_width(table);
            if matches!(
                crate::sql::exec::decode_projected_pub(row, presence_at + table),
                Datum::Bool(true)
            ) {
                let values = arena
                    .alloc_slice_with(n_columns, |_| Datum::Null)
                    .map_err(|_| arena_full())?;
                for (column, value) in values.iter_mut().enumerate() {
                    *value = crate::sql::exec::decode_projected_col_record(
                        row,
                        column_at + column,
                        arena,
                    )?;
                }
                read[table] = Some(BoundRow::Values(values));
            }
            column_at += n_columns;
            bound[table] = read[table];
            if scope.lateral[table] {
                let tref = if table == 0 {
                    &self.from.base
                } else {
                    &self.from.joins[table - 1].table
                };
                match lateral_recheck(storage, txid, tref)? {
                    LateralRecheck::Keep => {}
                    LateralRecheck::RerunJoined(select) => {
                        if let Some(read_row) = read[table] {
                            let BoundRow::Values(read_values) = read_row else {
                                unreachable!("a read source is decoded");
                            };
                            let order = arena
                                .alloc_slice_with(scope.n, |table| table)
                                .map_err(|_| arena_full())?;
                            let as_read_row =
                                sources_row(scope, read, order, rowids).left_of(table);
                            let as_read = Chained {
                                inner: &as_read_row,
                                outer: self.outer,
                            };
                            let left = sources_row(scope, bound, order, rowids).left_of(table);
                            let latest = Chained {
                                inner: &left,
                                outer: self.outer,
                            };
                            match super::joined_from::recheck_lateral_select(
                                storage,
                                txid,
                                select,
                                arena,
                                self.params,
                                &as_read,
                                &latest,
                                read_values,
                                self.hooks.sequences,
                            )? {
                                super::joined_from::LateralRowRecheck::Rerun(values) => {
                                    bound[table] = Some(BoundRow::Values(values));
                                }
                                super::joined_from::LateralRowRecheck::Lost => {
                                    bound[table] = None;
                                    if table == 0
                                        || matches!(
                                            self.from.joins[table - 1].kind,
                                            crate::sql::ast::JoinKind::Inner
                                                | crate::sql::ast::JoinKind::Cross
                                                | crate::sql::ast::JoinKind::Comma
                                        )
                                    {
                                        return Ok(None);
                                    }
                                }
                                super::joined_from::LateralRowRecheck::Ambiguous => {
                                    return Err(lateral_recheck_unsupported());
                                }
                            }
                        }
                    }
                    LateralRecheck::Rerun => {
                        let order = arena
                            .alloc_slice_with(scope.n, |table| table)
                            .map_err(|_| arena_full())?;
                        let left = sources_row(scope, bound, order, rowids).left_of(table);
                        let chained = Chained {
                            inner: &left,
                            outer: self.outer,
                        };
                        let first = super::scan::first_lateral_row(
                            storage,
                            txid,
                            tref,
                            n_columns,
                            arena,
                            self.params,
                            &chained,
                            self.hooks,
                        )?;
                        bound[table] = first.map(BoundRow::Values);
                        // An inner or cross join to a source that yields no
                        // row drops the locked row; a left join null-extends.
                        if first.is_none()
                            && (table == 0
                                || matches!(
                                    self.from.joins[table - 1].kind,
                                    crate::sql::ast::JoinKind::Inner
                                        | crate::sql::ast::JoinKind::Cross
                                        | crate::sql::ast::JoinKind::Comma
                                ))
                        {
                            return Ok(None);
                        }
                    }
                }
            }
            if let Some(slot) = self.changed[table] {
                let rowid = rowids[table].ok_or_else(|| {
                    sql_err!(
                        sqlstate::INTERNAL_ERROR,
                        "a changed locked row has no identity"
                    )
                })?;
                let Some(values) = super::scan::latest_locked_source(
                    storage, txid, scope, table, slot, rowid, arena,
                )?
                else {
                    return Ok(None);
                };
                bound[table] = Some(BoundRow::Values(values));
            }
        }
        let order = arena
            .alloc_slice_with(scope.n, |table| table)
            .map_err(|_| arena_full())?;
        for (table, slot) in self.changed.iter().enumerate() {
            if slot.is_none() {
                continue;
            }
            let Some(policy) =
                super::scan::source_select_security(storage, scope, table, txid, arena)?
            else {
                continue;
            };
            let Some(BoundRow::Values(values)) = bound[table] else {
                unreachable!("a changed source is bound");
            };
            let context = crate::sql::exec::RowCtx {
                table_oid: None,
                def: scope.defs.get(table).expect("resolved"),
                values,
                alias: None,
            };
            if !super::row_security_passes(
                policy,
                &context,
                storage,
                txid,
                arena,
                self.params,
                self.hooks,
            )? {
                return Ok(None);
            }
        }
        // Each join condition runs again in join order. A join that no
        // longer matches drops an inner join's row and null-extends an outer
        // join's nullable side, as the re-run join plan would.
        for (join_index, join) in self.from.joins.iter().enumerate() {
            let Some(condition) = join.on.or(self.scope.join_on[join_index]) else {
                continue;
            };
            let right = join_index + 1;
            let left_present = bound[..right].iter().any(Option::is_some);
            let right_present = bound[right].is_some();
            if !left_present || !right_present {
                continue;
            }
            let joined = sources_row(scope, bound, order, rowids);
            let passes = super::plan::where_passes(
                condition,
                arena,
                self.params,
                &Chained {
                    inner: &joined,
                    outer: self.outer,
                },
                self.hooks,
            )?;
            if passes {
                continue;
            }
            match join.kind {
                crate::sql::ast::JoinKind::Inner
                | crate::sql::ast::JoinKind::Cross
                | crate::sql::ast::JoinKind::Comma => {
                    return Ok(None);
                }
                crate::sql::ast::JoinKind::Left => bound[right] = None,
                crate::sql::ast::JoinKind::Right => bound[..right].fill(None),
                crate::sql::ast::JoinKind::Full => {
                    return Err(sql_err!(
                        sqlstate::INTERNAL_ERROR,
                        "a locked row changed on the nullable side of a full join"
                    ));
                }
            }
        }
        let joined = sources_row(scope, bound, order, rowids);
        if let Some(predicate) = self.statement.where_clause {
            let read_row = sources_row(scope, read, order, rowids);
            let latest = Chained {
                inner: &joined,
                outer: self.outer,
            };
            let first = Chained {
                inner: &read_row,
                outer: self.outer,
            };
            if !locked_row_where_passes(
                storage,
                txid,
                predicate,
                &first,
                &latest,
                arena,
                self.params,
                self.hooks,
            )? {
                return Ok(None);
            }
        }
        let layout = MaterializedLayout {
            width: self.plan.width,
            n_keys: self.plan.n_keys,
            identities_at: self.plan.identities_at,
            stored_width: self.plan.width
                + self.plan.n_keys
                + self.plan.n_raw
                + self.plan.n_identities,
            raw_columns: self.plan.raw_columns.as_ref(),
        };
        // The re-run plan's first row stands for the locked row, also when a
        // set-returning projection expands it.
        let mut rebuilt = None;
        for_each_materialized_projection(
            storage,
            scope,
            self.statement,
            &joined,
            txid,
            arena,
            self.params,
            self.hooks,
            self.base,
            &[],
            &self.plan.order_exprs,
            self.plan.n_keys,
            if self.plan.any_postponed {
                Some(&self.plan.postponed)
            } else {
                None
            },
            self.plan.has_srf,
            self.outer,
            &mut |row, projected, keys| {
                if rebuilt.is_none() {
                    rebuilt = Some(encode_materialized_row(
                        scope, row, projected, keys, &layout, arena,
                    )?);
                }
                Ok(())
            },
        )?;
        Ok(rebuilt)
    }
}

/// A schema-only lookup for aggregate and grouped projections: it exposes the
/// scope's column *types* — so static type inference (e.g. `pg_typeof`) still
/// names a column's type when an aggregate over an empty or all-NULL group
/// yields NULL — while resolving no values. Value lookups fail exactly as an
/// empty row's would, so grouped columns still resolve through the group hook
/// and bare ungrouped references remain the same error.
pub(crate) struct ScopeSchema<'s, 'd>(pub(crate) &'s QueryScope<'d>);
impl<'a> ColumnLookup<'a> for ScopeSchema<'_, '_> {
    fn suggest_columns(
        &self,
        suggestion: &mut crate::sql::column_suggestion::ColumnSuggestion<'_>,
    ) {
        self.0.suggest_columns(suggestion);
    }

    fn column_type_mod(&self, qualifier: Option<&str>, name: &str) -> i32 {
        crate::sql::exec::ColTypeResolver::column_meta(&super::ScopeCols(self.0), qualifier, name)
            .map_or(-1, |column| column.type_mod)
    }

    fn whole_row_field_metadata(
        &self,
        table: &str,
        index: usize,
        catalog: Option<&dyn super::super::eval::CatalogAccess>,
    ) -> Option<(crate::util::StackStr<64>, crate::sql::exec::StaticTypeMeta)> {
        let entry = self.0.qualified_star_entry(table, index).ok()?;
        match entry {
            super::scope::ResolvedColumn::Table(table, column) => {
                super::super::eval::stored_row_field_metadata(
                    self.0.defs.get(table)?.columns().get(column)?,
                    catalog,
                )
            }
            super::scope::ResolvedColumn::TableOid(_) => None,
            super::scope::ResolvedColumn::Merged(_) => Some((
                crate::util::StackStr::from_str(self.0.output_name(entry)),
                crate::sql::exec::StaticTypeMeta {
                    type_oid: self.0.output_type(entry).oid(),
                    ctype: self.0.output_type(entry),
                    type_mod: -1,
                    collation: self.0.output_collation(entry),
                },
            )),
        }
    }

    fn resolve_column_type(
        &self,
        qualifier: Option<&str>,
        name: &str,
    ) -> Result<ColType, SqlError> {
        ColumnLookup::<'a>::resolve_column_type_unsuggested(self, qualifier, name)
            .map_err(|error| self.0.suggested(error, qualifier, name))
    }

    fn resolve_column_type_unsuggested(
        &self,
        qualifier: Option<&str>,
        name: &str,
    ) -> Result<ColType, SqlError> {
        match self.0.find_column_unsuggested(qualifier, name) {
            Ok(column) => Ok(self.0.output_type(column)),
            Err(error) if qualifier.is_none() && error.sqlstate == sqlstate::UNDEFINED_COLUMN => {
                self.whole_row_type(name).ok_or(error)
            }
            Err(error) => Err(error),
        }
    }

    fn is_empty_scope(&self) -> bool {
        self.0.n == 0
    }

    fn lookup(&self, _qualifier: Option<&str>, name: &str) -> Result<Datum<'a>, SqlError> {
        Err(sql_err!(
            sqlstate::UNDEFINED_COLUMN,
            "column \"{}\" does not exist",
            name
        ))
    }
    fn col_type(&self, qualifier: Option<&str>, name: &str) -> Option<ColType> {
        match self.0.find_column_unsuggested(qualifier, name) {
            Ok(entry) => Some(self.0.output_type(entry)),
            Err(error)
                if error.sqlstate == sqlstate::UNDEFINED_COLUMN
                    && qualifier.is_none()
                    && (self.0.using_alias(name).is_some() || self.0.source(name).is_ok()) =>
            {
                self.0.whole_row_type(name)
            }
            Err(_) => None,
        }
    }
    fn collation(&self, qualifier: Option<&str>, name: &str) -> Collation {
        self.0
            .find_column_unsuggested(qualifier, name)
            .ok()
            .map(|entry| self.0.output_collation(entry))
            .unwrap_or(Collation::None)
    }
    fn record_field_collation(&self, base: &Expr<'a>, field: &str) -> Collation {
        crate::sql::exec::record_field_metadata(base, field, &super::ScopeCols(self.0))
            .map_or(Collation::None, |meta| meta.collation)
    }
    fn column_user_type(
        &self,
        qualifier: Option<&str>,
        name: &str,
    ) -> Option<crate::storage::UserTypeName> {
        match self.0.find_column_unsuggested(qualifier, name).ok()? {
            super::scope::ResolvedColumn::Table(table, column) => {
                self.0.defs.get(table)?.columns.get(column)?.user_type
            }
            super::scope::ResolvedColumn::Merged(_) | super::scope::ResolvedColumn::TableOid(_) => {
                None
            }
        }
    }
    fn column_identity(&self, qualifier: Option<&str>, name: &str) -> Option<(u32, u32)> {
        match self.0.find_column_unsuggested(qualifier, name).ok()? {
            super::scope::ResolvedColumn::Table(t, c) => Some((t as u32, c as u32)),
            super::scope::ResolvedColumn::Merged(m) => Some((u32::MAX, m as u32)),
            super::scope::ResolvedColumn::TableOid(table) => Some((table as u32, u32::MAX)),
        }
    }
    fn whole_row_is_scalar(&self, table: &str) -> bool {
        self.0.func_scalar_type(table).is_some()
    }

    fn whole_row_type(&self, table: &str) -> Option<ColType> {
        self.0.whole_row_type(table)
    }
}

/// [`ScopeSchema`] over the first `visible` FROM items of a scope: what a
/// `LATERAL` item's body reads of the items to its left, never its own or a
/// later item's columns.
#[derive(Clone, Copy)]
pub(crate) struct ScopePrefix<'s, 'd> {
    pub(crate) scope: &'s QueryScope<'d>,
    pub(crate) visible: usize,
}

impl ScopePrefix<'_, '_> {
    fn whole(&self) -> bool {
        self.visible >= self.scope.n
    }

    fn entry(
        &self,
        qualifier: Option<&str>,
        name: &str,
    ) -> Result<super::scope::ResolvedColumn, SqlError> {
        self.scope
            .find_column_among_unsuggested(self.visible, qualifier, name)
    }

    fn names_visible_item(&self, table: &str) -> bool {
        self.scope
            .source(table)
            .is_ok_and(|source| source.table < self.visible)
    }
}

impl<'a> ColumnLookup<'a> for ScopePrefix<'_, '_> {
    fn column_type_mod(&self, qualifier: Option<&str>, name: &str) -> i32 {
        if self.whole() {
            return ColumnLookup::<'a>::column_type_mod(&ScopeSchema(self.scope), qualifier, name);
        }
        match self.entry(qualifier, name) {
            Ok(super::scope::ResolvedColumn::Table(table, column)) => self
                .scope
                .defs
                .get(table)
                .and_then(|definition| definition.columns.get(column))
                .map_or(-1, |column| column.type_mod),
            _ => -1,
        }
    }

    fn whole_row_field_metadata(
        &self,
        table: &str,
        index: usize,
        catalog: Option<&dyn super::super::eval::CatalogAccess>,
    ) -> Option<(crate::util::StackStr<64>, crate::sql::exec::StaticTypeMeta)> {
        if !self.whole() && !self.names_visible_item(table) {
            return None;
        }
        ColumnLookup::<'a>::whole_row_field_metadata(
            &ScopeSchema(self.scope),
            table,
            index,
            catalog,
        )
    }

    fn resolve_column_type(
        &self,
        qualifier: Option<&str>,
        name: &str,
    ) -> Result<ColType, SqlError> {
        if self.whole() {
            return ColumnLookup::<'a>::resolve_column_type(
                &ScopeSchema(self.scope),
                qualifier,
                name,
            );
        }
        match self.entry(qualifier, name) {
            Ok(column) => Ok(self.scope.output_type(column)),
            Err(error)
                if qualifier.is_none()
                    && error.sqlstate == sqlstate::UNDEFINED_COLUMN
                    && self.names_visible_item(name) =>
            {
                self.scope
                    .whole_row_type(name)
                    .ok_or_else(|| self.scope.suggested(error, qualifier, name))
            }
            Err(error) => Err(self.scope.suggested(error, qualifier, name)),
        }
    }

    fn is_empty_scope(&self) -> bool {
        self.visible.min(self.scope.n) == 0
    }

    fn lookup(&self, _qualifier: Option<&str>, name: &str) -> Result<Datum<'a>, SqlError> {
        Err(sql_err!(
            sqlstate::UNDEFINED_COLUMN,
            "column \"{}\" does not exist",
            name
        ))
    }

    fn col_type(&self, qualifier: Option<&str>, name: &str) -> Option<ColType> {
        if self.whole() {
            return ColumnLookup::<'a>::col_type(&ScopeSchema(self.scope), qualifier, name);
        }
        match self.entry(qualifier, name) {
            Ok(entry) => Some(self.scope.output_type(entry)),
            Err(error)
                if error.sqlstate == sqlstate::UNDEFINED_COLUMN
                    && qualifier.is_none()
                    && self.names_visible_item(name) =>
            {
                self.scope.whole_row_type(name)
            }
            Err(_) => None,
        }
    }

    fn collation(&self, qualifier: Option<&str>, name: &str) -> Collation {
        self.entry(qualifier, name)
            .ok()
            .map(|entry| self.scope.output_collation(entry))
            .unwrap_or(Collation::None)
    }

    fn record_field_collation(&self, base: &Expr<'a>, field: &str) -> Collation {
        ScopeSchema(self.scope).record_field_collation(base, field)
    }

    fn column_user_type(
        &self,
        qualifier: Option<&str>,
        name: &str,
    ) -> Option<crate::storage::UserTypeName> {
        match self.entry(qualifier, name).ok()? {
            super::scope::ResolvedColumn::Table(table, column) => {
                self.scope.defs.get(table)?.columns.get(column)?.user_type
            }
            super::scope::ResolvedColumn::Merged(_) | super::scope::ResolvedColumn::TableOid(_) => {
                None
            }
        }
    }

    fn column_identity(&self, qualifier: Option<&str>, name: &str) -> Option<(u32, u32)> {
        match self.entry(qualifier, name).ok()? {
            super::scope::ResolvedColumn::Table(t, c) => Some((t as u32, c as u32)),
            super::scope::ResolvedColumn::Merged(m) => Some((u32::MAX, m as u32)),
            super::scope::ResolvedColumn::TableOid(table) => Some((table as u32, u32::MAX)),
        }
    }

    fn whole_row_is_scalar(&self, table: &str) -> bool {
        (self.whole() || self.names_visible_item(table))
            && self.scope.func_scalar_type(table).is_some()
    }

    fn whole_row_type(&self, table: &str) -> Option<ColType> {
        (self.whole() || self.names_visible_item(table))
            .then(|| self.scope.whole_row_type(table))
            .flatten()
    }
}

/// `postponed` marks output deferred until after sorting. Encoded rows append
/// raw source columns and source-presence flags starting at `raw_at`.
pub(crate) struct PostponedProjection {
    postponed: [bool; MAX_PROJ],
    raw_at: usize,
}

/// Fills `out` with an encoded row's visible columns, evaluating any postponed
/// projection items from the row's appended raw source columns. OFFSET rows
/// and WITH TIES' first non-tied lookahead are evaluated too; later rows are not.
#[expect(clippy::too_many_arguments, reason = "query pipeline plumbing")]
pub(crate) fn finalize_projected_row<'a>(
    bytes: &'a [u8],
    width: usize,
    deferred: Option<&PostponedProjection>,
    statement: &'a Select<'a>,
    scope: &QueryScope<'a>,
    storage: &Storage,
    txid: u32,
    arena: &'a Arena,
    params: &[Datum<'a>],
    hooks: &EvalHooks<'_, 'a>,
    outer: Option<&dyn ColumnLookup<'a>>,
    out: &mut [Datum<'a>; MAX_PROJ],
) -> Result<(), SqlError> {
    for (i, slot) in out.iter_mut().take(width).enumerate() {
        *slot = crate::sql::exec::decode_projected_col_record(bytes, i, arena)?;
    }
    let Some(d) = deferred else { return Ok(()) };
    let raw_row = EncodedRawRow {
        scope,
        bytes,
        raw_at: d.raw_at,
        arena,
    };
    let row = Chained {
        inner: &raw_row,
        outer,
    };
    // `postponed` is indexed by item; wildcards (never postponed) advance the
    // output slot by their column count.
    let mut slot = 0usize;
    for (i, item) in statement.items.iter().enumerate() {
        match item {
            SelectItem::Wildcard(_) => slot += scope.star_columns(),
            SelectItem::TableWildcard(q) => {
                slot += scope.qualified_star_columns(q)?;
            }
            SelectItem::RecordStar(base) => {
                slot += record_star_width(base, Some(scope), storage, txid, arena)?;
            }
            SelectItem::Expr { expression, .. } => {
                if d.postponed[i] {
                    out[slot] = eval_full(expression, arena, params, &row, hooks)?;
                }
                slot += 1;
            }
        }
    }
    Ok(())
}

/// Materialized rows, visible width, postponed-projection plan, and the hidden
/// column at which physical source row identities begin.
type MaterializedSelect<'a> = (&'a [&'a [u8]], usize, Option<PostponedProjection>, usize);

type ExternalRowEmitter<'a> =
    dyn for<'row> FnMut(&[Datum<'row>], &[Option<u64>]) -> Result<bool, SqlError> + 'a;

pub(super) struct MaterializationPlan<'a> {
    n_order: usize,
    n_on: usize,
    n_keys: usize,
    width: usize,
    n_raw: usize,
    raw_columns: Option<super::scan::PaxReadDemand>,
    identities_at: usize,
    n_identities: usize,
    where_correlated: &'a [&'a Expr<'a>],
    where_in_scan: Option<&'a Expr<'a>>,
    order_exprs: [Option<super::ResolvedSortKey<'a>>; MAX_PROJ],
    key_collations: [Collation; MAX_PROJ],
    postponed: [bool; MAX_PROJ],
    any_postponed: bool,
    has_srf: bool,
}

fn projected_collations<'a>(
    storage: &Storage,
    txid: u32,
    arena: &Arena,
    statement: &Select<'a>,
    scope: &QueryScope<'a>,
) -> Result<[Collation; MAX_PROJ], SqlError> {
    let mut collations = [Collation::None; MAX_PROJ];
    let mut width = 0;
    for item in statement.items {
        let remaining = match item {
            SelectItem::Expr { .. } => 1,
            SelectItem::Wildcard(_) => scope.star_columns(),
            SelectItem::TableWildcard(qualifier) => scope.qualified_star_columns(qualifier)?,
            SelectItem::RecordStar(base) => {
                record_star_width(base, Some(scope), storage, txid, arena)?
            }
        };
        if width + remaining > MAX_PROJ {
            return Err(sql_err!(
                sqlstate::TOO_MANY_COLUMNS,
                "target lists can have at most 1664 entries"
            ));
        }
        match item {
            SelectItem::Expr { expression, .. } => {
                collations[width] = scope.expression_collation(storage, arena, expression)?;
                width += 1;
            }
            SelectItem::Wildcard(_) => {
                for index in 0..scope.star_columns() {
                    collations[width] = scope.output_collation(scope.star_entry(index));
                    width += 1;
                }
            }
            SelectItem::TableWildcard(qualifier) => {
                for index in 0..scope.qualified_star_columns(qualifier)? {
                    collations[width] =
                        scope.output_collation(scope.qualified_star_entry(qualifier, index)?);
                    width += 1;
                }
            }
            SelectItem::RecordStar(base) => {
                crate::sql::exec::record_shape_metadata(
                    base,
                    &super::CatalogScopeCols {
                        scope,
                        outer: None,
                        storage,
                        txid,
                        arena,
                    },
                    |_, metadata| {
                        collations[width] = metadata.collation;
                        width += 1;
                    },
                )?;
            }
        }
    }
    Ok(collations)
}

/// DISTINCT over encoded rows; a stable sort keeps each group's first input
/// row, the representative PostgreSQL's hashed DISTINCT returns.
fn sort_dedup_collated(
    storage: &Storage,
    txid: u32,
    arena: &Arena,
    rows: &mut [&[u8]],
    width: usize,
    collations: &[Collation],
) -> Result<usize, SqlError> {
    for row in rows.iter() {
        for (index, &collation) in collations.iter().take(width).enumerate() {
            storage.validate_text_collation(
                collation,
                &crate::sql::exec::decode_projected_pub(row, index),
            )?;
        }
    }
    let compare = |left: &[u8], right: &[u8]| {
        for (index, &collation) in collations.iter().take(width).enumerate() {
            let ordering = compare_materialized_key(
                storage,
                txid,
                collation,
                crate::sql::exec::ProjectedValue::column(left, index),
                crate::sql::exec::ProjectedValue::column(right, index),
            )?;
            if !ordering.is_eq() {
                return Ok(ordering);
            }
        }
        Ok(core::cmp::Ordering::Equal)
    };
    crate::mem::arena::try_stable_sort_via(
        arena,
        rows,
        |left, right| compare(left, right),
        |_| arena_full(),
    )?;
    let mut unique = 0;
    for index in 0..rows.len() {
        if index == 0 || !compare(rows[index - 1], rows[index])?.is_eq() {
            rows[unique] = rows[index];
            unique += 1;
        }
    }
    Ok(unique)
}

pub(super) fn prepare_materialization<'a>(
    storage: &Storage,
    txid: u32,
    statement: &'a Select<'a>,
    scope: &QueryScope<'a>,
    correlated: &'a [&'a Expr<'a>],
    outer: Option<&dyn ColumnLookup<'a>>,
    arena: &'a Arena,
) -> Result<MaterializationPlan<'a>, SqlError> {
    let n_order = statement.order_by.len();
    let n_on = statement.distinct_on.len();
    let where_correlated = correlated_in_expression(statement.where_clause, correlated, arena)?;
    let where_in_scan = correlated_scan_conjuncts(statement.where_clause, where_correlated, arena)?;

    if n_order + n_on > MAX_PROJ {
        return Err(sql_err!(sqlstate::TOO_MANY_COLUMNS, "too many columns"));
    }
    let mut order_exprs: [Option<super::ResolvedSortKey<'a>>; MAX_PROJ] = [None; MAX_PROJ];
    let mut key_collations = [Collation::None; MAX_PROJ];
    for (key, order) in statement.order_by.iter().enumerate() {
        let resolved = super::resolve_sort_key(
            order.expression,
            statement.items,
            scope,
            arena,
            storage,
            txid,
            &order_exprs[..key],
        )?;
        key_collations[key] = scope.expression_collation(storage, arena, resolved.expression)?;
        order_exprs[key] = Some(resolved);
    }
    for (index, on) in statement.distinct_on.iter().enumerate() {
        let resolved = super::resolve_distinct_on_key(
            on,
            statement.items,
            scope,
            arena,
            storage,
            txid,
            &order_exprs[..n_order + index],
        )?;
        key_collations[n_order + index] =
            scope.expression_collation(storage, arena, resolved.expression)?;
        order_exprs[n_order + index] = Some(resolved);
    }
    let n_keys = n_order + n_on;
    let mut order_targets = [&Expr::Null; MAX_PROJ];
    for (target, key) in order_targets.iter_mut().zip(&order_exprs[..n_keys]) {
        *target = key.expect("resolved").expression;
    }
    check_distinct_on_order(
        &order_targets[..n_order],
        &order_targets[n_order..n_keys],
        statement.distinct_on,
    )?;
    if statement.distinct && n_on == 0 {
        for expression in order_exprs.iter().take(n_order) {
            let target = expression.expect("resolved").expression;
            let in_list = statement.items.iter().any(|item| {
                matches!(item, SelectItem::Expr { expression, .. } if **expression == *target)
            });
            if !in_list {
                return Err(sql_err!(
                    sqlstate::INVALID_COLUMN_REFERENCE,
                    "for SELECT DISTINCT, ORDER BY expressions must appear in select list"
                ));
            }
        }
    }

    let mut width = 0usize;
    for item in statement.items {
        width += match item {
            SelectItem::Wildcard(_) => scope.star_columns(),
            SelectItem::TableWildcard(qualifier) => scope.qualified_star_columns(qualifier)?,
            SelectItem::RecordStar(base) => {
                record_star_width(base, Some(scope), storage, txid, arena)?
            }
            SelectItem::Expr { .. } => 1,
        };
    }

    let has_srf = has_project_set(statement.items, storage, txid);
    let defer_allowed = n_order > 0
        && !statement.distinct
        && statement.distinct_on.is_empty()
        && correlated.is_empty()
        && !has_srf
        && statement.items.len() <= MAX_PROJ;
    let mut postponed = [false; MAX_PROJ];
    let mut any_postponed = false;
    let mut raw_needed = false;
    if defer_allowed {
        let schema = ScopeSchema(scope);
        let row = Chained {
            inner: &schema,
            outer,
        };
        let catalog = super::storage_catalog(storage, arena, txid);
        let columns = crate::sql::eval::RuntimeColumnTypes {
            row: &row,
            catalog: Some(&catalog),
        };
        for (index, item) in statement.items.iter().enumerate() {
            let SelectItem::Expr { expression, .. } = item else {
                continue;
            };
            let ordered_by = order_exprs[..n_order]
                .iter()
                .any(|order| order.is_some_and(|target| *target.expression == **expression));
            if ordered_by {
                continue;
            }
            let properties = super::predicate_safety::expression_properties(
                expression,
                &columns,
                storage,
                txid,
                super::predicate_safety::AnalysisStage::RuntimeProjection,
                arena,
            )?;
            let sorted_expensive =
                statement.limit.is_some() && postpone_cost(expression, scope, arena) > 20.0;
            if properties.volatile || sorted_expensive {
                postponed[index] = true;
                any_postponed = true;
                raw_needed |= properties.columns;
            }
        }
    }
    let locking = !statement.locking.is_empty();
    let raw_columns = if locking {
        // A locked row whose latest version replaced the one read is
        // evaluated again from its sources, so rows keep every observed
        // source column.
        let from = statement
            .from
            .as_ref()
            .ok_or_else(|| sql_err!(sqlstate::INTERNAL_ERROR, "locked query has no source"))?;
        Some(observed_source_columns(
            statement,
            scope,
            from,
            &order_exprs,
            n_keys,
        ))
    } else if raw_needed {
        let expressions = arena
            .alloc_slice_with(statement.items.len(), |_| &Expr::Null)
            .map_err(|_| arena_full())?;
        let mut count = 0usize;
        for (item, postponed) in statement.items.iter().zip(postponed.iter()) {
            if *postponed && let SelectItem::Expr { expression, .. } = item {
                expressions[count] = expression;
                count += 1;
            }
        }
        let from = statement.from.as_ref().ok_or_else(|| {
            sql_err!(
                sqlstate::INTERNAL_ERROR,
                "materialized projection has no source"
            )
        })?;
        Some(pax_column_demand(scope, from, &expressions[..count]))
    } else {
        None
    };
    // Presence is independent of field NULLs, including zero-column rows.
    let n_raw = if raw_needed || locking {
        scope.stored_columns() + scope.n
    } else {
        0
    };
    let n_identities = if statement.locking.is_empty() {
        0
    } else {
        scope.n
    };
    let identities_at = width + n_keys + n_raw;
    Ok(MaterializationPlan {
        n_order,
        n_on,
        n_keys,
        width,
        n_raw,
        raw_columns,
        identities_at,
        n_identities,
        where_correlated,
        where_in_scan,
        order_exprs,
        key_collations,
        postponed,
        any_postponed,
        has_srf,
    })
}

/// The physical PAX columns a materialization can observe.
///
/// Postponed expressions and correlated predicates are evaluated outside the
/// direct scan, but their source-column dependencies remain statically known
/// and belong in the same physical proof.
fn materialization_pax_columns<'a>(
    statement: &'a Select<'a>,
    scope: &QueryScope<'a>,
    from: &'a FromClause<'a>,
    plan: &MaterializationPlan<'a>,
) -> super::scan::PaxReadDemand {
    observed_source_columns(statement, scope, from, &plan.order_exprs, plan.n_keys)
}

/// The source columns a query's projection, WHERE, and sort keys observe.
fn observed_source_columns<'a>(
    statement: &'a Select<'a>,
    scope: &QueryScope<'a>,
    from: &'a FromClause<'a>,
    order_exprs: &[Option<super::ResolvedSortKey<'a>>; MAX_PROJ],
    n_keys: usize,
) -> super::scan::PaxReadDemand {
    // A pulled-up LATERAL subquery's recheck finds its source row again from
    // every value it produced.
    if scope.lateral[..scope.n].iter().any(|&lateral| lateral) {
        return super::scan::PaxReadDemand::full_row(
            super::scan::PaxFullRowReason::RetainedJoinRow,
        );
    }
    if statement
        .items
        .iter()
        .any(|item| matches!(item, SelectItem::Wildcard(_) | SelectItem::TableWildcard(_)))
    {
        return super::scan::PaxReadDemand::full_row(
            super::scan::PaxFullRowReason::WildcardProjection,
        );
    }
    let mut expressions = [&Expr::Null; MAX_PROJ * 2 + 1];
    if statement.items.len() + 1 + n_keys > expressions.len() {
        return super::scan::PaxReadDemand::full_row(
            super::scan::PaxFullRowReason::WildcardProjection,
        );
    }
    let mut count = 0usize;
    for item in statement.items {
        match item {
            SelectItem::Expr { expression, .. } | SelectItem::RecordStar(expression) => {
                expressions[count] = expression;
                count += 1;
            }
            SelectItem::Wildcard(_) | SelectItem::TableWildcard(_) => {
                unreachable!("returned above")
            }
        }
    }
    if let Some(predicate) = statement.where_clause {
        expressions[count] = predicate;
        count += 1;
    }
    for expression in order_exprs.iter().take(n_keys) {
        expressions[count] = expression.expect("resolved").expression;
        count += 1;
    }
    pax_column_demand(scope, from, &expressions[..count])
}

/// The row-producing half of DISTINCT / ORDER BY execution: materialize
/// projected rows (with hidden ORDER BY key columns), dedupe on the visible
/// prefix for DISTINCT, and sort by the hidden keys. Returns `(rows, width)`;
/// the caller pages with LIMIT/OFFSET and emits. Shared by the wire path
/// (`materialized_select`) and the row-source path (`select_into_rows`).
#[expect(clippy::too_many_arguments, reason = "query pipeline plumbing")]
pub(crate) fn materialized_rows<'a>(
    storage: &'a Storage,
    scope: &QueryScope<'a>,
    from: &'a FromClause<'a>,
    txid: u32,
    statement: &'a Select<'a>,
    arena: &'a Arena,
    params: &[Datum<'a>],
    hooks: &EvalHooks<'_, 'a>,
    correlated: &'a [&'a Expr<'a>],
    base: &SubqueryValues<'a, 'a>,
    outer: Option<&dyn ColumnLookup<'a>>,
    rank_window: Option<usize>,
    stages: &instrument::Stages,
) -> Result<MaterializedSelect<'a>, SqlError> {
    let plan = prepare_materialization(storage, txid, statement, scope, correlated, outer, arena)?;
    let visible_collations = projected_collations(storage, txid, arena, statement, scope)?;
    let pax_columns = materialization_pax_columns(statement, scope, from, &plan);
    let MaterializationPlan {
        n_order,
        n_on,
        n_keys,
        width,
        n_raw,
        raw_columns,
        identities_at,
        n_identities,
        where_correlated,
        where_in_scan,
        order_exprs,
        key_collations,
        postponed,
        any_postponed,
        has_srf,
    } = plan;
    let ordered_candidates = if !statement.distinct
        && statement.distinct_on.is_empty()
        && n_order > 0
        && n_keys == n_order
        && correlated.is_empty()
        && !has_srf
        && scope.n == 1
        && from.joins.is_empty()
        && from.base.sample.is_none()
    {
        match super::scan::ordered_index_access_plan(
            storage,
            scope,
            txid,
            from.base.inheritance == crate::sql::ast::RelationInheritance::Descendants,
            where_in_scan,
            statement.items,
            statement.order_by,
            arena,
        )? {
            Some(ordered) => super::scan::ordered_indexed_candidates(
                storage,
                scope,
                txid,
                ordered,
                rank_window.filter(|_| {
                    where_in_scan.is_none() && !statement.with_ties && statement.locking.is_empty()
                }),
                arena,
                params,
                hooks,
            )?,
            None => None,
        }
    } else {
        None
    };

    let stored_width = width + n_keys + n_raw + n_identities;
    let mut rows = ArenaList::new_persistent(arena);
    // Evaluate each projection once, in scan order. Encoded rows and their
    // references survive the scan's per-row scratch rewinds in the arena tail.
    let mut encode = |row: &JoinRow<'_, 'a, '_>| {
        for_each_materialized_projection(
            storage,
            scope,
            statement,
            row,
            txid,
            arena,
            params,
            hooks,
            base,
            where_correlated,
            &order_exprs,
            n_keys,
            if any_postponed {
                Some(&postponed)
            } else {
                None
            },
            has_srf,
            outer,
            &mut |row, projected, keys| {
                let output = encode_materialized_row(
                    scope,
                    row,
                    projected,
                    keys,
                    &MaterializedLayout {
                        width,
                        n_keys,
                        identities_at,
                        stored_width,
                        raw_columns: raw_columns.as_ref(),
                    },
                    arena,
                )?;
                rows.push(output).map_err(|_| arena_full())?;
                Ok(())
            },
        )?;
        Ok(true)
    };
    if let Some(indexed) = ordered_candidates.as_ref() {
        scan_source_recycling_with_indexed_candidates(
            storage,
            scope,
            from,
            txid,
            where_in_scan,
            arena,
            params,
            hooks,
            outer,
            pax_columns,
            indexed,
            &mut encode,
        )?;
    } else {
        scan_source_recycling_with_pax_columns(
            storage,
            scope,
            from,
            txid,
            where_in_scan,
            arena,
            params,
            hooks,
            outer,
            pax_columns,
            &mut encode,
        )?;
    }
    let rows = rows.into_mut_slice().map_err(|_| arena_full())?;

    // Plain DISTINCT dedups on the visible prefix before the ORDER BY sort.
    // DISTINCT ON dedups on its keys *after* the sort (below), so the first
    // row per key in ORDER BY order survives.
    let mut live = rows.len();
    let scanned = live;
    if statement.distinct && n_on == 0 {
        live = sort_dedup_collated(storage, txid, arena, rows, width, &visible_collations)?;
    }
    let rows = &mut rows[..live];

    // Sort by ORDER BY (a stable sort so DISTINCT ON without ORDER BY keeps
    // the first-scanned row per key). For DISTINCT ON the ON keys are appended
    // ascending as a tiebreak — a no-op when ORDER BY already begins with them
    // (as PostgreSQL requires), but it groups equal keys when ORDER BY is
    // absent so the run dedup below works.
    if (n_order > 0 && ordered_candidates.is_none()) || n_on > 0 {
        for row in rows.iter() {
            for (key, &collation) in key_collations.iter().enumerate().take(n_keys) {
                storage.validate_text_collation(
                    collation,
                    &crate::sql::exec::decode_projected_pub(row, width + key),
                )?;
            }
        }
        // A limit directly over the sort bounds it: like PostgreSQL's,
        // past twice the bound it keeps only a heap of the rows the limit
        // returns.
        let bound =
            rank_window.filter(|&bound| !statement.distinct && n_on == 0 && rows.len() > bound * 2);
        let compare = |a: &&[u8], b: &&[u8]| {
            compare_materialized_rows(
                storage,
                txid,
                &key_collations,
                statement,
                width,
                n_order,
                n_on,
                false,
                a,
                b,
            )
        };
        match bound {
            Some(bound) => {
                crate::mem::arena::try_stable_top_n_via(arena, rows, bound, compare, |_| {
                    arena_full()
                })?
            }
            None => crate::mem::arena::try_stable_sort_via(arena, rows, compare, |_| arena_full())?,
        }
        stages
            .sort
            .sorted(instrument::SortSpace::in_memory(rows, bound.is_some()));
    }
    // A sort under a DISTINCT returns every row it sorted.
    if stages.unique.measured() {
        stages.sort.rows(scanned as u64);
    }

    // DISTINCT ON: keep the first row of each run of equal ON keys.
    let rows: &mut [&[u8]] = if n_on > 0 {
        let mut unique = 0usize;
        for i in 0..rows.len() {
            if i == 0 {
                rows[unique] = rows[i];
                unique += 1;
                continue;
            }
            let mut same = true;
            for j in 0..n_on {
                let ka = crate::sql::exec::ProjectedValue::column(rows[i], width + n_order + j);
                let kb = crate::sql::exec::ProjectedValue::column(rows[i - 1], width + n_order + j);
                let equal =
                    compare_materialized_key(storage, txid, key_collations[n_order + j], ka, kb)?
                        .is_eq();
                if !equal {
                    same = false;
                    break;
                }
            }
            if !same {
                rows[unique] = rows[i];
                unique += 1;
            }
        }
        &mut rows[..unique]
    } else {
        rows
    };

    let deferred = any_postponed.then_some(PostponedProjection {
        postponed,
        raw_at: width + n_keys,
    });
    Ok((rows, width, deferred, identities_at))
}

/// Orders two encoded key values, NULL last, under `collation`; see
/// [`crate::sql::exec::compare_projected_key`].
pub(crate) fn compare_materialized_key(
    storage: &Storage,
    txid: u32,
    collation: Collation,
    left: crate::sql::exec::ProjectedValue<'_>,
    right: crate::sql::exec::ProjectedValue<'_>,
) -> Result<core::cmp::Ordering, SqlError> {
    crate::sql::exec::compare_projected_key(
        collation,
        (false, false),
        left,
        right,
        &|collation, x, y| {
            crate::sql::eval::compare_datums_collated(storage, txid, collation, x, y)
        },
    )
}

/// Whether two sorted rows tie on their `n_order` hidden ORDER BY key columns
/// (stored after `width`), with NULLs comparing equal — the `WITH TIES` peer
/// test, independent of ASC/DESC.
fn order_keys_equal_collated(
    storage: &Storage,
    txid: u32,
    collations: &[Collation],
    a: &[u8],
    b: &[u8],
    width: usize,
    n_order: usize,
) -> Result<bool, SqlError> {
    for (k, &collation) in collations.iter().enumerate().take(n_order) {
        let ka = crate::sql::exec::ProjectedValue::column(a, width + k);
        let kb = crate::sql::exec::ProjectedValue::column(b, width + k);
        if !compare_materialized_key(storage, txid, collation, ka, kb)?.is_eq() {
            return Ok(false);
        }
    }
    Ok(true)
}

/// Orders one encoded key under its `ORDER BY` direction `(descending,
/// nulls_first)`; see [`crate::sql::exec::compare_projected_key`].
pub(crate) fn compare_ordered_key(
    storage: &Storage,
    txid: u32,
    collation: Collation,
    direction: (bool, bool),
    left: crate::sql::exec::ProjectedValue<'_>,
    right: crate::sql::exec::ProjectedValue<'_>,
) -> Result<core::cmp::Ordering, SqlError> {
    crate::sql::exec::compare_projected_key(
        collation,
        direction,
        left,
        right,
        &|collation, x, y| {
            crate::sql::eval::compare_datums_collated(storage, txid, collation, x, y)
        },
    )
}

#[expect(
    clippy::too_many_arguments,
    reason = "external sort comparator receives plan fields explicitly"
)]
fn compare_materialized_rows(
    storage: &Storage,
    txid: u32,
    key_collations: &[Collation],
    statement: &Select<'_>,
    width: usize,
    n_order: usize,
    n_on: usize,
    distinct_prefix: bool,
    a: &[u8],
    b: &[u8],
) -> Result<core::cmp::Ordering, SqlError> {
    for (key, order) in statement.order_by.iter().enumerate() {
        let ordering = compare_ordered_key(
            storage,
            txid,
            key_collations[key],
            (order.descending, order.nulls_first),
            crate::sql::exec::ProjectedValue::column(a, width + key),
            crate::sql::exec::ProjectedValue::column(b, width + key),
        )?;
        if !ordering.is_eq() {
            return Ok(ordering);
        }
    }
    for index in 0..n_on {
        let ordering = compare_materialized_key(
            storage,
            txid,
            key_collations[n_order + index],
            crate::sql::exec::ProjectedValue::column(a, width + n_order + index),
            crate::sql::exec::ProjectedValue::column(b, width + n_order + index),
        )?;
        if !ordering.is_eq() {
            return Ok(ordering);
        }
    }
    if distinct_prefix {
        crate::sql::exec::compare_projected_prefix(storage, txid, a, b, width)
    } else {
        Ok(core::cmp::Ordering::Equal)
    }
}

/// PostgreSQL's rule for `DISTINCT ON` with `ORDER BY`: the leading
/// `ORDER BY` items are the `DISTINCT ON` expressions, in any order, and
/// those missing from `ORDER BY` are allowed only when every item is one.
/// The sort then uses `ORDER BY`, followed by the `DISTINCT ON` expressions
/// ascending. `on` are the resolved `DISTINCT ON` expressions, `written`
/// their text, which a mismatch points at.
pub(super) fn check_distinct_on_order(
    order: &[&Expr<'_>],
    on: &[&Expr<'_>],
    written: &[&Expr<'_>],
) -> Result<(), SqlError> {
    let mismatch = |index: usize| {
        crate::sql::error_position::at_expression(
            sql_err!(
                sqlstate::INVALID_COLUMN_REFERENCE,
                "SELECT DISTINCT ON expressions must match initial ORDER BY expressions"
            ),
            written[index],
        )
    };
    let mut skipped = false;
    for key in order {
        if let Some(index) = on.iter().position(|expression| **expression == **key) {
            if skipped {
                return Err(mismatch(index));
            }
        } else {
            skipped = true;
        }
    }
    if skipped
        && let Some(index) = on
            .iter()
            .position(|expression| !order.iter().any(|key| **key == **expression))
    {
        return Err(mismatch(index));
    }
    Ok(())
}

#[expect(
    clippy::too_many_arguments,
    reason = "distinct comparison receives its row layout and transaction explicitly"
)]
fn distinct_on_keys_equal(
    storage: &Storage,
    txid: u32,
    collations: &[Collation],
    a: &[u8],
    b: &[u8],
    width: usize,
    n_order: usize,
    n_on: usize,
) -> Result<bool, SqlError> {
    for index in 0..n_on {
        let left = crate::sql::exec::ProjectedValue::column(a, width + n_order + index);
        let right = crate::sql::exec::ProjectedValue::column(b, width + n_order + index);
        if !compare_materialized_key(storage, txid, collations[n_order + index], left, right)?
            .is_eq()
        {
            return Ok(false);
        }
    }
    Ok(true)
}

/// Keeps the WITH TIES boundary row in the reader's fixed buffer.
fn keep_boundary(boundary: &mut [u8], row: &[u8]) -> Result<(), SqlError> {
    boundary
        .get_mut(..row.len())
        .ok_or_else(|| {
            sql_err!(
                sqlstate::PROGRAM_LIMIT_EXCEEDED,
                "query row exceeds the external sort payload limit"
            )
        })?
        .copy_from_slice(row);
    Ok(())
}

fn decode_source_rowids(
    row: &[u8],
    identities_at: usize,
    identity_count: usize,
    output: &mut [Option<u64>],
) -> Result<(), SqlError> {
    output.fill(None);
    for (table, identity) in output.iter_mut().enumerate().take(identity_count) {
        *identity = match crate::sql::exec::decode_projected_pub(row, identities_at + table) {
            Datum::Int8(rowid) if rowid >= 0 => Some(rowid as u64),
            Datum::Null => None,
            _ => {
                return Err(sql_err!(
                    sqlstate::INTERNAL_ERROR,
                    "invalid row identity in external query run"
                ));
            }
        };
    }
    Ok(())
}

#[expect(clippy::too_many_arguments, reason = "external lock-pass plumbing")]
fn prelock_external_run<'a>(
    storage: &'a Storage,
    reader: &mut crate::sql::external::ExternalRunReader,
    run: crate::sql::external::ExternalRun,
    txid: u32,
    statement: &Select<'_>,
    scope: &QueryScope<'_>,
    plan: &MaterializationPlan<'_>,
    recheck: &mut LockedRowRecheck<'_, 'a>,
    limit: u64,
    offset: u64,
    sort: instrument::Node,
    arena: &'a Arena,
) -> Result<(), SqlError> {
    let window = offset.saturating_add(limit);
    let mut logical_index = 0u64;
    let mut boundary_len = 0usize;
    let source_rowids = arena
        .alloc_slice_with(scope.n, |_| None)
        .map_err(|_| arena_full())?;
    storage
        .with_block_store(|blocks| reader.start(blocks, run))
        .expect("spill-attached block store")?;
    loop {
        let keep_scanning = {
            let Some(context) = reader.context() else {
                break;
            };
            let crate::sql::external::ExternalRunContext {
                row,
                previous,
                boundary,
                ..
            } = context;
            let duplicate = if let Some(prior) = previous {
                if statement.distinct && plan.n_on == 0 {
                    crate::sql::exec::compare_projected_prefix(
                        storage, txid, row, prior, plan.width,
                    )?
                    .is_eq()
                } else if plan.n_on > 0 {
                    distinct_on_keys_equal(
                        storage,
                        txid,
                        &plan.key_collations[..plan.n_keys],
                        row,
                        prior,
                        plan.width,
                        plan.n_order,
                        plan.n_on,
                    )?
                } else {
                    false
                }
            } else {
                false
            };
            if duplicate {
                true
            } else {
                let past_window = logical_index >= window;
                if past_window && !(statement.with_ties && limit > 0) {
                    false
                } else {
                    // The row locks read this row from the sort.
                    sort.row();
                    decode_source_rowids(
                        row,
                        plan.identities_at,
                        plan.n_identities,
                        source_rowids,
                    )?;
                    match recheck
                        .lock(storage, txid, row, source_rowids, arena)?
                        .row(row)
                    {
                        None => true,
                        Some(row) => {
                            let in_ties = past_window
                                && boundary_len > 0
                                && order_keys_equal_collated(
                                    storage,
                                    txid,
                                    &plan.key_collations[..plan.n_order],
                                    &boundary[..boundary_len],
                                    row,
                                    plan.width,
                                    plan.n_order,
                                )?;
                            let lookahead = past_window && !in_ties;
                            if statement.with_ties && limit > 0 && logical_index + 1 == window {
                                keep_boundary(boundary, row)?;
                                boundary_len = row.len();
                            }
                            logical_index += 1;
                            !lookahead
                        }
                    }
                }
            }
        };
        if !keep_scanning {
            break;
        }
        storage
            .with_block_store(|blocks| reader.advance(blocks))
            .expect("spill-attached block store")?;
    }
    Ok(())
}

#[expect(clippy::too_many_arguments, reason = "query pipeline plumbing")]
pub(crate) fn external_materialized_into<'a>(
    storage: &'a Storage,
    scope: &QueryScope<'a>,
    from: &'a FromClause<'a>,
    txid: u32,
    statement: &'a Select<'a>,
    arena: &'a Arena,
    params: &[Datum<'a>],
    hooks: &EvalHooks<'_, 'a>,
    correlated: &'a [&'a Expr<'a>],
    base: &SubqueryValues<'a, 'a>,
    outer: Option<&dyn ColumnLookup<'a>>,
    limit: u64,
    offset: u64,
    emit: &mut ExternalRowEmitter<'_>,
) -> Result<u64, SqlError> {
    let plan = prepare_materialization(storage, txid, statement, scope, correlated, outer, arena)?;
    let pax_columns = materialization_pax_columns(statement, scope, from, &plan);
    let source_rowids = arena
        .alloc_slice_with(scope.n, |_| None)
        .map_err(|_| arena_full())?;
    let ordered_candidates = if !statement.distinct
        && statement.distinct_on.is_empty()
        && plan.n_order > 0
        && plan.n_keys == plan.n_order
        && correlated.is_empty()
        && !plan.has_srf
        && scope.n == 1
        && from.joins.is_empty()
        && from.base.sample.is_none()
    {
        match super::scan::ordered_index_access_plan(
            storage,
            scope,
            txid,
            from.base.inheritance == crate::sql::ast::RelationInheritance::Descendants,
            plan.where_in_scan,
            statement.items,
            statement.order_by,
            arena,
        )? {
            Some(ordered) => super::scan::ordered_indexed_candidates(
                storage,
                scope,
                txid,
                ordered,
                (statement.limit.is_some()
                    && limit != u64::MAX
                    && !statement.with_ties
                    && statement.locking.is_empty()
                    && plan.where_in_scan.is_none())
                .then_some(offset.saturating_add(limit).min(usize::MAX as u64) as usize),
                arena,
                params,
                hooks,
            )?,
            None => None,
        }
    } else {
        None
    };
    if !statement.with_ties
        && statement.locking.is_empty()
        && let Some(indexed) = ordered_candidates.as_ref()
    {
        if limit == 0 {
            return Ok(0);
        }
        let window = offset.saturating_add(limit);
        let mut logical_index = 0u64;
        let mut emitted = 0u64;
        let mut keep_emitting = true;
        scan_source_recycling_with_indexed_candidates(
            storage,
            scope,
            from,
            txid,
            plan.where_in_scan,
            arena,
            params,
            hooks,
            outer,
            pax_columns,
            indexed,
            &mut |row| {
                source_rowids.fill(None);
                source_rowids[..row.rowids.len()].copy_from_slice(row.rowids);
                for_each_materialized_projection(
                    storage,
                    scope,
                    statement,
                    row,
                    txid,
                    arena,
                    params,
                    hooks,
                    base,
                    plan.where_correlated,
                    &plan.order_exprs,
                    plan.n_keys,
                    None,
                    false,
                    outer,
                    &mut |_row, projected, _keys| {
                        if logical_index >= offset {
                            keep_emitting = emit(projected, source_rowids)?;
                            emitted += 1;
                        }
                        logical_index += 1;
                        Ok(())
                    },
                )?;
                Ok(keep_emitting && logical_index < window)
            },
        )?;
        return Ok(emitted);
    }
    let stages = materialized_stages(limit != u64::MAX);
    let _stages = stages.start();
    let below_limit = stages.below_limit();
    // A sort under a DISTINCT returns every row it sorted; one under row
    // locks returns the rows the lock pass reads.
    let sorted_rows = if stages.unique.measured() {
        stages.sort
    } else {
        instrument::Node::NONE
    };
    let mut sorter = storage.external_sorter()?;
    sorter.reset();
    let mut compare = |left: &[u8], right: &[u8]| {
        compare_materialized_rows(
            storage,
            txid,
            &plan.key_collations[..plan.n_keys],
            statement,
            plan.width,
            plan.n_order,
            plan.n_on,
            statement.distinct && plan.n_on == 0,
            left,
            right,
        )
    };
    let mut push = |row: &JoinRow<'_, 'a, '_>| {
        let mark = arena.mark();
        let result = for_each_materialized_projection(
            storage,
            scope,
            statement,
            row,
            txid,
            arena,
            params,
            hooks,
            base,
            plan.where_correlated,
            &plan.order_exprs,
            plan.n_keys,
            if plan.any_postponed {
                Some(&plan.postponed)
            } else {
                None
            },
            plan.has_srf,
            outer,
            &mut |row, projected, keys| {
                storage
                    .with_block_store(|blocks| {
                        sorter.push_projected_by(
                            blocks,
                            plan.width + plan.n_keys + plan.n_raw + plan.n_identities,
                            |index| {
                                materialized_value_at(
                                    scope,
                                    row,
                                    projected,
                                    keys,
                                    plan.width,
                                    plan.n_keys,
                                    plan.identities_at,
                                    plan.raw_columns.as_ref(),
                                    index,
                                )
                            },
                            &mut compare,
                        )
                    })
                    .expect("spill-attached block store")
            },
        );
        // SAFETY: every value allocated above `mark` was encoded into the
        // external run before the callback returned; none escapes.
        unsafe { arena.rewind_to(mark) };
        result?;
        Ok(true)
    };
    if let Some(indexed) = ordered_candidates.as_ref() {
        scan_source_recycling_with_indexed_candidates(
            storage,
            scope,
            from,
            txid,
            plan.where_in_scan,
            arena,
            params,
            hooks,
            outer,
            pax_columns,
            indexed,
            &mut push,
        )?;
    } else {
        scan_source_recycling_with_pax_columns(
            storage,
            scope,
            from,
            txid,
            plan.where_in_scan,
            arena,
            params,
            hooks,
            outer,
            pax_columns,
            &mut push,
        )?;
    }
    // A limit directly over the sort bounds it, as in the in-memory path.
    if statement.limit.is_some()
        && limit != u64::MAX
        && !statement.with_ties
        && statement.locking.is_empty()
        && !statement.distinct
        && plan.n_on == 0
        && plan.n_order > 0
    {
        sorter.keep_least(
            offset.saturating_add(limit).min(usize::MAX as u64) as usize,
            &mut compare,
        )?;
    }
    let run = storage
        .with_block_store(|blocks| sorter.finish(blocks, &mut compare))
        .expect("spill-attached block store")?;
    if plan.n_keys != 0
        && let Some(space) = sorter.space()
    {
        stages.sort.sorted(space);
    }
    let Some(run) = run else {
        return Ok(0);
    };
    drop(sorter);
    let mut reader = storage.external_run_reader()?;

    let deferred = plan.any_postponed.then_some(PostponedProjection {
        postponed: plan.postponed,
        raw_at: plan.width + plan.n_keys,
    });
    let window = offset.saturating_add(limit);
    let mut logical_index = 0u64;
    let mut emitted = 0u64;
    let mut boundary_len = 0usize;
    let mut recheck = if statement.locking.is_empty() {
        None
    } else {
        Some(LockedRowRecheck::new(
            storage, txid, statement, scope, from, correlated, base, params, hooks, outer, arena,
        )?)
    };
    if let Some(recheck) = recheck.as_mut() {
        prelock_external_run(
            storage,
            &mut reader,
            run,
            txid,
            statement,
            scope,
            &plan,
            recheck,
            limit,
            offset,
            stages.sort,
            arena,
        )?;
    }
    storage
        .with_block_store(|blocks| reader.start(blocks, run))
        .expect("spill-attached block store")?;
    loop {
        let mut staged_len = 0usize;
        source_rowids.fill(None);
        let keep_scanning = {
            let Some(context) = reader.context() else {
                break;
            };
            sorted_rows.row();
            let crate::sql::external::ExternalRunContext {
                row,
                previous,
                boundary,
                output: staged,
            } = context;
            let duplicate = if let Some(prior) = previous {
                if statement.distinct && plan.n_on == 0 {
                    crate::sql::exec::compare_projected_prefix(
                        storage, txid, row, prior, plan.width,
                    )?
                    .is_eq()
                } else if plan.n_on > 0 {
                    distinct_on_keys_equal(
                        storage,
                        txid,
                        &plan.key_collations[..plan.n_keys],
                        row,
                        prior,
                        plan.width,
                        plan.n_order,
                        plan.n_on,
                    )?
                } else {
                    false
                }
            } else {
                false
            };
            if duplicate {
                true
            } else {
                let past_window = logical_index >= window;
                if past_window && !(statement.with_ties && limit > 0) {
                    false
                } else {
                    decode_source_rowids(
                        row,
                        plan.identities_at,
                        plan.n_identities,
                        source_rowids,
                    )?;
                    let locked = match recheck.as_mut() {
                        Some(recheck) => recheck
                            .lock(storage, txid, row, source_rowids, arena)?
                            .row(row),
                        None => Some(row),
                    };
                    if let Some(row) = locked {
                        let in_ties = past_window
                            && boundary_len > 0
                            && order_keys_equal_collated(
                                storage,
                                txid,
                                &plan.key_collations[..plan.n_order],
                                &boundary[..boundary_len],
                                row,
                                plan.width,
                                plan.n_order,
                            )?;
                        let lookahead = past_window && !in_ties;
                        below_limit.row();
                        if logical_index >= offset && !lookahead {
                            stages.limit.row();
                        }
                        let mark = arena.mark();
                        let encoded = if deferred.is_some() {
                            (|| {
                                let encoded =
                                    arena.alloc_slice_copy(row).map_err(|_| arena_full())?;
                                let mut output = [Datum::Null; MAX_PROJ];
                                finalize_projected_row(
                                    encoded,
                                    plan.width,
                                    deferred.as_ref(),
                                    statement,
                                    scope,
                                    storage,
                                    txid,
                                    arena,
                                    params,
                                    hooks,
                                    outer,
                                    &mut output,
                                )?;
                                if logical_index >= offset && !lookahead {
                                    crate::sql::exec::encode_projected_into(
                                        &output[..plan.width],
                                        staged,
                                    )
                                } else {
                                    Ok(0)
                                }
                            })()
                        } else {
                            (|| {
                                let mut output = [Datum::Null; MAX_PROJ];
                                for (column, value) in
                                    output.iter_mut().enumerate().take(plan.width)
                                {
                                    *value = crate::sql::exec::decode_projected_col_record(
                                        row, column, arena,
                                    )?;
                                }
                                if logical_index >= offset && !lookahead {
                                    crate::sql::exec::encode_projected_into(
                                        &output[..plan.width],
                                        staged,
                                    )
                                } else {
                                    Ok(0)
                                }
                            })()
                        };
                        let encoded = match encoded {
                            Ok(encoded) => encoded,
                            Err(error) => {
                                // SAFETY: failed row evaluation cannot publish values.
                                unsafe { arena.rewind_to(mark) };
                                return Err(error);
                            }
                        };
                        if statement.with_ties && limit > 0 && logical_index + 1 == window {
                            keep_boundary(boundary, row)?;
                            boundary_len = row.len();
                        }
                        logical_index += 1;
                        // SAFETY: every finalized value was copied into reader-
                        // owned staging. The consumer runs only after this rewind,
                        // so any arena allocation it retains is not discarded.
                        unsafe { arena.rewind_to(mark) };
                        staged_len = encoded;
                        !lookahead
                    } else {
                        // SKIP LOCKED removes this tuple below Limit, so it
                        // consumes neither OFFSET nor LIMIT.
                        true
                    }
                }
            }
        };
        if !keep_scanning {
            break;
        }
        if staged_len > 0 {
            let encoded = reader.output(staged_len);
            // A record column must arrive structurally: decoding it as its
            // rendered text would break row-valued consumers (a `ROW(...) IN
            // (subquery)` probe compares records, not text). The field
            // allocations persist for the emitter exactly as any arena
            // allocation it makes itself.
            let mut output = [Datum::Null; MAX_PROJ];
            for (column, value) in output.iter_mut().enumerate().take(plan.width) {
                *value = crate::sql::exec::decode_projected_col_record(encoded, column, arena)?;
            }
            if !emit(&output[..plan.width], source_rowids)? {
                return Ok(emitted);
            }
            emitted += 1;
        }
        storage
            .with_block_store(|blocks| reader.advance(blocks))
            .expect("spill-attached block store")?;
    }
    Ok(emitted)
}

#[expect(clippy::too_many_arguments, reason = "query pipeline plumbing")]
fn external_materialized_select<'a>(
    storage: &'a Storage,
    scope: &QueryScope<'a>,
    from: &'a FromClause<'a>,
    txid: u32,
    statement: &'a Select<'a>,
    arena: &'a Arena,
    params: &[Datum<'a>],
    hooks: &EvalHooks<'_, 'a>,
    correlated: &'a [&'a Expr<'a>],
    base: &SubqueryValues<'a, 'a>,
    limit: u64,
    offset: u64,
    responder: &mut Responder,
) -> Outcome {
    let mut wire_full = false;
    let emitted = match external_materialized_into(
        storage,
        scope,
        from,
        txid,
        statement,
        arena,
        params,
        hooks,
        correlated,
        base,
        None,
        limit,
        offset,
        &mut |values, _rowids| match super::emit_data_row(storage, txid, arena, responder, values) {
            Ok(Ok(())) => Ok(true),
            Ok(Err(error)) => Err(error),
            Err(_) => {
                wire_full = true;
                Ok(false)
            }
        },
    ) {
        Ok(emitted) => emitted,
        Err(error) => return sql_fail(error),
    };
    if wire_full {
        return Err(crate::pg::wire::WireFull);
    }
    let tag = stack_format!(48, "SELECT {}", emitted);
    responder.command_complete(tag.as_str())?;
    sql_ok()
}

/// Extends an exclusive limit window to include every following row that ties
/// with `rows[window - 1]` on the ORDER BY keys (`FETCH FIRST ... WITH TIES`).
/// A `window` at or past the end (or no ORDER BY) is returned clamped.
pub(crate) fn extend_ties(
    storage: &Storage,
    txid: u32,
    collations: &[Collation],
    rows: &[&[u8]],
    width: usize,
    n_order: usize,
    window: usize,
) -> Result<usize, SqlError> {
    if n_order == 0 || window == 0 || window >= rows.len() {
        return Ok(window.min(rows.len()));
    }
    let boundary = rows[window - 1];
    let mut end = window;
    while end < rows.len()
        && order_keys_equal_collated(
            storage, txid, collations, boundary, rows[end], width, n_order,
        )?
    {
        end += 1;
    }
    Ok(end)
}

/// The stages of a materialized query that `EXPLAIN ANALYZE` counts. A
/// `LIMIT` stops PostgreSQL's `Unique` early, after fewer sorted rows than
/// the deduplication before the sort reads, so that sort is not counted.
pub(crate) fn materialized_stages(limited: bool) -> instrument::Stages {
    let mut stages = instrument::Stages::current();
    if stages.unique.measured() && limited {
        stages.sort = instrument::Node::NONE;
    }
    stages
}

/// DISTINCT / ORDER BY execution to the wire: materialize the rows, then page
/// with LIMIT/OFFSET and emit.
#[expect(clippy::too_many_arguments, reason = "query pipeline plumbing")]
pub(crate) fn materialized_select<'a>(
    storage: &'a Storage,
    scope: &QueryScope<'a>,
    from: &'a FromClause<'a>,
    txid: u32,
    statement: &'a Select<'a>,
    arena: &'a Arena,
    params: &[Datum<'a>],
    hooks: &EvalHooks<'_, 'a>,
    correlated: &'a [&'a Expr<'a>],
    base: &SubqueryValues<'a, 'a>,
    limit: u64,
    offset: u64,
    responder: &mut Responder,
) -> Outcome {
    if storage.spill_attached() {
        return external_materialized_select(
            storage, scope, from, txid, statement, arena, params, hooks, correlated, base, limit,
            offset, responder,
        );
    }
    let stages = materialized_stages(limit != u64::MAX);
    let _stages = stages.start();
    let (rows, width, deferred, identities_at) = match materialized_rows(
        storage,
        scope,
        from,
        txid,
        statement,
        arena,
        params,
        hooks,
        correlated,
        base,
        None,
        (statement.limit.is_some()
            && limit != u64::MAX
            && !statement.with_ties
            && statement.locking.is_empty())
        .then_some(offset.saturating_add(limit).min(usize::MAX as u64) as usize),
        &stages,
    ) {
        Ok(x) => x,
        Err(e) => return sql_fail(e),
    };
    let mut wire_full = false;
    let emitted = match materialized_rows_into(
        storage,
        scope,
        txid,
        statement,
        from,
        correlated,
        base,
        rows,
        width,
        deferred.as_ref(),
        identities_at,
        arena,
        params,
        hooks,
        None,
        limit,
        offset,
        &stages,
        &mut |values| match super::emit_data_row(storage, txid, arena, responder, values) {
            Ok(Ok(())) => Ok(true),
            Ok(Err(error)) => Err(error),
            Err(_) => {
                wire_full = true;
                Ok(false)
            }
        },
    ) {
        Ok(emitted) => emitted,
        Err(error) => return sql_fail(error),
    };
    if wire_full {
        return Err(crate::pg::wire::WireFull);
    }
    let tag = stack_format!(48, "SELECT {}", emitted);
    responder.command_complete(tag.as_str())?;
    sql_ok()
}

#[expect(
    clippy::too_many_arguments,
    reason = "materialized result and locking context"
)]
pub(crate) fn materialized_rows_into<'a>(
    storage: &'a Storage,
    scope: &QueryScope<'a>,
    txid: u32,
    statement: &'a Select<'a>,
    from: &'a FromClause<'a>,
    correlated: &'a [&'a Expr<'a>],
    base: &SubqueryValues<'a, 'a>,
    rows: &'a [&'a [u8]],
    width: usize,
    deferred: Option<&PostponedProjection>,
    identities_at: usize,
    arena: &'a Arena,
    params: &[Datum<'a>],
    hooks: &EvalHooks<'_, 'a>,
    outer: Option<&dyn ColumnLookup<'a>>,
    limit: u64,
    offset: u64,
    stages: &instrument::Stages,
    emit: &mut dyn FnMut(&[Datum<'a>]) -> Result<bool, SqlError>,
) -> Result<u64, SqlError> {
    let mut order_collations = [Collation::None; MAX_PROJ];
    for (index, order) in statement.order_by.iter().enumerate() {
        let expression = resolve_order_target(
            order.expression,
            statement.items,
            scope,
            arena,
            storage,
            txid,
        )?;
        order_collations[index] = scope.expression_collation(storage, arena, expression)?;
    }
    let mut emitted = 0u64;
    if !statement.locking.is_empty() {
        let rowids = arena
            .alloc_slice_with(scope.n, |_| None)
            .map_err(|_| arena_full())?;
        // Acquire returned rows and any WITH TIES lookahead before the first
        // DataRow. A later conflict can therefore park and retry the statement
        // without having leaked a partial result to a flushing responder.
        let mut recheck = LockedRowRecheck::new(
            storage, txid, statement, scope, from, correlated, base, params, hooks, outer, arena,
        )?;
        let tied = |boundary: Option<&[u8]>, row: &[u8]| match boundary {
            Some(boundary) if statement.with_ties => order_keys_equal_collated(
                storage,
                txid,
                &order_collations[..statement.order_by.len()],
                boundary,
                row,
                width,
                statement.order_by.len(),
            ),
            _ => Ok(false),
        };
        let mut lock = |row: &'a [u8], rowids: &mut [Option<u64>]| {
            rowids.fill(None);
            for (table, identity) in rowids.iter_mut().enumerate().take(scope.n) {
                *identity = match crate::sql::exec::decode_projected_pub(row, identities_at + table)
                {
                    Datum::Int8(rowid) if rowid >= 0 => Some(rowid as u64),
                    Datum::Null => None,
                    _ => {
                        return Err(sql_err!(
                            sqlstate::INTERNAL_ERROR,
                            "invalid row identity in materialized query"
                        ));
                    }
                };
            }
            Ok(recheck.lock(storage, txid, row, rowids, arena)?.row(row))
        };
        // The Limit node reads one row past a WITH TIES window, which the
        // LockRows node below it locks. The rows the preflight reads and
        // locks are the ones PostgreSQL's nodes return.
        let below_locks = [
            stages.unique,
            stages.sort,
            stages.project_set,
            stages.window,
        ]
        .into_iter()
        .find(|node| node.measured())
        .unwrap_or(instrument::Node::NONE);
        let mut preflight_accepted = 0u64;
        let mut preflight_emitted = 0u64;
        let mut preflight_boundary: Option<&[u8]> = None;
        for &row in rows {
            if preflight_emitted >= limit && (!statement.with_ties || limit == 0) {
                break;
            }
            below_locks.row();
            let Some(row) = lock(row, rowids)? else {
                continue;
            };
            stages.lock_rows.row();
            if preflight_emitted >= limit && !tied(preflight_boundary, row)? {
                break;
            }
            if preflight_accepted >= offset {
                preflight_emitted += 1;
                stages.limit.row();
                if preflight_emitted == limit {
                    preflight_boundary = Some(row);
                }
            }
            preflight_accepted += 1;
        }

        let mut accepted = 0u64;
        let mut boundary: Option<&[u8]> = None;
        for &row in rows {
            if emitted >= limit && (!statement.with_ties || limit == 0) {
                break;
            }
            let Some(row) = lock(row, rowids)? else {
                continue;
            };
            let lookahead = emitted >= limit && !tied(boundary, row)?;
            let mut out = [Datum::Null; MAX_PROJ];
            finalize_projected_row(
                row, width, deferred, statement, scope, storage, txid, arena, params, hooks, outer,
                &mut out,
            )?;
            if lookahead {
                break;
            }
            if accepted < offset {
                accepted += 1;
                continue;
            }
            let keep_emitting = emit(&out[..width])?;
            accepted += 1;
            emitted += 1;
            if !keep_emitting {
                return Ok(emitted);
            }
            if emitted == limit {
                boundary = Some(row);
            }
        }
        return Ok(emitted);
    }
    // OFFSET rows flow through PostgreSQL's projection before Limit discards
    // them, so deferred items are evaluated for them too (their errors
    // surface); only rows past the offset are emitted.
    let mut window = offset.saturating_add(limit).min(usize::MAX as u64) as usize;
    // FETCH FIRST ... WITH TIES: keep rows past the limit that tie with the
    // last one on the ORDER BY keys (which ride as hidden columns after
    // `width`). Only meaningful when a row was actually emitted (`limit > 0`).
    if statement.with_ties && limit > 0 {
        window = extend_ties(
            storage,
            txid,
            &order_collations[..statement.order_by.len()],
            rows,
            width,
            statement.order_by.len(),
            window,
        )?;
    }
    let projection_window = if statement.with_ties && limit > 0 && window < rows.len() {
        window + 1
    } else {
        window
    };
    let below_limit = stages.below_limit();
    for (index, row) in rows.iter().take(projection_window).enumerate() {
        let mut out = [Datum::Null; MAX_PROJ];
        finalize_projected_row(
            row, width, deferred, statement, scope, storage, txid, arena, params, hooks, outer,
            &mut out,
        )?;
        below_limit.row();
        if (index as u64) >= offset && index < window {
            stages.limit.row();
            let keep_emitting = emit(&out[..width])?;
            emitted += 1;
            if !keep_emitting {
                break;
            }
        }
    }
    Ok(emitted)
}
