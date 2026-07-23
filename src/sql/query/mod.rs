//! SELECT execution: one pipeline for single tables and joins.
//!
//! Shape: resolve the FROM clause into a scope → enumerate source rows
//! (nested-loop joins, LEFT emitting a null row) → WHERE → then either
//! stream straight to the wire, or materialize projected rows as tagged
//! byte strings in the statement arena for GROUP BY / DISTINCT / ORDER BY.
//! ORDER BY keys ride along as hidden columns after the visible ones, so
//! arbitrary key expressions order both plain and joined queries.
//!
//! Subqueries are uncorrelated and pre-evaluated once per statement; their
//! results are injected into evaluation by node identity (EvalHooks).

use crate::mem::arena::Arena;
use crate::pg::respond::Responder;
use crate::pg::wire::WireFull;
use crate::sql_err;
use crate::stack_format;
use crate::storage::{ColumnMeta, Storage};

use super::ast::{
    Expr, FrameBound, FromClause, OrderBy, Select, SelectItem, TableRef,
    WindowFrame,
};
use super::eval::{
    compare_datums, eval_full, sqlstate, ColumnLookup, EvalHooks, SqlError, SubqueryValues,
};
use super::exec::{describe_items, MAX_PROJ};
use super::types::{ColDesc, ColType, Datum};

mod setops;
pub use setops::set_query;
use setops::materialize_set_body;

mod materialize;
use materialize::{
    finalize_projected_row, materialized_rows, materialized_select, visible_prefix, ScopeSchema,
};

mod scan;
pub use scan::JoinRow;
use scan::{scan_source, Chained};

mod scope;
pub use scope::{MergedColumn, QueryScope, ResolvedColumn, MAX_MERGED_COLUMNS};

mod cte;
pub use cte::{describe_set_query, expand_ctes, expand_ctes_exec};
use cte::expand_set_tree_exec;

mod aggregate;
use aggregate::{fold_aggregates, AggState};

mod srf;
use srf::{find_srf, srf_count, srf_max_count, synth_derived_def, table_func_def, table_func_rows};

mod group;
use group::{grouped_rows, grouped_select};

mod plan;
use plan::{join_order, reorder_qual, simplify_qual, where_passes, postpone_cost};

mod subquery;
pub use subquery::{prepare_subqueries, subquery_hooks};
use subquery::{merge_correlated, prepare_outer_subqueries, subquery_witness, walk_children};

mod window;
use window::{
    cmp_key_rows, dedup_window_rows, project_window_rows, rewrite_grouped_windows, window_select,
};

pub const MAX_JOIN_TABLES: usize = 9; // base + 8 joins
const MAX_AGGS: usize = 16;

use core::cell::Cell;
std::thread_local! {
    /// Wall-clock deadline (micros since 2000-01-01) for the running statement;
    /// 0 means no `statement_timeout` is armed. Single-threaded per connection.
    static DEADLINE: Cell<i64> = const { Cell::new(0) };
    /// Amortizes the clock read in [`check_timeout`] to roughly 1 in 1024 calls.
    static TICK: Cell<u32> = const { Cell::new(0) };
}

/// Arms `statement_timeout` for the current statement (`timeout_ms == 0` clears
/// it). Call [`disarm_timeout`] when the statement completes.
pub fn arm_timeout(timeout_ms: u64) {
    let dl = if timeout_ms == 0 {
        0
    } else {
        super::datetime::now_micros().saturating_add(timeout_ms as i64 * 1000)
    };
    DEADLINE.with(|d| d.set(dl));
}

/// Clears any armed statement deadline.
pub fn disarm_timeout() {
    DEADLINE.with(|d| d.set(0));
}

/// Errors 57014 if the armed statement deadline has passed. Called at scan
/// boundaries; the clock is only read about once per 1024 calls.
pub fn check_timeout() -> Result<(), SqlError> {
    let dl = DEADLINE.with(|d| d.get());
    if dl == 0 {
        return Ok(());
    }
    let t = TICK.with(|c| {
        let v = c.get().wrapping_add(1);
        c.set(v);
        v
    });
    if !t.is_multiple_of(1024) {
        return Ok(());
    }
    if super::datetime::now_micros() >= dl {
        return Err(sql_err!("57014", "canceling statement due to statement timeout"));
    }
    Ok(())
}
pub(super) const MAX_WINDOWS: usize = 16;
/// Maximum ORDER BY / PARTITION BY keys in one window clause.
const MAX_WIN_KEYS: usize = 8;
const MAX_SUBQUERIES: usize = 8;
const SUBQUERY_DEPTH: u32 = 4;

type Outcome = Result<Result<(), SqlError>, WireFull>;

/// Bridges `EvalHooks`' abstract `CatalogAccess` to the concrete `Storage`, so
/// `pg_get_indexdef` can reconstruct an index's definition during evaluation.
struct StorageCatalog<'s> {
    storage: &'s Storage,
}

impl super::eval::CatalogAccess for StorageCatalog<'_> {
    fn index_def<'a>(
        &self,
        oid: i32,
        col: usize,
        arena: &'a Arena,
    ) -> Result<Option<&'a str>, SqlError> {
        super::catalog::index_def_text(self.storage, oid, col, arena)
    }
    fn constraint_def<'a>(&self, oid: i32, arena: &'a Arena) -> Result<Option<&'a str>, SqlError> {
        super::catalog::constraint_def_text(self.storage, oid, arena)
    }
    fn relname<'a>(&self, oid: i32, arena: &'a Arena) -> Result<Option<&'a str>, SqlError> {
        super::catalog::relname_text(self.storage, oid, arena)
    }

    fn reloid(&self, name: &str) -> Option<i32> {
        super::catalog::reloid_of_name(self.storage, name)
    }
}

fn sql_ok() -> Outcome {
    Ok(Ok(()))
}

fn sql_fail(e: SqlError) -> Outcome {
    Ok(Err(e))
}


/// The aggregate hook data for a select's items: the aggregate-call node
/// addresses and their folded values.
type AggregateHookData<'a> = (&'a [*const Expr<'a>], &'a [Datum<'a>]);

/// FROM-less aggregation: PostgreSQL treats the missing FROM clause as a
/// single virtual row (zero rows when WHERE is false). Returns the aggregate
/// hook data for evaluating the select items, or None when the query yields
/// no output row at all (WHERE false under GROUP BY, or HAVING false).
pub(super) fn fromless_aggregate_hooks<'a, R: ColumnLookup<'a>>(
    statement: &'a Select<'a>,
    agg_nodes: &[(*const Expr<'a>, &'a Expr<'a>)],
    arena: &'a Arena,
    params: &[Datum<'a>],
    row: &R,
    hooks: &EvalHooks<'_, 'a>,
) -> Result<Option<AggregateHookData<'a>>, SqlError> {
    let pass = match statement.where_clause {
        Some(w) => where_passes(w, arena, params, row, hooks)?,
        None => true,
    };
    if !statement.group_by.is_empty() && !pass {
        // Zero input rows grouped: zero groups, zero output rows (a plain
        // aggregate still emits its one row over the empty input).
        return Ok(None);
    }
    let mut states = [AggState::default(); MAX_AGGS];
    for (i, (_, node)) in agg_nodes.iter().enumerate() {
        states[i].init(node)?;
    }
    if pass {
        for (i, (_, node)) in agg_nodes.iter().enumerate() {
            states[i].update(node, arena, params, row, hooks)?;
        }
    }
    let values = arena
        .alloc_slice_with(agg_nodes.len(), |_| Datum::Null)
        .map_err(|_| arena_full())?;
    for (i, state) in states[..agg_nodes.len()].iter_mut().enumerate() {
        values[i] = state.finish(arena)?;
    }
    let ptrs: &[*const Expr] = arena
        .alloc_slice_with(agg_nodes.len(), |i| agg_nodes[i].0)
        .map_err(|_| arena_full())?;
    if let Some(h) = statement.having {
        let agg_hooks = EvalHooks { aggs: Some((ptrs, values)), ..*hooks };
        let held = matches!(
            eval_full(h, arena, params, row, &agg_hooks)?,
            Datum::Bool(true)
        );
        if !held {
            return Ok(None);
        }
    }
    Ok(Some((ptrs, values)))
}

/// A scalar (or ARRAY) subquery's output type is known only from its
/// pre-evaluated datum — static inference cannot reach into storage, so the
/// describe pass types it UNKNOWN (rendered text). The same holds for a bare
/// `$n` parameter, whose type arrives with its bound value. Where a select
/// item is one of these, override the described column type from the value.
#[allow(clippy::too_many_arguments, reason = "query pipeline plumbing")]
fn patch_subquery_column_types<'a>(
    items: &'a [SelectItem<'a>],
    scope: Option<&QueryScope<'a>>,
    subs: &SubqueryValues,
    params: &[Datum],
    storage: &'a Storage,
    txid: u32,
    arena: &'a Arena,
    columns: &mut [ColDesc],
) {
    let mut slot = 0usize;
    for item in items {
        match item {
            SelectItem::Wildcard => slot += scope.map_or(0, |s| s.star_columns()),
            SelectItem::TableWildcard(q) => {
                slot += scope
                    .and_then(|s| s.table_index(q).ok().map(|t| {
                        s.defs[t].expect("resolved").n_columns
                    }))
                    .unwrap_or(0);
            }
            SelectItem::RecordStar(base) => {
                slot += scope.map_or(0, |s| record_star_width(base, s));
            }
            SelectItem::Expr { expression, .. } => {
                if slot < columns.len()
                    && matches!(**expression, Expr::Subquery(_) | Expr::ArraySubquery(_))
                {
                    let node: *const Expr = *expression;
                    match subs.scalars.iter().find(|(p, _, _)| core::ptr::eq(*p, node)) {
                        Some((_, v, w)) => {
                            let typed = if v.is_null() { w } else { v };
                            if !typed.is_null()
                                && let Some(ct) = super::exec::coltype_of_oid(typed.type_oid())
                            {
                                columns[slot] = ColDesc::of_type(columns[slot].name, ct);
                            }
                        }
                        // A correlated subquery has no pre-evaluated value;
                        // infer its column type from the inner select's item.
                        None => {
                            if let Expr::Subquery(sub) = &**expression
                                && let Some(SelectItem::Expr { expression: inner, .. }) =
                                    sub.items.first()
                            {
                                let inner_scope = sub.from.as_ref().and_then(|f| {
                                    QueryScope::resolve_schema(storage, f, txid, arena).ok()
                                });
                                let witness = subquery_witness(inner, inner_scope.as_ref());
                                if !witness.is_null()
                                    && let Some(ct) =
                                        super::exec::coltype_of_oid(witness.type_oid())
                                {
                                    columns[slot] = ColDesc::of_type(columns[slot].name, ct);
                                }
                            }
                        }
                    }
                }
                if slot < columns.len()
                    && let Expr::Param(n) = **expression
                    && let Some(v) = params.get(n as usize - 1)
                    && !v.is_null()
                    && columns[slot].type_oid == super::types::oid::TEXT
                    && let Some(ct) = super::exec::coltype_of_oid(v.type_oid())
                {
                    columns[slot] = ColDesc::of_type(columns[slot].name, ct);
                }
                slot += 1;
            }
        }
    }
}

/// PostgreSQL has no equality or ordering for `json` — two documents differing
/// only in whitespace or key order are the same value but not the same text, so
/// it declines rather than answer by a rule it does not hold to, and offers
/// canonicalized `jsonb` instead. The `=` operator already declines here; these
/// three sort and deduplicate by the projected encoding and so never consult
/// it, which is why each has to be checked where its keys are known rather than
/// where they are compared.
fn check_key_types<'a>(
    statement: &'a Select<'a>,
    scope: &QueryScope<'a>,
    arena: &'a Arena,
) -> Result<(), SqlError> {
    let undefined = |ordering: bool| {
        Err(sql_err!(
            sqlstate::UNDEFINED_FUNCTION,
            "could not identify an {} operator for type json",
            if ordering { "ordering" } else { "equality" }
        ))
    };
    let is_json =
        |e: &Expr<'a>| matches!(infer_scope_type(e, scope), Ok((super::types::oid::JSON, _)));
    for key in statement.group_by.iter().chain(statement.distinct_on) {
        if is_json(key) {
            return undefined(false);
        }
    }
    for order in statement.order_by {
        let target = resolve_order_target(order.expression, statement.items, scope, arena)?;
        if is_json(target) {
            return undefined(true);
        }
    }
    if statement.distinct {
        for item in statement.items {
            if let SelectItem::Expr { expression, .. } = item
                && is_json(expression)
            {
                return undefined(false);
            }
        }
    }
    Ok(())
}

/// `GROUP BY <n>` names the *n*th select-list column, exactly as `ORDER BY <n>`
/// does. Resolved once, against the scope so a star item expands the same way,
/// and the resolved expressions replace the ordinals — so grouping, HAVING,
/// grouping sets and the ungrouped-column check all see what the position
/// stood for rather than the literal integer. A bare integer only; `GROUP BY
/// 1+0` is a constant expression in PostgreSQL too, and errors as one.
fn resolve_group_ordinals<'a>(
    statement: &'a Select<'a>,
    scope: &QueryScope<'a>,
    arena: &'a Arena,
) -> Result<&'a Select<'a>, SqlError> {
    if !statement.group_by.iter().any(|g| matches!(g, Expr::Int(_))) {
        return Ok(statement);
    }
    // The parser bounds a GROUP BY list by the same limit it bounds any
    // expression list by, so a parsed statement always fits.
    let mut resolved = [&Expr::Null; super::parser::MAX_LIST];
    for (slot, g) in resolved.iter_mut().zip(statement.group_by) {
        *slot = match g {
            Expr::Int(_) => resolve_position_target(g, statement.items, scope, arena, "GROUP BY")?,
            _ => g,
        };
    }
    let group_by = arena
        .alloc_slice_copy(&resolved[..statement.group_by.len()])
        .map_err(|_| arena_full())?;
    let mut rewritten = *statement;
    rewritten.group_by = &*group_by;
    Ok(&*arena.alloc(rewritten).map_err(|_| arena_full())?)
}

/// ORDER BY `n` refers to the n-th *output* column: select-list stars count
/// one position per expanded column, as in PostgreSQL. A position inside a
/// star synthesizes the column reference; names and expressions delegate to
/// the select-list name-binding rules.
fn resolve_order_target<'a>(
    expression: &'a Expr<'a>,
    items: &'a [SelectItem<'a>],
    scope: &QueryScope<'a>,
    arena: &'a Arena,
) -> Result<&'a Expr<'a>, SqlError> {
    resolve_position_target(expression, items, scope, arena, "ORDER BY")
}

/// An `ORDER BY` / `GROUP BY` target: a bare integer is a 1-based position in
/// the select list (a star item expanding to its columns), anything else is the
/// expression itself, matched against the select list's aliases.
fn resolve_position_target<'a>(
    expression: &'a Expr<'a>,
    items: &'a [SelectItem<'a>],
    scope: &QueryScope<'a>,
    arena: &'a Arena,
    clause: &str,
) -> Result<&'a Expr<'a>, SqlError> {
    let Expr::Int(n) = expression else {
        return super::exec::resolve_order_expr_pub(expression, items);
    };
    let index = *n;
    let position_error =
        || sql_err!("42P10", "{} position {} is not in select list", clause, index);
    if index < 1 {
        return Err(position_error());
    }
    let column_ref = |qualifier: Option<&'a str>, name: &'a str| {
        Ok(&*arena.alloc(Expr::Column { qualifier, name }).map_err(|_| arena_full())?)
    };
    let mut remaining = index as usize - 1;
    for item in items {
        match item {
            SelectItem::Expr { expression, .. } => {
                if remaining == 0 {
                    return Ok(expression);
                }
                remaining -= 1;
            }
            SelectItem::Wildcard => {
                let width = scope.star_columns();
                if remaining < width {
                    return match scope.star_entry(remaining) {
                        ResolvedColumn::Table(t, c) => column_ref(
                            Some(scope.names[t]),
                            scope.defs[t].expect("resolved").columns()[c].name.as_str(),
                        ),
                        // Unqualified: resolves back to the merged column.
                        ResolvedColumn::Merged(m) => column_ref(None, scope.merged[m].name),
                    };
                }
                remaining -= width;
            }
            SelectItem::TableWildcard(q) => {
                let t = scope.table_index(q)?;
                let def = scope.defs[t].expect("resolved");
                if remaining < def.n_columns {
                    return column_ref(
                        Some(scope.names[t]),
                        def.columns()[remaining].name.as_str(),
                    );
                }
                remaining -= def.n_columns;
            }
            SelectItem::RecordStar(base) => {
                let width = record_star_width(base, scope);
                if remaining < width {
                    // A positional (ORDER BY/GROUP BY ordinal) reference into a
                    // `(record).*` expansion has no simple column to resolve to.
                    return Err(sql_err!(
                        "0A000",
                        "ORDER BY/GROUP BY position into a record expansion is not supported"
                    ));
                }
                remaining -= width;
            }
        }
    }
    Err(position_error())
}

/// The common type of a USING/NATURAL-merged column pair, per PostgreSQL's
/// `select_common_type` (the preferred type of the category wins). `None`
/// means the pair has no `=` operator — an error, as in PostgreSQL.
fn common_using_type(a: ColType, b: ColType) -> Option<ColType> {
    use ColType::*;
    if a == b {
        return Some(a);
    }
    let numeric_rank = |t: ColType| match t {
        Int2 => Some(0),
        Int4 => Some(1),
        Int8 => Some(2),
        Numeric => Some(3),
        Float4 => Some(4),
        Float8 => Some(5),
        _ => None,
    };
    if let (Some(ra), Some(rb)) = (numeric_rank(a), numeric_rank(b)) {
        return Some(if ra >= rb { a } else { b });
    }
    if matches!(a, Text | Varchar | Bpchar) && matches!(b, Text | Varchar | Bpchar) {
        return Some(Text);
    }
    let datetime_rank = |t: ColType| match t {
        Date => Some(0),
        Timestamp => Some(1),
        Timestamptz => Some(2),
        _ => None,
    };
    if let (Some(ra), Some(rb)) = (datetime_rank(a), datetime_rank(b)) {
        return Some(if ra >= rb { a } else { b });
    }
    if matches!(a, Bit { .. }) && matches!(b, Bit { .. }) {
        return Some(Bit { varying: true });
    }
    None
}


/// A view that PostgreSQL treats as auto-updatable: a single base table, no
/// aggregation/DISTINCT/GROUP BY/HAVING/LIMIT/joins, and every output column a
/// plain (un-aliased) base column. `where_clause` is the view's own filter, to
/// be AND-ed into any DML on the view; `columns` are the exposed base columns.
pub struct UpdatableView<'a> {
    pub base: &'a str,
    pub where_clause: Option<&'a Expr<'a>>,
    pub columns: &'a [&'a str],
}

/// If `name` is a view, resolve it for DML: `Ok(Some(..))` when auto-updatable,
/// `Err` (0A000) when it is a view but not auto-updatable, `Ok(None)` when it
/// is not a view at all (the DML then targets a table normally).
pub fn resolve_view_for_dml<'a>(
    storage: &Storage,
    name: &str,
    txid: u32,
    arena: &'a Arena,
) -> Result<Option<UpdatableView<'a>>, SqlError> {
    let Some(sql) = storage.find_view(name, txid) else {
        return Ok(None);
    };
    // Copy the definition into the arena so the parsed AST no longer borrows
    // storage (the caller then takes a mutable storage borrow to run the DML).
    let sql = arena.alloc_str(sql).map_err(|_| arena_full())?;
    let not_updatable = || {
        sql_err!(
            sqlstate::FEATURE_NOT_SUPPORTED,
            "cannot change view \"{}\": it is not auto-updatable",
            name
        )
    };
    let sel = super::parser::parse_view_select(sql, arena)?;
    if sel.distinct
        || !sel.group_by.is_empty()
        || sel.having.is_some()
        || sel.limit.is_some()
        || sel.offset.is_some()
    {
        return Err(not_updatable());
    }
    let Some(from) = &sel.from else {
        return Err(not_updatable());
    };
    if !from.joins.is_empty() || from.base.subquery.is_some() || from.base.schema.is_some() {
        return Err(not_updatable());
    }
    let base = from.base.table;
    let mut columns = [""; MAX_PROJ];
    let mut n = 0;
    for it in sel.items {
        match it {
            SelectItem::Wildcard => {
                let Some(ti) = storage.find_table(base) else {
                    return Err(not_updatable());
                };
                for c in storage.table(ti).def.columns() {
                    if n == MAX_PROJ {
                        return Err(not_updatable());
                    }
                    // Copy into the arena so it does not borrow storage.
                    columns[n] = arena.alloc_str(c.name.as_str()).map_err(|_| arena_full())?;
                    n += 1;
                }
            }
            // Only a plain, un-aliased base column keeps view and base names in
            // sync (so the view's/DML's WHERE resolve directly against the base).
            SelectItem::Expr { expression: Expr::Column { name: cn, .. }, alias } => {
                if alias.is_some_and(|a| a != *cn) {
                    return Err(not_updatable());
                }
                if n == MAX_PROJ {
                    return Err(not_updatable());
                }
                columns[n] = cn;
                n += 1;
            }
            _ => return Err(not_updatable()),
        }
    }
    let columns = arena.alloc_slice_copy(&columns[..n]).map_err(|_| arena_full())?;
    Ok(Some(UpdatableView { base, where_clause: sel.where_clause, columns }))
}

/// Combines a view's filter with a DML's WHERE (AND), for view DML rewriting.
pub fn and_where<'a>(
    view_where: Option<&'a Expr<'a>>,
    dml_where: Option<&'a Expr<'a>>,
    arena: &'a Arena,
) -> Result<Option<&'a Expr<'a>>, SqlError> {
    match (view_where, dml_where) {
        (None, w) | (w, None) => Ok(w),
        (Some(a), Some(b)) => {
            let e = Expr::Binary { operator: super::ast::BinaryOp::And, left: a, right: b };
            Ok(Some(&*arena.alloc(e).map_err(|_| arena_full())?))
        }
    }
}

/// Validates a view definition at CREATE VIEW time, as PostgreSQL does: the
/// SELECT must parse, its tables/views must exist, and its output columns must
/// resolve. Surfaces the same errors (42P01 / 42703) a query would.
pub fn validate_view<'a>(
    sql: &'a str,
    storage: &'a Storage,
    txid: u32,
    arena: &'a Arena,
) -> Result<(), SqlError> {
    let sel = super::parser::parse_view_select(sql, arena)?;
    let sel = expand_ctes(sel, storage, txid, arena)?;
    let mut columns = [ColDesc::new("", 0, 0); MAX_PROJ];
    match &sel.from {
        Some(from) => {
            // Committed catalog: a view's referents are validated as visible now.
            let scope = QueryScope::resolve_schema(storage, from, 0, arena)?;
            describe_scope_items(sel.items, &scope, &mut columns)?;
        }
        None => {
            describe_items(sel.items, None, &mut columns)?;
        }
    }
    Ok(())
}

/// Walks an expression tree collecting aggregate call nodes. A windowed call
/// (`sum(x) OVER (...)`) is not one: it is a window function, and counting it
/// here would send the query down the grouped executor instead of the window
/// one — so its arguments are walked into like any other expression.
pub(super) fn collect_aggs<'a>(
    expression: &'a Expr<'a>,
    out: &mut [(*const Expr<'a>, &'a Expr<'a>); MAX_AGGS],
    n: &mut usize,
) -> Result<(), SqlError> {
    if expression.is_aggregate_use() {
        if out[..*n].iter().any(|(p, _)| core::ptr::eq(*p, expression)) {
            return Ok(());
        }
        if *n == MAX_AGGS {
            return Err(sql_err!(sqlstate::PROGRAM_LIMIT_EXCEEDED, "too many aggregates in one query"));
        }
        out[*n] = (expression as *const _, expression);
        *n += 1;
        return Ok(()); // aggregate arguments evaluate per input row
    }
    walk_children(expression, &mut |child| collect_aggs(child, out, n))
}

/// Collects window-function call nodes (a `Call` with an `OVER` clause).
pub(super) fn collect_windows<'a>(
    expression: &'a Expr<'a>,
    out: &mut [&'a Expr<'a>; MAX_WINDOWS],
    n: &mut usize,
) -> Result<(), SqlError> {
    if let Expr::Call { over: Some(_), .. } = expression {
        if out[..*n].iter().any(|e| core::ptr::eq(*e, expression)) {
            return Ok(());
        }
        if *n == MAX_WINDOWS {
            return Err(sql_err!("54023", "too many window functions in one query"));
        }
        out[*n] = expression;
        *n += 1;
        // The arguments and PARTITION/ORDER expressions evaluate per input row;
        // a window function nested inside another is not supported and would be
        // found by the analysis pass, not here.
        return Ok(());
    }
    walk_children(expression, &mut |child| collect_windows(child, out, n))
}

/// Builds a `JoinRow` view over one flat materialized row (all scope columns
/// concatenated, table by table).
fn window_row<'r, 'a>(
    scope: &'r QueryScope<'a>,
    flat: &'r [Datum<'a>],
    offs: &[usize],
) -> JoinRow<'r, 'a, 'a> {
    let mut values: [Option<&[Datum]>; MAX_JOIN_TABLES] = [None; MAX_JOIN_TABLES];
    for (t, offset) in offs.iter().enumerate().take(scope.n) {
        let nc = scope.defs[t].expect("resolved").n_columns;
        values[t] = Some(&flat[*offset..*offset + nc]);
    }
    JoinRow { scope, values }
}

/// Whether two rows have equal tuples over `keys` (NULLs compare equal, as in
/// window PARTITION BY / peer grouping).
#[allow(clippy::too_many_arguments)]
fn keys_equal<'a>(
    keys: &[&'a Expr<'a>],
    scope: &QueryScope<'a>,
    rows: &[&'a [Datum<'a>]],
    offs: &[usize],
    a: usize,
    b: usize,
    arena: &'a Arena,
    params: &[Datum<'a>],
    hooks: &EvalHooks<'_, 'a>,
) -> Result<bool, SqlError> {
    for k in keys {
        let ra = window_row(scope, rows[a], offs);
        let va = eval_full(k, arena, params, &ra, hooks)?;
        let rb = window_row(scope, rows[b], offs);
        let vb = eval_full(k, arena, params, &rb, hooks)?;
        let eq = match (va.is_null(), vb.is_null()) {
            (true, true) => true,
            (true, false) | (false, true) => false,
            (false, false) => compare_datums(&va, &vb)?.is_eq(),
        };
        if !eq {
            return Ok(false);
        }
    }
    Ok(true)
}

/// Collects aggregate-call nodes for a grouped window query: aggregates
/// outside window functions plus those inside window arguments and keys
/// (`sum(sum(v)) OVER (...)`: the inner sum aggregates per group).
fn collect_grouped_aggs<'a>(
    e: &'a Expr<'a>,
    out: &mut [(*const Expr<'a>, &'a Expr<'a>); MAX_AGGS],
    n: &mut usize,
) -> Result<(), SqlError> {
    if let Expr::Call { over: Some(spec), args, filter, .. } = e {
        for a in *args {
            collect_grouped_aggs(a, out, n)?;
        }
        for pk in spec.partition_by {
            collect_grouped_aggs(pk, out, n)?;
        }
        for o in spec.order_by {
            collect_grouped_aggs(o.expression, out, n)?;
        }
        if let Some(frame) = &spec.frame {
            for bound in [&frame.start, &frame.end] {
                if let FrameBound::Preceding(x) | FrameBound::Following(x) = bound {
                    collect_grouped_aggs(x, out, n)?;
                }
            }
        }
        if let Some(f) = filter {
            collect_grouped_aggs(f, out, n)?;
        }
        return Ok(());
    }
    if e.is_aggregate() {
        return collect_aggs(e, out, n);
    }
    // GROUPING() reads the current grouping-set mask, so it must evaluate in
    // the inner grouped select, like an aggregate.
    if let Expr::Call { name: "grouping", over: None, .. } = e {
        if out[..*n].iter().any(|(p, _)| core::ptr::eq(*p, e)) {
            return Ok(());
        }
        if *n == MAX_AGGS {
            return Err(sql_err!(sqlstate::PROGRAM_LIMIT_EXCEEDED, "too many aggregates in one query"));
        }
        out[*n] = (e as *const _, e);
        *n += 1;
        return Ok(());
    }
    walk_children(e, &mut |child| collect_grouped_aggs(child, out, n))
}

/// Context for [`rewrite_grouped_expr`]: the inner derived table's exposed
/// columns for each grouping key and aggregate.
struct GroupedRewrite<'a, 's> {
    group_by: &'a [&'a Expr<'a>],
    group_names: &'a [&'a str],
    aggs: &'a [(*const Expr<'a>, &'a Expr<'a>)],
    agg_names: &'a [&'a str],
    /// The source scope, to distinguish an unknown column (42703) from a
    /// known-but-ungrouped one (42803), as PostgreSQL.
    scope: Option<&'s QueryScope<'a>>,
}

/// Rewrites an outer-select expression of a grouped window query: aggregate
/// nodes and grouping-key expressions become references to the inner derived
/// table's columns; window calls keep their shape with rewritten insides. A
/// leftover bare column is the PostgreSQL grouping error (42803).
fn rewrite_grouped_expr<'a>(
    e: &'a Expr<'a>,
    context: &GroupedRewrite<'a, '_>,
    arena: &'a Arena,
) -> Result<&'a Expr<'a>, SqlError> {
    if let Some(i) = context.aggs.iter().position(|(p, _)| core::ptr::eq(*p, e)) {
        return Ok(&*arena
            .alloc(Expr::Column { qualifier: None, name: context.agg_names[i] })
            .map_err(|_| arena_full())?);
    }
    if let Some(i) = context.group_by.iter().position(|g| **g == *e) {
        return Ok(&*arena
            .alloc(Expr::Column { qualifier: None, name: context.group_names[i] })
            .map_err(|_| arena_full())?);
    }
    let rewrite = |x: &'a Expr<'a>| rewrite_grouped_expr(x, context, arena);
    let alloc = |x: Expr<'a>| -> Result<&'a Expr<'a>, SqlError> {
        Ok(&*arena.alloc(x).map_err(|_| arena_full())?)
    };
    match e {
        Expr::WholeRow(t) => Err(sql_err!(
            "42803",
            "column \"{}.*\" must appear in the GROUP BY clause or be used in an aggregate function",
            t
        )),
        Expr::Null
        | Expr::Bool(_)
        | Expr::Int(_)
        | Expr::Float(_)
        | Expr::NumericLit(_)
        | Expr::Str(_)
        | Expr::BitLit(_)
        | Expr::Param(_)
        | Expr::DefaultMarker
        // Subqueries evaluate through their own hooks, not the group.
        | Expr::Subquery(_)
        | Expr::Exists(_)
        | Expr::ArraySubquery(_) => Ok(e),
        Expr::Column { qualifier, name } => {
            // An unknown column errors as such; a known one is ungrouped.
            if let Some(scope) = context.scope
                && scope.find_column(*qualifier, name).is_err()
            {
                return Err(sql_err!(
                    sqlstate::UNDEFINED_COLUMN,
                    "column \"{}\" does not exist",
                    name
                ));
            }
            Err(sql_err!(
                "42803",
                "column \"{}{}{}\" must appear in the GROUP BY clause or be used in an aggregate function",
                qualifier.unwrap_or(""),
                if qualifier.is_some() { "." } else { "" },
                name
            ))
        }
        Expr::Unary { operator, operand } => {
            alloc(Expr::Unary { operator: *operator, operand: rewrite(operand)? })
        }
        Expr::Binary { operator, left, right } => alloc(Expr::Binary {
            operator: *operator,
            left: rewrite(left)?,
            right: rewrite(right)?,
        }),
        Expr::Cast { operand, type_name, type_mod } => alloc(Expr::Cast {
            operand: rewrite(operand)?,
            type_name,
            type_mod: *type_mod,
        }),
        Expr::IsNull { operand, negated } => {
            alloc(Expr::IsNull { operand: rewrite(operand)?, negated: *negated })
        }
        Expr::InList { operand, list, negated } => {
            let mut items = [&Expr::Null as &'a Expr<'a>; super::parser::MAX_LIST];
            for (i, x) in list.iter().enumerate() {
                items[i] = rewrite(x)?;
            }
            let list = arena.alloc_slice_copy(&items[..list.len()]).map_err(|_| arena_full())?;
            alloc(Expr::InList { operand: rewrite(operand)?, list, negated: *negated })
        }
        Expr::Between { operand, low, high, negated } => alloc(Expr::Between {
            operand: rewrite(operand)?,
            low: rewrite(low)?,
            high: rewrite(high)?,
            negated: *negated,
        }),
        Expr::Like { operand, pattern, negated, case_insensitive, escape } => alloc(Expr::Like {
            operand: rewrite(operand)?,
            pattern: rewrite(pattern)?,
            negated: *negated,
            case_insensitive: *case_insensitive,
            escape: match escape {
                Some(e) => Some(rewrite(e)?),
                None => None,
            },
        }),
        Expr::Match { operand, pattern, negated, case_insensitive } => alloc(Expr::Match {
            operand: rewrite(operand)?,
            pattern: rewrite(pattern)?,
            negated: *negated,
            case_insensitive: *case_insensitive,
        }),
        Expr::Case { operand, whens, otherwise, synthetic } => {
            let operand = match operand {
                Some(o) => Some(rewrite(o)?),
                None => None,
            };
            let mut pairs = [(&Expr::Null as &'a Expr<'a>, &Expr::Null as &'a Expr<'a>);
                super::parser::MAX_LIST];
            for (i, (c, r)) in whens.iter().enumerate() {
                pairs[i] = (rewrite(c)?, rewrite(r)?);
            }
            let whens = arena.alloc_slice_copy(&pairs[..whens.len()]).map_err(|_| arena_full())?;
            let otherwise = match otherwise {
                Some(o) => Some(rewrite(o)?),
                None => None,
            };
            alloc(Expr::Case { operand, whens, otherwise, synthetic: *synthetic })
        }
        Expr::Call { name, args, star, distinct, order_by, over, filter } => {
            let mut rewritten = [&Expr::Null as &'a Expr<'a>; super::parser::MAX_LIST];
            for (i, a) in args.iter().enumerate() {
                rewritten[i] = rewrite(a)?;
            }
            let args = arena.alloc_slice_copy(&rewritten[..args.len()]).map_err(|_| arena_full())?;
            let over = match over {
                None => None,
                Some(spec) => {
                    let mut parts = [&Expr::Null as &'a Expr<'a>; super::parser::MAX_LIST];
                    for (i, pk) in spec.partition_by.iter().enumerate() {
                        parts[i] = rewrite(pk)?;
                    }
                    let partition_by = arena
                        .alloc_slice_copy(&parts[..spec.partition_by.len()])
                        .map_err(|_| arena_full())?;
                    let mut obs = [OrderBy {
                        expression: &Expr::Null,
                        descending: false,
                        nulls_first: false,
                    }; super::parser::MAX_LIST];
                    for (i, o) in spec.order_by.iter().enumerate() {
                        obs[i] = OrderBy { expression: rewrite(o.expression)?, ..*o };
                    }
                    let order_by = arena
                        .alloc_slice_copy(&obs[..spec.order_by.len()])
                        .map_err(|_| arena_full())?;
                    let frame = match &spec.frame {
                        None => None,
                        Some(f) => {
                            let bound = |b: &FrameBound<'a>| -> Result<FrameBound<'a>, SqlError> {
                                Ok(match b {
                                    FrameBound::Preceding(x) => FrameBound::Preceding(rewrite(x)?),
                                    FrameBound::Following(x) => FrameBound::Following(rewrite(x)?),
                                    other => *other,
                                })
                            };
                            Some(WindowFrame {
                                units: f.units,
                                start: bound(&f.start)?,
                                end: bound(&f.end)?,
                                exclusion: f.exclusion,
                            })
                        }
                    };
                    let spec = super::ast::WindowSpec { partition_by, order_by, frame };
                    Some(&*arena.alloc(spec).map_err(|_| arena_full())?)
                }
            };
            let filter = match filter {
                None => None,
                Some(f) => Some(rewrite(f)?),
            };
            alloc(Expr::Call {
                name,
                args,
                star: *star,
                distinct: *distinct,
                order_by,
                over,
                filter,
            })
        }
        Expr::InSubquery { operand, select, negated } => alloc(Expr::InSubquery {
            operand: rewrite(operand)?,
            select,
            negated: *negated,
        }),
        Expr::Array(items) => {
            let mut rewritten = [&Expr::Null as &'a Expr<'a>; super::parser::MAX_LIST];
            for (i, x) in items.iter().enumerate() {
                rewritten[i] = rewrite(x)?;
            }
            let items =
                arena.alloc_slice_copy(&rewritten[..items.len()]).map_err(|_| arena_full())?;
            alloc(Expr::Array(items))
        }
        Expr::Subscript { base, index } => {
            alloc(Expr::Subscript { base: rewrite(base)?, index: rewrite(index)? })
        }
        Expr::Field { base, field } => alloc(Expr::Field { base: rewrite(base)?, field }),
        Expr::AnyAll { operand, operator, array, all } => alloc(Expr::AnyAll {
            operand: rewrite(operand)?,
            operator: *operator,
            array: rewrite(array)?,
            all: *all,
        }),
    }
}

/// Surfaces plan-time constant errors (e.g. `1/0`) across every expression
/// of a SELECT, matching PostgreSQL's constant folding.
pub fn check_select_constants<'a>(statement: &Select<'a>, arena: &'a Arena) -> Result<(), SqlError> {
    for item in statement.items {
        if let SelectItem::Expr { expression, .. } = item {
            super::eval::check_constant_errors(expression, arena)?;
        }
    }
    if let Some(w) = statement.where_clause {
        super::eval::check_constant_errors(w, arena)?;
    }
    for g in statement.group_by {
        super::eval::check_constant_errors(g, arena)?;
    }
    if let Some(h) = statement.having {
        super::eval::check_constant_errors(h, arena)?;
    }
    for ob in statement.order_by {
        super::eval::check_constant_errors(ob.expression, arena)?;
    }
    Ok(())
}

/// The SELECT entry point (FROM present; FROM-less selects stay in the
/// engine).
pub fn select_query<'a>(
    storage: &'a Storage,
    txid: u32,
    statement: &'a Select<'a>,
    arena: &'a Arena,
    params: &[Datum<'a>],
    responder: &mut Responder,
) -> Outcome {
    let from = statement.from.as_ref().expect("FROM-less handled by caller");
    // Windows over a grouped query evaluate after GROUP BY / HAVING: rewrite
    // to the two-level form up front (before the result is described) and run
    // the rewritten statement instead.
    {
        let mut win_probe: [&Expr; MAX_WINDOWS] = [&Expr::Null; MAX_WINDOWS];
        let mut n_win_probe = 0;
        let mut grouped_aggs: [(*const Expr, &Expr); MAX_AGGS] =
            [(core::ptr::null(), &Expr::Null); MAX_AGGS];
        let mut n_grouped_aggs = 0;
        for item in statement.items {
            if let SelectItem::Expr { expression, .. } = item {
                if let Err(e) = collect_windows(expression, &mut win_probe, &mut n_win_probe) {
                    return sql_fail(e);
                }
                if let Err(e) =
                    collect_grouped_aggs(expression, &mut grouped_aggs, &mut n_grouped_aggs)
                {
                    return sql_fail(e);
                }
            }
        }
        let has_srf = find_srf(statement.items).is_some();
        if (n_win_probe > 0 || has_srf)
            && (!statement.group_by.is_empty()
                || statement.having.is_some()
                || n_grouped_aggs > 0)
        {
            let rewritten = match rewrite_grouped_windows(statement, storage, txid, arena) {
                Ok(r) => r,
                Err(e) => return sql_fail(e),
            };
            return select_query(storage, txid, rewritten, arena, params, responder);
        }
    }
    // Catalog relations (pg_catalog / information_schema) are synthesized and
    // registered as derived tables by resolve_exec, so they flow through the
    // general executor — joins, subqueries, aggregates, and ORDER BY included.
    let scope = match QueryScope::resolve_exec(storage, from, txid, arena, params) {
        Ok(s) => s,
        Err(e) => return sql_fail(e),
    };
    // A GROUP BY position names a select-list column; resolve it before
    // anything reads the grouping keys.
    let statement = match resolve_group_ordinals(statement, &scope, arena) {
        Ok(s) => s,
        Err(e) => return sql_fail(e),
    };
    if let Err(e) = check_key_types(statement, &scope, arena) {
        return sql_fail(e);
    }

    // Subqueries first (uncorrelated, evaluated once).
    let mut sub_exprs: [Option<&Expr>; 4 + 2 * super::parser::MAX_LIST] =
        [None; 4 + 2 * super::parser::MAX_LIST];
    sub_exprs[0] = statement.where_clause;
    sub_exprs[1] = statement.having;
    for (i, item) in statement.items.iter().enumerate() {
        if let SelectItem::Expr { expression, .. } = item {
            sub_exprs[4 + i] = Some(expression);
        }
    }
    // ORDER BY expressions may carry (correlated) subqueries too; a bare
    // ordinal resolves to a select item already covered above.
    for (i, ob) in statement.order_by.iter().enumerate() {
        if !matches!(ob.expression, Expr::Int(_)) {
            sub_exprs[4 + super::parser::MAX_LIST + i] = Some(ob.expression);
        }
    }
    // Uncorrelated subqueries are evaluated once; correlated ones are deferred
    // and re-evaluated per outer row during the scan.
    let outer_subs = match prepare_outer_subqueries(&sub_exprs, storage, txid, arena, params) {
        Ok(s) => s,
        Err(e) => return sql_fail(e),
    };
    let correlated = outer_subs.correlated;
    let catalog = StorageCatalog { storage };
    let hooks = EvalHooks {
        group: None,
        aggs: None,
        subs: Some(&outer_subs.base),
        windows: None,
        catalog: Some(&catalog), srf_index: None,
    };

    // Plan-time type analysis: validate operator/aggregate types across every
    // clause so incompatible types error before scanning (matching
    // PostgreSQL), not only when a row reaches them. SELECT items are also
    // type-checked by describe below.
    {
        let columns = ScopeCols(&scope);
        let check = |e: &Expr| -> Result<(), SqlError> {
            super::exec::infer_type_res(e, &columns).map(|_| ())
        };
        let analyze = || -> Result<(), SqlError> {
            // SELECT-list items first: PostgreSQL analyzes types before it folds
            // constants, so an invalid aggregate/operator (e.g. `min(boolean)`)
            // errors ahead of a constant division elsewhere in the query.
            for item in statement.items {
                if let SelectItem::Expr { expression, .. } = item {
                    check(expression)?;
                }
            }
            if let Some(w) = statement.where_clause {
                check(w)?;
            }
            for g in statement.group_by {
                check(g)?;
            }
            if let Some(h) = statement.having {
                check(h)?;
            }
            for ob in statement.order_by {
                check(resolve_order_target(ob.expression, statement.items, &scope, arena)?)?;
            }
            Ok(())
        };
        if let Err(e) = analyze() {
            return sql_fail(e);
        }
    }

    // Constant folding runs after type analysis, matching PostgreSQL's
    // analyze-then-plan order: `min(boolean)` errors before `1/0` folds.
    if let Err(e) = check_select_constants(statement, arena) {
        return sql_fail(e);
    }

    // Result description.
    let mut columns = [ColDesc::new("", 0, 0); MAX_PROJ];
    let n_cols = match describe_scope_items(statement.items, &scope, &mut columns) {
        Ok(n) => n,
        Err(e) => return sql_fail(e),
    };
    patch_subquery_column_types(statement.items, Some(&scope), &outer_subs.base, params, storage, txid, arena, &mut columns[..n_cols]);
    responder.row_description(&columns[..n_cols])?;

    let limit = match super::exec::eval_limit_pub(statement.limit, arena, params) {
        Ok(l) => l,
        Err(e) => return sql_fail(e),
    };
    let offset = match super::exec::eval_offset_pub(statement.offset, arena, params) {
        Ok(o) => o,
        Err(e) => return sql_fail(e),
    };

    // LIMIT 0 returns no rows without scanning or projecting anything, as
    // PostgreSQL does — so a per-row error in an unreturned row does not
    // surface (constant errors already surfaced via the plan-time check).
    if limit == 0 {
        responder.command_complete("SELECT 0")?;
        return sql_ok();
    }

    // Window functions? They run over materialized rows before ORDER BY/LIMIT.
    // An ORDER BY key may be a window function without the select list holding
    // one (`ORDER BY rank() OVER (...)`), so it counts toward the decision.
    let mut win_nodes: [&Expr; MAX_WINDOWS] = [&Expr::Null; MAX_WINDOWS];
    let mut n_win = 0;
    for item in statement.items {
        if let SelectItem::Expr { expression, .. } = item
            && let Err(e) = collect_windows(expression, &mut win_nodes, &mut n_win)
        {
            return sql_fail(e);
        }
    }
    for ob in statement.order_by {
        if let Err(e) = collect_windows(ob.expression, &mut win_nodes, &mut n_win) {
            return sql_fail(e);
        }
    }
    if n_win > 0 {
        return window_select(
            storage, txid, statement, from, &scope, &win_nodes[..n_win], &hooks, correlated,
            &outer_subs.base, arena, params, limit, offset, responder,
        );
    }

    // Aggregates / GROUP BY?
    let mut agg_nodes: [(*const Expr, &Expr); MAX_AGGS] =
        [(core::ptr::null(), &Expr::Null); MAX_AGGS];
    let mut n_aggs = 0;
    for item in statement.items {
        if let SelectItem::Expr { expression, .. } = item
            && let Err(e) = collect_aggs(expression, &mut agg_nodes, &mut n_aggs) {
                return sql_fail(e);
            }
    }
    if let Some(h) = statement.having
        && let Err(e) = collect_aggs(h, &mut agg_nodes, &mut n_aggs) {
            return sql_fail(e);
        }
    if n_aggs > 0 || !statement.group_by.is_empty() {
        return grouped_select(
            storage,
            &scope,
            from,
            txid,
            statement,
            &agg_nodes[..n_aggs],
            arena,
            params,
            &hooks,
            correlated,
            limit,
            offset,
            responder,
        );
    }

    let needs_materialize = statement.distinct || !statement.order_by.is_empty();
    if !needs_materialize {
        // Stream.
        let mut emitted = 0u64;
        let mut skipped = 0u64;
        let mut wire_full = false;
        let mut wire_result: Result<(), WireFull> = Ok(());
        // With correlated subqueries, WHERE is applied per row against merged
        // hooks (which include the correlated results); otherwise the scan
        // applies WHERE directly for the common, faster path.
        let where_in_scan = if correlated.is_empty() { statement.where_clause } else { None };
        // A set-returning `_pg_expandarray(array)` expands each row into one output
        // row per array element.
        let srf_call = find_srf(statement.items);
        let scan = scan_source(
            storage,
            &scope,
            from,
            txid,
            where_in_scan,
            arena,
            params,
            &hooks,
            None,
            &mut |row| {
                if emitted >= limit {
                    return Ok(false);
                }
                // Per-row hooks for correlated subqueries; then WHERE.
                let mut sc: [(*const Expr, Datum, Datum); MAX_SUBQUERIES] =
                    [(core::ptr::null(), Datum::Null, Datum::Null); MAX_SUBQUERIES];
                let mut ls: [(*const Expr, &[Datum], bool, Datum); MAX_SUBQUERIES] =
                    [(core::ptr::null(), &[], false, Datum::Null); MAX_SUBQUERIES];
                let row_subs;
                let row_hooks_owned;
                let row_hooks: &EvalHooks = if correlated.is_empty() {
                    &hooks
                } else {
                    row_subs = merge_correlated(
                        correlated, &outer_subs.base, row, storage, txid, arena, params,
                        &mut sc, &mut ls,
                    )?;
                    row_hooks_owned =
                        EvalHooks { group: None, aggs: None, subs: Some(&row_subs) , windows: None, catalog: None, srf_index: None };
                    &row_hooks_owned
                };
                if !correlated.is_empty()
                    && let Some(w) = statement.where_clause {
                        match eval_full(w, arena, params, row, row_hooks)? {
                            Datum::Bool(true) => {}
                            Datum::Bool(false) | Datum::Null => return Ok(true),
                            _ => return Err(sql_err!(
                                sqlstate::DATATYPE_MISMATCH,
                                "argument of WHERE must be type boolean"
                            )),
                        }
                    }
                // Number of output rows this source row yields (1, unless an
                // `_pg_expandarray` expands it per array element).
                let count = srf_max_count(statement.items, arena, params, row, row_hooks)?;
                for k in 1..=count {
                    if emitted >= limit {
                        break;
                    }
                    if skipped < offset {
                        skipped += 1;
                        continue;
                    }
                    let srf_hooks;
                    let use_hooks: &EvalHooks = if srf_call.is_some() {
                        srf_hooks = EvalHooks { srf_index: Some(k), ..*row_hooks };
                        &srf_hooks
                    } else {
                        row_hooks
                    };
                    let mut projected = [Datum::Null; MAX_PROJ];
                    let n = project_row(statement.items, &scope, row, arena, params, use_hooks, &mut projected, None)?;
                    if let Err(w) = responder.data_row(&projected[..n]) {
                        wire_full = true;
                        wire_result = Err(w);
                        return Ok(false);
                    }
                    emitted += 1;
                }
                Ok(true)
            },
        );
        if wire_full {
            return Err(WireFull);
        }
        if let Err(e) = scan {
            return sql_fail(e);
        }
        let tag = stack_format!(48, "SELECT {}", emitted);
        responder.command_complete(tag.as_str())?;
        return sql_ok();
    }

    // Materialize: visible columns + hidden ORDER BY keys (set-returning
    // functions expand inside the materializer).
    materialized_select(
        storage, &scope, from, txid, statement, arena, params, &hooks, correlated, &outer_subs.base,
        limit, offset, responder,
    )
}

/// Rewrites a FROM-less SELECT to read from a one-row derived table, so the
/// scanning path can run it. The virtual row a FROM-less SELECT already has is
/// spelled out as `(SELECT 1)`; nothing in the select list refers to it.
fn over_one_row<'a>(
    statement: &'a Select<'a>,
    arena: &'a Arena,
) -> Result<&'a Select<'a>, SqlError> {
    let one = arena.alloc(Expr::Int(1)).map_err(|_| arena_full())?;
    let items = arena
        .alloc_slice_copy(&[SelectItem::Expr { expression: one, alias: None }])
        .map_err(|_| arena_full())?;
    let inner = Select { items, from: None, ..*statement };
    // The inner select carries only the row; every clause stays outside.
    let inner = Select {
        distinct: false,
        distinct_on: &[],
        where_clause: None,
        group_by: &[],
        grouping_sets: &[],
        having: None,
        order_by: &[],
        limit: None,
        offset: None,
        with: &[],
        set_body: None,
        ..inner
    };
    let inner = &*arena.alloc(inner).map_err(|_| arena_full())?;
    let from = FromClause {
        base: TableRef {
            schema: None,
            table: "",
            alias: Some("?onerow"),
            subquery: Some(inner),
            func_args: None,
            col_alias: None,
            cte: None,
            with_ordinality: false,
        },
        joins: &[],
    };
    Ok(&*arena.alloc(Select { from: Some(from), ..*statement }).map_err(|_| arena_full())?)
}

/// FROM-less `SELECT` (one virtual row, no columns). Item and WHERE
/// expressions may still contain subqueries — always uncorrelated here, since
/// there is no outer row to reference — so they are prepared once and injected
/// by node identity, exactly as the table path does.
pub fn constant_select<'a>(
    storage: &'a Storage,
    txid: u32,
    statement: &'a Select<'a>,
    arena: &'a Arena,
    params: &[Datum<'a>],
    responder: &mut Responder,
) -> Outcome {
    if let Err(e) = check_select_constants(statement, arena) {
        return sql_fail(e);
    }
    // A window function needs rows to compute over, and this path has no scan
    // to give it. A FROM-less SELECT is exactly one row, though, so it can be
    // written as a one-row derived table and handed to the ordinary path —
    // which already knows about partitions, frames and every window function
    // there is. Teaching this path any of that would be a second copy.
    let mut win_probe: [&Expr; MAX_WINDOWS] = [&Expr::Null; MAX_WINDOWS];
    let mut n_win = 0;
    for item in statement.items {
        if let SelectItem::Expr { expression, .. } = item
            && let Err(e) = collect_windows(expression, &mut win_probe, &mut n_win)
        {
            return sql_fail(e);
        }
    }
    if n_win > 0 {
        return match over_one_row(statement, arena) {
            Ok(wrapped) => select_query(storage, txid, wrapped, arena, params, responder),
            Err(e) => sql_fail(e),
        };
    }
    let mut columns = [ColDesc::new("", 0, 0); MAX_PROJ];
    let n = match describe_items(statement.items, None, &mut columns) {
        Ok(n) => n,
        Err(e) => return sql_fail(e),
    };

    let mut sub_exprs: [Option<&Expr>; 1 + MAX_PROJ] = [None; 1 + MAX_PROJ];
    sub_exprs[0] = statement.where_clause;
    for (i, item) in statement.items.iter().enumerate() {
        if let SelectItem::Expr { expression, .. } = item {
            sub_exprs[1 + i] = Some(expression);
        }
    }
    let subs = match prepare_subqueries(&sub_exprs, storage, txid, arena, params, SUBQUERY_DEPTH, None)
    {
        Ok(s) => s,
        Err(e) => return sql_fail(e),
    };
    patch_subquery_column_types(statement.items, None, &subs, params, storage, txid, arena, &mut columns[..n]);
    let hooks = EvalHooks { group: None, aggs: None, subs: Some(&subs) , windows: None, catalog: None, srf_index: None };

    // Aggregates (or GROUP BY / HAVING) without FROM: PostgreSQL aggregates
    // over one virtual input row (zero when WHERE is false) and emits at most
    // one output row.
    let mut agg_nodes: [(*const Expr, &Expr); MAX_AGGS] =
        [(core::ptr::null(), &Expr::Null); MAX_AGGS];
    let mut n_aggs = 0;
    for item in statement.items {
        if let SelectItem::Expr { expression, .. } = item
            && let Err(e) = collect_aggs(expression, &mut agg_nodes, &mut n_aggs) {
                return sql_fail(e);
            }
    }
    if let Some(h) = statement.having
        && let Err(e) = collect_aggs(h, &mut agg_nodes, &mut n_aggs)
    {
        return sql_fail(e);
    }
    if n_aggs > 0 || statement.having.is_some() || !statement.group_by.is_empty() {
        if find_srf(statement.items).is_some() {
            // The set-returning function expands after aggregation: rewrite
            // to the two-level form (aggregates in a derived table) and run
            // through the FROM executor.
            let rewritten = match rewrite_grouped_windows(statement, storage, txid, arena) {
                Ok(r) => r,
                Err(e) => return sql_fail(e),
            };
            return select_query(storage, txid, rewritten, arena, params, responder);
        }
        responder.row_description(&columns[..n])?;
        let hook_data =
            match fromless_aggregate_hooks(statement, &agg_nodes[..n_aggs], arena, params, &super::eval::NoColumns, &hooks)
            {
                Ok(d) => d,
                Err(e) => return sql_fail(e),
            };
        let mut rows = 0u64;
        if let Some((ptrs, values)) = hook_data {
            let agg_hooks = EvalHooks { aggs: Some((ptrs, values)), ..hooks };
            let mut vals = [Datum::Null; MAX_PROJ];
            for (i, item) in statement.items.iter().enumerate() {
                let SelectItem::Expr { expression, .. } = item else {
                    unreachable!("wildcard rejected by describe_items");
                };
                match eval_full(expression, arena, params, &super::eval::NoColumns, &agg_hooks) {
                    Ok(v) => vals[i] = v,
                    Err(e) => return sql_fail(e),
                }
            }
            responder.data_row(&vals[..statement.items.len()])?;
            rows = 1;
        }
        let tag = stack_format!(48, "SELECT {}", rows);
        responder.command_complete(tag.as_str())?;
        return sql_ok();
    }

    // A set-returning function in the select list expands the single virtual
    // row into one output row per element/value.
    let srf_call = find_srf(statement.items);
    let count = match srf_max_count(statement.items, arena, params, &super::eval::NoColumns, &hooks) {
        Ok(n) => n,
        Err(e) => return sql_fail(e),
    };
    responder.row_description(&columns[..n])?;
    // Resolve ORDER BY targets against the select list: ordinals and output
    // names/expressions bind to an item (whose computed value is the key —
    // a set-returning item cannot re-evaluate outside its hook); anything
    // else evaluates per output row.
    // Each item occupies `col_start[i]..col_start[i+1]` output columns; a
    // `(record).*` item expands to several, everything else to one. `width` is
    // the true visible column count (matching the row description).
    let mut col_start = [0usize; MAX_PROJ + 1];
    {
        let mut col = 0usize;
        for (i, item) in statement.items.iter().enumerate() {
            col_start[i] = col;
            col += match item {
                SelectItem::RecordStar(base) => {
                    super::exec::record_shape(base, &super::exec::NoCols, |_, _| {}).unwrap_or(0)
                }
                _ => 1,
            };
        }
        col_start[statement.items.len()] = col;
    }
    let width = col_start[statement.items.len()];
    let n_order = statement.order_by.len();
    let mut order_item: [Option<usize>; MAX_PROJ] = [None; MAX_PROJ];
    for (j, ob) in statement.order_by.iter().enumerate() {
        if let Expr::Int(pos) = ob.expression {
            if *pos < 1 || *pos as usize > width {
                return sql_fail(sql_err!(
                    "42P10",
                    "ORDER BY position {} is not in select list",
                    pos
                ));
            }
            order_item[j] = Some(*pos as usize - 1);
            continue;
        }
        // A non-ordinal key binds to the (single-column) item whose expression
        // or output name it matches; record-star items never match by name.
        order_item[j] = statement.items.iter().position(|item| {
            matches!(item, SelectItem::Expr { expression, alias }
                if **expression == *ob.expression
                    || matches!(ob.expression, Expr::Column { qualifier: None, name }
                        if *name == alias.unwrap_or(super::exec::derived_name(expression))))
        }).map(|i| col_start[i]);
        if statement.distinct && order_item[j].is_none() {
            return sql_fail(sql_err!(
                "42P10",
                "for SELECT DISTINCT, ORDER BY expressions must appear in select list"
            ));
        }
    }

    // Materialize every output row (visible values + hidden sort keys).
    let mut n_rows = 0usize;
    let max_rows = count;
    let empty: &[u8] = &[];
    let encoded = match arena.alloc_slice_with(max_rows, |_| empty) {
        Ok(e) => e,
        Err(_) => return sql_fail(arena_full()),
    };
    for k in 1..=count {
        let khooks = if srf_call.is_some() {
            EvalHooks { srf_index: Some(k), ..hooks }
        } else {
            hooks
        };
        let mut values = [Datum::Null; MAX_PROJ + MAX_PROJ];
        for (i, item) in statement.items.iter().enumerate() {
            match item {
                SelectItem::Expr { expression, .. } => {
                    match eval_full(expression, arena, params, &super::eval::NoColumns, &khooks) {
                        Ok(v) => values[col_start[i]] = v,
                        Err(e) => return sql_fail(e),
                    }
                }
                SelectItem::RecordStar(base) => {
                    match super::eval::record_star_expand(base, arena, params, &super::eval::NoColumns, &khooks) {
                        Ok(fields) => {
                            for (k, f) in fields.iter().enumerate() {
                                values[col_start[i] + k] = f.value;
                            }
                        }
                        Err(e) => return sql_fail(e),
                    }
                }
                _ => unreachable!("wildcard rejected by describe_items in a FROM-less select"),
            }
        }
        if let Some(w) = statement.where_clause {
            match where_passes(w, arena, params, &super::eval::NoColumns, &khooks) {
                Ok(false) => continue,
                Ok(true) => {}
                Err(e) => return sql_fail(e),
            }
        }
        for (j, ob) in statement.order_by.iter().enumerate() {
            values[width + j] = match order_item[j] {
                Some(i) => values[i],
                None => {
                    match eval_full(ob.expression, arena, params, &super::eval::NoColumns, &khooks)
                    {
                        Ok(v) => v,
                        Err(e) => return sql_fail(e),
                    }
                }
            };
        }
        encoded[n_rows] = match super::exec::encode_projected_pub(&values[..width + n_order], arena)
        {
            Ok(b) => b,
            Err(e) => return sql_fail(e),
        };
        n_rows += 1;
    }
    let out_rows = &mut encoded[..n_rows];

    let mut live = out_rows.len();
    if statement.distinct {
        out_rows.sort_unstable();
        let mut unique = 0usize;
        for i in 0..out_rows.len() {
            let same = i > 0
                && visible_prefix(out_rows[i], width) == visible_prefix(out_rows[i - 1], width);
            if !same {
                out_rows[unique] = out_rows[i];
                unique += 1;
            }
        }
        live = unique;
    }
    let out_rows = &mut out_rows[..live];
    if n_order > 0 {
        out_rows.sort_unstable_by(|a, b| {
            for (j, ob) in statement.order_by.iter().enumerate() {
                let ka = super::exec::decode_projected_pub(a, width + j);
                let kb = super::exec::decode_projected_pub(b, width + j);
                let ord = match (ka.is_null(), kb.is_null()) {
                    (true, true) => core::cmp::Ordering::Equal,
                    (true, false) => {
                        if ob.nulls_first { core::cmp::Ordering::Less } else { core::cmp::Ordering::Greater }
                    }
                    (false, true) => {
                        if ob.nulls_first { core::cmp::Ordering::Greater } else { core::cmp::Ordering::Less }
                    }
                    (false, false) => {
                        let c = compare_datums(&ka, &kb).unwrap_or(core::cmp::Ordering::Equal);
                        if ob.descending { c.reverse() } else { c }
                    }
                };
                if !ord.is_eq() {
                    return ord;
                }
            }
            core::cmp::Ordering::Equal
        });
    }

    let limit = match super::exec::eval_limit_pub(statement.limit, arena, params) {
        Ok(l) => l,
        Err(e) => return sql_fail(e),
    };
    let offset = match super::exec::eval_offset_pub(statement.offset, arena, params) {
        Ok(o) => o,
        Err(e) => return sql_fail(e),
    };
    let start = (offset as usize).min(out_rows.len());
    let take = ((out_rows.len() - start) as u64).min(limit) as usize;
    let mut rows = 0u64;
    for row in &out_rows[start..start + take] {
        let mut values = [Datum::Null; MAX_PROJ];
        for (i, slot) in values.iter_mut().take(width).enumerate() {
            *slot = super::exec::decode_projected_pub(row, i);
        }
        responder.data_row(&values[..width])?;
        rows += 1;
    }
    let tag = stack_format!(32, "SELECT {}", rows);
    responder.command_complete(tag.as_str())?;
    sql_ok()
}

/// Runs a `SELECT` used as an INSERT source, invoking `emit` once per output
/// row with that row's projected datums. The resulting table is unordered, so
/// ORDER BY is ignored; DISTINCT/GROUP BY/aggregate sources are rejected loudly
/// (not yet supported). Subqueries (including correlated) in the source are
/// supported.
#[allow(clippy::too_many_arguments)]
pub fn select_into_rows<'a>(
    storage: &'a Storage,
    txid: u32,
    statement: &'a Select<'a>,
    arena: &'a Arena,
    params: &[Datum<'a>],
    outer: Option<&dyn ColumnLookup<'a>>,
    emit: &mut dyn FnMut(&[Datum<'a>]) -> Result<(), SqlError>,
) -> Result<(), SqlError> {
    if let Some(tree) = statement.set_body {
        let (rows, _target, n) = materialize_set_body(storage, txid, tree, arena, params)?;
        let mut vals = [Datum::Null; MAX_PROJ];
        for row in rows.iter() {
            for (c, slot) in vals[..n].iter_mut().enumerate() {
                *slot = super::exec::decode_projected_pub(row, c);
            }
            emit(&vals[..n])?;
        }
        return Ok(());
    }
    check_select_constants(statement, arena)?;
    let mut agg_nodes: [(*const Expr, &Expr); MAX_AGGS] =
        [(core::ptr::null(), &Expr::Null); MAX_AGGS];
    let mut n_aggs = 0;
    for item in statement.items {
        if let SelectItem::Expr { expression, .. } = item {
            collect_aggs(expression, &mut agg_nodes, &mut n_aggs)?;
        }
    }
    if let Some(h) = statement.having {
        collect_aggs(h, &mut agg_nodes, &mut n_aggs)?;
    }
    // GROUP BY or aggregates: run the grouped executor (which sorts by any
    // ORDER BY and dedups DISTINCT) and emit each output row, honoring
    // LIMIT/OFFSET. A set-returning function expands after aggregation —
    // rewrite to the two-level form first.
    if (!statement.group_by.is_empty() || n_aggs > 0) && find_srf(statement.items).is_some() {
        let rewritten = rewrite_grouped_windows(statement, storage, txid, arena)?;
        return select_into_rows(storage, txid, rewritten, arena, params, outer, emit);
    }
    if !statement.group_by.is_empty() || n_aggs > 0 {
        let Some(from) = &statement.from else {
            // FROM-less aggregate: one virtual input row.
            let mut sub_exprs: [Option<&Expr>; 2 + MAX_PROJ] = [None; 2 + MAX_PROJ];
            sub_exprs[0] = statement.where_clause;
            sub_exprs[1] = statement.having;
            for (i, item) in statement.items.iter().enumerate() {
                if let SelectItem::Expr { expression, .. } = item {
                    sub_exprs[2 + i] = Some(expression);
                }
            }
            let subs =
                prepare_subqueries(&sub_exprs, storage, txid, arena, params, SUBQUERY_DEPTH, None)?;
            let hooks = EvalHooks {
                group: None,
                aggs: None,
                subs: Some(&subs),
                windows: None,
                catalog: None,
                srf_index: None,
            };
            let Some((ptrs, values)) =
                fromless_aggregate_hooks(statement, &agg_nodes[..n_aggs], arena, params, &super::eval::NoColumns, &hooks)?
            else {
                return Ok(());
            };
            let agg_hooks = EvalHooks { aggs: Some((ptrs, values)), ..hooks };
            let mut vals = [Datum::Null; MAX_PROJ];
            let mut n = 0;
            for item in statement.items {
                match item {
                    SelectItem::Expr { expression, .. } => {
                        vals[n] = eval_full(expression, arena, params, &super::eval::NoColumns, &agg_hooks)?;
                        n += 1;
                    }
                    SelectItem::RecordStar(base) => {
                        for field in super::eval::record_star_expand(base, arena, params, &super::eval::NoColumns, &agg_hooks)? {
                            vals[n] = field.value;
                            n += 1;
                        }
                    }
                    _ => return Err(sql_err!(
                        sqlstate::SYNTAX_ERROR,
                        "SELECT * with no tables specified is not valid"
                    )),
                }
            }
            emit(&vals[..n])?;
            return Ok(());
        };
        let scope = QueryScope::resolve_exec(storage, from, txid, arena, params)?;
        let statement = resolve_group_ordinals(statement, &scope, arena)?;
        check_key_types(statement, &scope, arena)?;
        let mut sub_exprs: [Option<&Expr>; 2 + MAX_PROJ] = [None; 2 + MAX_PROJ];
        sub_exprs[0] = statement.where_clause;
        sub_exprs[1] = statement.having;
        for (i, item) in statement.items.iter().enumerate() {
            if let SelectItem::Expr { expression, .. } = item {
                sub_exprs[2 + i] = Some(expression);
            }
        }
        let outer_subs = prepare_outer_subqueries(&sub_exprs, storage, txid, arena, params)?;
        let hooks = EvalHooks { group: None, aggs: None, subs: Some(&outer_subs.base) , windows: None, catalog: None, srf_index: None };
        let (rows, width) = grouped_rows(
            storage, &scope, from, txid, statement, &agg_nodes[..n_aggs], arena, params, &hooks,
            outer_subs.correlated, outer,
        )?;
        let limit = super::exec::eval_limit_pub(statement.limit, arena, params)?;
        let offset = super::exec::eval_offset_pub(statement.offset, arena, params)?;
        let start = (offset as usize).min(rows.len());
        let n = ((rows.len() - start) as u64).min(limit) as usize;
        for row in &rows[start..start + n] {
            let mut out = [Datum::Null; MAX_PROJ];
            for (i, slot) in out.iter_mut().take(width).enumerate() {
                *slot = super::exec::decode_projected_pub(row, i);
            }
            emit(&out[..width])?;
        }
        return Ok(());
    }
    let mut sub_exprs: [Option<&Expr>; 1 + MAX_PROJ] = [None; 1 + MAX_PROJ];
    sub_exprs[0] = statement.where_clause;
    for (i, item) in statement.items.iter().enumerate() {
        if let SelectItem::Expr { expression, .. } = item {
            sub_exprs[1 + i] = Some(expression);
        }
    }

    let Some(from) = &statement.from else {
        // A window function here has nothing to compute over, so the single
        // virtual row is spelled out as a derived table and the whole query
        // re-enters through the scanning path (as `constant_select` does).
        let mut win_probe: [&Expr; MAX_WINDOWS] = [&Expr::Null; MAX_WINDOWS];
        let mut n_win = 0;
        for item in statement.items {
            if let SelectItem::Expr { expression, .. } = item {
                collect_windows(expression, &mut win_probe, &mut n_win)?;
            }
        }
        if n_win > 0 {
            let wrapped = over_one_row(statement, arena)?;
            return select_into_rows(storage, txid, wrapped, arena, params, outer, emit);
        }
        // FROM-less: one row (or zero, when WHERE is false), unless a
        // set-returning function in the list expands it to several.
        let subs =
            prepare_subqueries(&sub_exprs, storage, txid, arena, params, SUBQUERY_DEPTH, None)?;
        let hooks = EvalHooks { group: None, aggs: None, subs: Some(&subs) , windows: None, catalog: None, srf_index: None };
        let srf_call = find_srf(statement.items);
        let count = srf_max_count(statement.items, arena, params, &super::eval::NoColumns, &hooks)?;
        for k in 1..=count {
            let khooks = if srf_call.is_some() {
                EvalHooks { srf_index: Some(k), ..hooks }
            } else {
                hooks
            };
            if let Some(w) = statement.where_clause
                && !where_passes(w, arena, params, &super::eval::NoColumns, &khooks)?
            {
                continue;
            }
            let mut vals = [Datum::Null; MAX_PROJ];
            let mut n = 0;
            for item in statement.items {
                match item {
                    SelectItem::Expr { expression, .. } => {
                        vals[n] = eval_full(expression, arena, params, &super::eval::NoColumns, &khooks)?;
                        n += 1;
                    }
                    SelectItem::RecordStar(base) => {
                        for field in super::eval::record_star_expand(base, arena, params, &super::eval::NoColumns, &khooks)? {
                            vals[n] = field.value;
                            n += 1;
                        }
                    }
                    _ => return Err(sql_err!(sqlstate::SYNTAX_ERROR, "SELECT * with no tables specified is not valid")),
                }
            }
            emit(&vals[..n])?;
        }
        return Ok(());
    };

    let scope = QueryScope::resolve_exec(storage, from, txid, arena, params)?;
    let outer_subs = prepare_outer_subqueries(&sub_exprs, storage, txid, arena, params)?;
    let correlated = outer_subs.correlated;
    let hooks = EvalHooks { group: None, aggs: None, subs: Some(&outer_subs.base) , windows: None, catalog: None, srf_index: None };

    // Window functions (`OVER (...)`) in the projection: materialize the rows
    // with each window value computed, then emit. ORDER BY/LIMIT are handled by
    // the outer query, so the derived-table order is left unspecified.
    let mut win_nodes: [&Expr; MAX_WINDOWS] = [&Expr::Null; MAX_WINDOWS];
    let mut n_win = 0;
    for item in statement.items {
        if let SelectItem::Expr { expression, .. } = item {
            collect_windows(expression, &mut win_nodes, &mut n_win)?;
        }
    }
    for ob in statement.order_by {
        collect_windows(ob.expression, &mut win_nodes, &mut n_win)?;
    }
    if n_win > 0 {
        // Windows over a grouped query: rewrite to the two-level form.
        let mut grouped_aggs: [(*const Expr, &Expr); MAX_AGGS] =
            [(core::ptr::null(), &Expr::Null); MAX_AGGS];
        let mut n_grouped_aggs = 0;
        for item in statement.items {
            if let SelectItem::Expr { expression, .. } = item {
                collect_grouped_aggs(expression, &mut grouped_aggs, &mut n_grouped_aggs)?;
            }
        }
        if !statement.group_by.is_empty() || statement.having.is_some() || n_grouped_aggs > 0 {
            let rewritten = rewrite_grouped_windows(statement, storage, txid, arena)?;
            return select_into_rows(storage, txid, rewritten, arena, params, outer, emit);
        }
        let (proj_rows, sort_keys) = project_window_rows(
            storage, txid, statement, from, &scope, &win_nodes[..n_win], &hooks, correlated,
            &outer_subs.base, arena, params, outer,
        )?;
        // DISTINCT dedups on the projected values; ORDER BY and LIMIT/OFFSET
        // apply here too (a derived table keeps its inner LIMIT).
        let (proj_rows, sort_keys) = if statement.distinct {
            dedup_window_rows(proj_rows, sort_keys, arena)?
        } else {
            (proj_rows, sort_keys)
        };
        let count = proj_rows.len();
        let order = arena.alloc_slice_with(count, |i| i).map_err(|_| arena_full())?;
        if !statement.order_by.is_empty() {
            for x in 1..count {
                let mut y = x;
                while y > 0 {
                    let c = cmp_key_rows(
                        sort_keys[order[y - 1]],
                        sort_keys[order[y]],
                        statement.order_by,
                    );
                    if c == core::cmp::Ordering::Greater {
                        order.swap(y - 1, y);
                        y -= 1;
                    } else {
                        break;
                    }
                }
            }
        }
        let limit = super::exec::eval_limit_pub(statement.limit, arena, params)?;
        let offset = super::exec::eval_offset_pub(statement.offset, arena, params)?;
        let start = (offset as usize).min(count);
        let n = ((count - start) as u64).min(limit) as usize;
        for &i in &order[start..start + n] {
            emit(proj_rows[i])?;
        }
        return Ok(());
    }

    // DISTINCT / ORDER BY / LIMIT / OFFSET need the whole set materialized
    // (so top-N and dedup are correct), then paged.
    if statement.distinct || !statement.order_by.is_empty() || statement.limit.is_some() || statement.offset.is_some() {
        let (rows, width, deferred) = materialized_rows(
            storage, &scope, from, txid, statement, arena, params, &hooks, correlated, &outer_subs.base, outer,
        )?;
        let limit = super::exec::eval_limit_pub(statement.limit, arena, params)?;
        let offset = super::exec::eval_offset_pub(statement.offset, arena, params)?;
        // OFFSET rows flow through PostgreSQL's projection before Limit
        // discards them, so deferred items are evaluated for them too (their
        // errors surface); only rows past the offset are emitted.
        let window = offset.saturating_add(limit).min(usize::MAX as u64) as usize;
        for (index, row) in rows.iter().take(window).enumerate() {
            let mut out = [Datum::Null; MAX_PROJ];
            finalize_projected_row(
                row, width, deferred.as_ref(), statement, &scope, arena, params, &hooks, &mut out,
            )?;
            if (index as u64) >= offset {
                emit(&out[..width])?;
            }
        }
        return Ok(());
    }
    let where_in_scan = if correlated.is_empty() { statement.where_clause } else { None };

    // A set-returning `_pg_expandarray(array)` in the projection expands each
    // source row into one output row per array element.
    let srf_call = find_srf(statement.items);
    scan_source(
        storage, &scope, from, txid, where_in_scan, arena, params, &hooks, None,
        &mut |row| {
            let mut sc: [(*const Expr, Datum, Datum); MAX_SUBQUERIES] =
                [(core::ptr::null(), Datum::Null, Datum::Null); MAX_SUBQUERIES];
            let mut ls: [(*const Expr, &[Datum], bool, Datum); MAX_SUBQUERIES] =
                [(core::ptr::null(), &[], false, Datum::Null); MAX_SUBQUERIES];
            let row_subs;
            let row_hooks_owned;
            let row_hooks: &EvalHooks = if correlated.is_empty() {
                &hooks
            } else {
                row_subs = merge_correlated(
                    correlated, &outer_subs.base, row, storage, txid, arena, params, &mut sc, &mut ls,
                )?;
                row_hooks_owned = EvalHooks { group: None, aggs: None, subs: Some(&row_subs) , windows: None, catalog: None, srf_index: None };
                if let Some(w) = statement.where_clause
                    && !where_passes(w, arena, params, row, &row_hooks_owned)? {
                        return Ok(true);
                    }
                &row_hooks_owned
            };
            let mut projected = [Datum::Null; MAX_PROJ];
            match srf_call {
                None => {
                    let n = project_row(statement.items, &scope, row, arena, params, row_hooks, &mut projected, None)?;
                    emit(&projected[..n])?;
                }
                Some(c) => {
                    let count = srf_count(c, arena, params, row, row_hooks)?;
                    for k in 1..=count {
                        let srf_hooks = EvalHooks { srf_index: Some(k), ..*row_hooks };
                        let n = project_row(statement.items, &scope, row, arena, params, &srf_hooks, &mut projected, None)?;
                        emit(&projected[..n])?;
                    }
                }
            }
            Ok(true)
        },
    )
}

/// Projects one source row through the select items.
#[expect(clippy::too_many_arguments, reason = "query pipeline plumbing")]
fn project_row<'a>(
    items: &[SelectItem<'a>],
    scope: &QueryScope,
    row: &JoinRow<'_, 'a, '_>,
    arena: &'a Arena,
    params: &[Datum<'a>],
    hooks: &EvalHooks<'_, 'a>,
    out: &mut [Datum<'a>; MAX_PROJ],
    // Enclosing query's row when this is a correlated subquery's body, so an
    // outer column in the select list resolves after this row's own.
    outer: Option<&dyn ColumnLookup<'a>>,
) -> Result<usize, SqlError> {
    project_row_skipping(items, None, scope, row, arena, params, hooks, out, outer)
}

/// [`project_row`], with `skip` marking items whose evaluation is deferred
/// until after the sort (their slots stay NULL placeholders).
#[expect(clippy::too_many_arguments, reason = "query pipeline plumbing")]
fn project_row_skipping<'a>(
    items: &[SelectItem<'a>],
    skip: Option<&[bool; MAX_PROJ]>,
    scope: &QueryScope,
    row: &JoinRow<'_, 'a, '_>,
    arena: &'a Arena,
    params: &[Datum<'a>],
    hooks: &EvalHooks<'_, 'a>,
    out: &mut [Datum<'a>; MAX_PROJ],
    outer: Option<&dyn ColumnLookup<'a>>,
) -> Result<usize, SqlError> {
    // Star expansion reads this query's tables directly; only expressions may
    // reach past them to the enclosing row.
    let chained = Chained { inner: row, outer };
    let mut n = 0;
    for (item_index, item) in items.iter().enumerate() {
        if skip.is_some_and(|s| s[item_index]) {
            // A postponed item occupies one slot (wildcards are never skipped).
            if n == MAX_PROJ {
                return Err(sql_err!(
                    sqlstate::PROGRAM_LIMIT_EXCEEDED,
                    "select list expands past {} columns",
                    MAX_PROJ
                ));
            }
            out[n] = Datum::Null;
            n += 1;
            continue;
        }
        match item {
            SelectItem::TableWildcard(q) => {
                let t = scope.table_index(q)?;
                let vals = row.values[t].expect("bound");
                for c in 0..scope.defs[t].expect("resolved").n_columns {
                    if n == MAX_PROJ {
                        return Err(sql_err!(
                            sqlstate::PROGRAM_LIMIT_EXCEEDED,
                            "select list expands past {} columns",
                            MAX_PROJ
                        ));
                    }
                    out[n] = if vals.is_empty() { Datum::Null } else { vals[c] };
                    n += 1;
                }
            }
            SelectItem::Wildcard => {
                let value_of = |t: usize, c: usize| {
                    let vals = row.values[t].expect("bound");
                    if vals.is_empty() { Datum::Null } else { vals[c] }
                };
                for k in 0..scope.star_columns() {
                    if n == MAX_PROJ {
                        return Err(sql_err!(
                            sqlstate::PROGRAM_LIMIT_EXCEEDED,
                            "select list expands past {} columns",
                            MAX_PROJ
                        ));
                    }
                    out[n] = match scope.star_entry(k) {
                        ResolvedColumn::Table(t, c) => value_of(t, c),
                        // Merged USING/NATURAL column: first non-null side.
                        ResolvedColumn::Merged(m) => {
                            let mc = &scope.merged[m];
                            mc.parts[..mc.n_parts]
                                .iter()
                                .map(|&(t, c)| value_of(t, c))
                                .find(|v| !v.is_null())
                                .unwrap_or(Datum::Null)
                        }
                    };
                    n += 1;
                }
            }
            SelectItem::RecordStar(base) => {
                for field in super::eval::record_star_expand(base, arena, params, row, hooks)? {
                    if n == MAX_PROJ {
                        return Err(sql_err!(
                            sqlstate::PROGRAM_LIMIT_EXCEEDED,
                            "select list expands past {} columns",
                            MAX_PROJ
                        ));
                    }
                    out[n] = field.value;
                    n += 1;
                }
            }
            SelectItem::Expr { expression, .. } => {
                if n == MAX_PROJ {
                    return Err(sql_err!(
                        sqlstate::PROGRAM_LIMIT_EXCEEDED,
                        "select list expands past {} columns",
                        MAX_PROJ
                    ));
                }
                out[n] = eval_full(expression, arena, params, &chained, hooks)?;
                n += 1;
            }
        }
    }
    Ok(n)
}


/// Column descriptions across the whole scope (wildcards expand every
/// table).
pub fn describe_scope_items<'q>(
    items: &[SelectItem<'q>],
    scope: &QueryScope<'q>,
    out: &mut [ColDesc<'q>],
) -> Result<usize, SqlError> {
    // The single-table fast path resolves a qualifier against the table name,
    // so only take it when the exposed name equals the table name (no alias);
    // an aliased table falls through to the alias-aware scope path below.
    if scope.n == 1 && scope.names[0] == scope.defs[0].expect("resolved").name.as_str() {
        return describe_items(items, Some(scope.defs[0].expect("resolved")), out);
    }
    let mut n = 0;
    for item in items {
        match item {
            SelectItem::TableWildcard(q) => {
                let t = scope.table_index(q)?;
                for c in scope.defs[t].expect("resolved").columns() {
                    if n == out.len() {
                        return Err(sql_err!(sqlstate::PROGRAM_LIMIT_EXCEEDED, "select list too wide"));
                    }
                    out[n] = ColDesc::of_type(c.name.as_str(), c.ctype);
                    n += 1;
                }
            }
            SelectItem::Wildcard => {
                for k in 0..scope.star_columns() {
                    if n == out.len() {
                        return Err(sql_err!(sqlstate::PROGRAM_LIMIT_EXCEEDED, "select list too wide"));
                    }
                    let entry = scope.star_entry(k);
                    out[n] =
                        ColDesc::of_type(scope.output_name(entry), scope.output_type(entry));
                    n += 1;
                }
            }
            SelectItem::RecordStar(base) => {
                n = describe_scope_record_star(base, scope, out, n)?;
            }
            SelectItem::Expr { expression, alias } => {
                if n == out.len() {
                    return Err(sql_err!(sqlstate::PROGRAM_LIMIT_EXCEEDED, "select list too wide"));
                }
                // Multi-table type inference: columns resolve via scope.
                let (oid, typlen) = infer_scope_type(expression, scope)?;
                let name = alias.unwrap_or(super::exec::derived_name(expression));
                out[n] = ColDesc::new(name, oid, typlen);
                n += 1;
            }
        }
    }
    Ok(n)
}

/// Emits one `ColDesc` per field of a `(record).*` expansion against a join
/// scope, resolving whole-row bases to their table's columns. Returns the new
/// column count.
fn describe_scope_record_star<'q>(
    base: &Expr<'q>,
    scope: &QueryScope<'q>,
    out: &mut [ColDesc<'q>],
    mut n: usize,
) -> Result<usize, SqlError> {
    let mut push = |desc: ColDesc<'q>, n: &mut usize| -> Result<(), SqlError> {
        if *n == out.len() {
            return Err(sql_err!(sqlstate::PROGRAM_LIMIT_EXCEEDED, "select list too wide"));
        }
        out[*n] = desc;
        *n += 1;
        Ok(())
    };
    match base {
        Expr::Call { name, args, .. } if name.eq_ignore_ascii_case("row") => {
            super::exec::check_row_field_types(base, &ScopeCols(scope))?;
            for (i, arg) in args.iter().take(super::exec::RECORD_FIELD_NAMES.len()).enumerate() {
                let (oid, typlen) = infer_scope_type(arg, scope)?;
                push(ColDesc::new(super::exec::RECORD_FIELD_NAMES[i], oid, typlen), &mut n)?;
            }
        }
        Expr::WholeRow(table) | Expr::Column { qualifier: None, name: table }
            if scope.table_index(table).is_ok() =>
        {
            let t = scope.table_index(table)?;
            for c in &scope.defs[t].expect("resolved").columns()[..scope.defs[t].expect("resolved").n_columns] {
                push(ColDesc::of_type(c.name.as_str(), c.ctype), &mut n)?;
            }
        }
        // json_each family: `(key, value)` with statically-known names/types.
        Expr::Call { name, .. } if super::exec::json_each_value_type_pub(name).is_some() => {
            push(ColDesc::of_type("key", ColType::Text), &mut n)?;
            let value_type = super::exec::json_each_value_type_pub(name).expect("checked");
            push(ColDesc::of_type("value", value_type), &mut n)?;
        }
        _ => {
            return Err(sql_err!(
                "42809",
                "row expansion is not supported on this expression"
            ));
        }
    }
    Ok(n)
}

/// Resolves column types across all tables in a join scope.
pub(super) struct ScopeCols<'s, 'd>(&'s QueryScope<'d>);
impl super::exec::ColTypeResolver for ScopeCols<'_, '_> {
    fn resolve(&self, qualifier: Option<&str>, name: &str) -> Result<ColType, SqlError> {
        let entry = self.0.find_column(qualifier, name)?;
        Ok(self.0.output_type(entry))
    }

    fn is_whole_row(&self, name: &str) -> bool {
        self.0.table_index(name).is_ok()
    }

    fn whole_row_scalar_type(&self, name: &str) -> Option<ColType> {
        self.0.func_scalar_type(name)
    }

    fn table_columns(&self, name: &str) -> Option<&[ColumnMeta]> {
        let t = self.0.table_index(name).ok()?;
        let def = self.0.defs[t]?;
        Some(&def.columns()[..def.n_columns])
    }
}

/// The number of columns a `(base).*` record expansion contributes, or 0 when
/// its shape is not statically known (surfaced loudly at projection time).
pub(super) fn record_star_width(base: &Expr, scope: &QueryScope) -> usize {
    super::exec::record_shape(base, &ScopeCols(scope), |_, _| {}).unwrap_or(0)
}

fn infer_scope_type(expression: &Expr, scope: &QueryScope) -> Result<(i32, i16), SqlError> {
    let (oid, typlen) = super::exec::infer_type_res(expression, &ScopeCols(scope))?;
    if oid == super::types::oid::UNKNOWN {
        Ok((super::types::oid::TEXT, -1))
    } else {
        Ok((oid, typlen))
    }
}

/// Whether `target` occurs within `e` (pointer identity, expression-level
/// walk — nested subquery bodies evaluate their own subqueries).
fn expr_contains_node<'a>(e: &'a Expr<'a>, target: *const Expr<'a>) -> bool {
    if core::ptr::eq(e, target) {
        return true;
    }
    let mut found = false;
    let _ = walk_children(e, &mut |c| {
        if expr_contains_node(c, target) {
            found = true;
        }
        Ok(())
    });
    found
}

/// For `UPDATE ... FROM` / `DELETE ... USING`: enumerates the extra tables in
/// `from`, resolving the target row's columns through `target` (as the outer
/// scope), and invokes `on_match` with a combined lookup for the FIRST joined
/// row that satisfies `where_clause`. Returns whether any match was found.
#[allow(clippy::too_many_arguments)]
pub fn first_from_match<'a>(
    storage: &'a Storage,
    from: &'a FromClause<'a>,
    txid: u32,
    where_clause: Option<&'a Expr<'a>>,
    arena: &'a Arena,
    params: &[Datum<'a>],
    target: &dyn ColumnLookup<'a>,
    on_match: &mut dyn FnMut(&dyn ColumnLookup<'a>) -> Result<(), SqlError>,
) -> Result<bool, SqlError> {
    let scope = QueryScope::resolve_exec(storage, from, txid, arena, params)?;
    let subs = subquery_hooks(&[where_clause], storage, txid, arena, params)?;
    let hooks = EvalHooks { group: None, aggs: None, subs: Some(&subs) , windows: None, catalog: None, srf_index: None };
    let mut found = false;
    scan_source(
        storage,
        &scope,
        from,
        txid,
        where_clause,
        arena,
        params,
        &hooks,
        Some(target),
        &mut |jr| {
            let chained_row = Chained { inner: jr, outer: Some(target) };
            on_match(&chained_row)?;
            found = true;
            Ok(false) // stop at the first match (PostgreSQL uses one arbitrary row)
        },
    )?;
    Ok(found)
}

fn arena_full() -> SqlError {
    sql_err!(
        sqlstate::PROGRAM_LIMIT_EXCEEDED,
        "query result exceeds the statement arena"
    )
}

/// Public view-DML rewriting uses this for arena-exhaustion.
pub fn arena_full_pub() -> SqlError {
    arena_full()
}
