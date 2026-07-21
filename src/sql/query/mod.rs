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
use crate::storage::{ColumnMeta, SqlName, Storage, TableDef, MAX_COLUMNS};

use super::ast::{
    Expr, FrameBound, FromClause, OrderBy, Select, SelectItem, SetTree, TableRef,
    WindowFrame,
};
use super::eval::{
    compare_datums, eval_full, sqlstate, ColumnLookup, EvalHooks, SqlError, SubqueryValues,
};
use super::exec::{describe_items, MAX_PROJ};
use super::types::{ColDesc, ColType, Datum};

mod setops;
pub use setops::set_query;
use setops::{describe_set_body, materialize_set_body};

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
const MAX_WINDOWS: usize = 16;
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
fn fromless_aggregate_hooks<'a, R: ColumnLookup<'a>>(
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
    let Expr::Int(n) = expression else {
        return super::exec::resolve_order_expr_pub(expression, items);
    };
    let index = *n;
    let position_error =
        || sql_err!("42P10", "ORDER BY position {} is not in select list", index);
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


/// Expands a query's non-recursive `WITH` CTEs into derived tables: every
/// FROM reference to a CTE name (in the body and in nested subqueries) is
/// rewritten to `(cte_query) alias`, so the ordinary derived-table executor
/// runs the whole thing. Returns the CTE-free select. A no-operator when there are
/// no CTEs. Each CTE is substituted against the CTEs declared before it.
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
fn collect_aggs<'a>(
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
fn collect_windows<'a>(
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
        Expr::Like { operand, pattern, negated, case_insensitive } => alloc(Expr::Like {
            operand: rewrite(operand)?,
            pattern: rewrite(pattern)?,
            negated: *negated,
            case_insensitive: *case_insensitive,
        }),
        Expr::Match { operand, pattern, negated, case_insensitive } => alloc(Expr::Match {
            operand: rewrite(operand)?,
            pattern: rewrite(pattern)?,
            negated: *negated,
            case_insensitive: *case_insensitive,
        }),
        Expr::Case { operand, whens, otherwise } => {
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
            alloc(Expr::Case { operand, whens, otherwise })
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

/// Windows over a grouped query: PostgreSQL evaluates window functions after
/// GROUP BY / HAVING, over the grouped rows. Rewrites the statement into the
/// equivalent two-level form — an inner grouped select exposing every
/// grouping key and aggregate as a named column, and an outer select (window
/// calls, DISTINCT, ORDER BY, LIMIT) over it as a derived table — so the
/// existing grouped and window executors compose.
/// Walks an expression tree collecting subquery nodes.
fn collect_subqueries<'a>(
    expression: &'a Expr<'a>,
    out: &mut [Option<&'a Expr<'a>>; MAX_SUBQUERIES],
    n: &mut usize,
) -> Result<(), SqlError> {
    if matches!(
        expression,
        Expr::Subquery(_) | Expr::InSubquery { .. } | Expr::Exists(_) | Expr::ArraySubquery(_)
    ) {
        if out[..*n].iter().any(|e| core::ptr::eq(e.expect("set"), expression)) {
            return Ok(());
        }
        if *n == MAX_SUBQUERIES {
            return Err(sql_err!(sqlstate::PROGRAM_LIMIT_EXCEEDED, "too many subqueries in one query"));
        }
        out[*n] = Some(expression);
        *n += 1;
        // The operand of IN (SELECT ..) may itself contain subqueries.
        if let Expr::InSubquery { operand, .. } = expression {
            collect_subqueries(operand, out, n)?;
        }
        return Ok(());
    }
    walk_children(expression, &mut |child| collect_subqueries(child, out, n))
}

fn walk_children<'a>(
    expression: &'a Expr<'a>,
    f: &mut dyn FnMut(&'a Expr<'a>) -> Result<(), SqlError>,
) -> Result<(), SqlError> {
    match expression {
        Expr::Unary { operand, .. }
        | Expr::Cast { operand, .. }
        | Expr::IsNull { operand, .. } => f(operand),
        Expr::Binary { left, right, .. } => {
            f(left)?;
            f(right)
        }
        Expr::Call { args, .. } => {
            for a in *args {
                f(a)?;
            }
            Ok(())
        }
        Expr::InList { operand, list, .. } => {
            f(operand)?;
            for e in *list {
                f(e)?;
            }
            Ok(())
        }
        Expr::Between { operand, low, high, .. } => {
            f(operand)?;
            f(low)?;
            f(high)
        }
        Expr::Like { operand, pattern, .. } | Expr::Match { operand, pattern, .. } => {
            f(operand)?;
            f(pattern)
        }
        Expr::Case { operand, whens, otherwise } => {
            if let Some(o) = operand {
                f(o)?;
            }
            for (c, r) in *whens {
                f(c)?;
                f(r)?;
            }
            if let Some(o) = otherwise {
                f(o)?;
            }
            Ok(())
        }
        Expr::InSubquery { operand, .. } => f(operand),
        // A quantified comparison's array side may be a collected subquery.
        Expr::AnyAll { operand, array, .. } => {
            f(operand)?;
            f(array)
        }
        Expr::Field { base, .. } => f(base),
        _ => Ok(()),
    }
}

/// Pre-evaluates every (uncorrelated) subquery in the statement and stores
/// the results in the arena for hook-based lookup during evaluation.
#[allow(clippy::too_many_arguments)]
pub fn prepare_subqueries<'a>(
    exprs: &[Option<&'a Expr<'a>>],
    storage: &'a Storage,
    txid: u32,
    arena: &'a Arena,
    params: &[Datum<'a>],
    depth: u32,
    outer: Option<&dyn ColumnLookup<'a>>,
) -> Result<SubqueryValues<'a, 'a>, SqlError> {
    let mut nodes: [Option<&Expr>; MAX_SUBQUERIES] = [None; MAX_SUBQUERIES];
    let mut n = 0;
    for expression in exprs.iter().flatten() {
        collect_subqueries(expression, &mut nodes, &mut n)?;
    }
    eval_subquery_nodes(&nodes[..n], storage, txid, arena, params, depth, outer)
}

/// Evaluates a set of already-collected subquery nodes (scalar, IN, or
/// EXISTS) into arena-backed [`SubqueryValues`] keyed by node identity.
/// EXISTS results are stored as boolean scalars.
#[allow(clippy::too_many_arguments)]
fn eval_subquery_nodes<'a>(
    nodes: &[Option<&'a Expr<'a>>],
    storage: &'a Storage,
    txid: u32,
    arena: &'a Arena,
    params: &[Datum<'a>],
    depth: u32,
    outer: Option<&dyn ColumnLookup<'a>>,
) -> Result<SubqueryValues<'a, 'a>, SqlError> {
    let mut scalars_tmp: [(*const Expr, Datum, Datum); MAX_SUBQUERIES] =
        [(core::ptr::null(), Datum::Null, Datum::Null); MAX_SUBQUERIES];
    let mut lists_tmp: [(*const Expr, &[Datum], bool, Datum); MAX_SUBQUERIES] =
        [(core::ptr::null(), &[], false, Datum::Null); MAX_SUBQUERIES];
    let (mut n_scalars, mut n_lists) = (0, 0);
    for node in nodes.iter().flatten() {
        match node {
            Expr::Subquery(select) => {
                let (values, _, witness) =
                    run_subquery(select, storage, txid, arena, params, depth, outer)?;
                if values.len() > 1 {
                    return Err(sql_err!(
                        "21000",
                        "more than one row returned by a subquery used as an expression"
                    ));
                }
                let v = values.first().copied().unwrap_or(Datum::Null);
                scalars_tmp[n_scalars] = (*node as *const _, v, witness);
                n_scalars += 1;
            }
            Expr::Exists(select) => {
                let found = subquery_exists(select, storage, txid, arena, params, depth, outer)?;
                scalars_tmp[n_scalars] = (*node as *const _, Datum::Bool(found), Datum::Bool(false));
                n_scalars += 1;
            }
            Expr::ArraySubquery(select) => {
                let (values, _, witness) =
                    run_subquery(select, storage, txid, arena, params, depth, outer)?;
                let v = build_array_scalar(values, &witness, arena)?;
                scalars_tmp[n_scalars] = (*node as *const _, v, v);
                n_scalars += 1;
            }
            Expr::InSubquery { select, .. } => {
                let (values, saw_null, witness) =
                    run_subquery(select, storage, txid, arena, params, depth, outer)?;
                lists_tmp[n_lists] = (*node as *const _, values, saw_null, witness);
                n_lists += 1;
            }
            _ => unreachable!("collector only stores subquery nodes"),
        }
    }
    let scalars = arena
        .alloc_slice_copy(&scalars_tmp[..n_scalars])
        .map_err(|_| arena_full())?;
    let lists = arena
        .alloc_slice_copy(&lists_tmp[..n_lists])
        .map_err(|_| arena_full())?;
    Ok(SubqueryValues { scalars, lists })
}

/// Runs a subquery only to determine whether it yields any row (EXISTS).
/// Stops at the first matching row. `outer` supplies correlated columns.
#[allow(clippy::too_many_arguments)]
fn subquery_exists<'a>(
    select: &'a Select<'a>,
    storage: &'a Storage,
    txid: u32,
    arena: &'a Arena,
    params: &[Datum<'a>],
    depth: u32,
    outer: Option<&dyn ColumnLookup<'a>>,
) -> Result<bool, SqlError> {
    if depth == 0 {
        return Err(sql_err!("54001", "subqueries nested too deeply"));
    }
    if let Some(tree) = select.set_body {
        let (vals, _, _) = run_set_subquery(tree, select, storage, txid, arena, params)?;
        return Ok(!vals.is_empty());
    }
    if !select.group_by.is_empty() || select.having.is_some() || select.distinct {
        // Grouped/DISTINCT EXISTS: the row-source executor already handles
        // grouping, HAVING, and DISTINCT — existence is whether it emits.
        let mut found = false;
        select_into_rows(storage, txid, select, arena, params, outer, &mut |_| {
            found = true;
            Ok(())
        })?;
        return Ok(found);
    }
    // The projection list of EXISTS is irrelevant (only row presence matters),
    // but its expressions may carry subqueries; prepare them for the scan.
    let mut item_exprs: [Option<&Expr>; MAX_PROJ] = [None; MAX_PROJ];
    let mut n_items = 0;
    for item in select.items {
        if let SelectItem::Expr { expression, .. } = item {
            item_exprs[n_items] = Some(expression);
            n_items += 1;
        }
    }
    let inner_subs = prepare_subqueries(
        &{
            let mut e = item_exprs;
            // WHERE joins the set of expressions whose subqueries we prepare.
            if n_items < MAX_PROJ {
                e[n_items] = select.where_clause;
            }
            e
        },
        storage,
        txid,
        arena,
        params,
        depth - 1,
        outer,
    )?;
    let hooks = EvalHooks { group: None, aggs: None, subs: Some(&inner_subs) , windows: None, catalog: None, srf_index: None };

    let Some(from) = &select.from else {
        // FROM-less: an aggregate query yields its one output row even over
        // zero input rows (WHERE false), so EXISTS is true unless HAVING
        // filters it. A plain query yields one row when WHERE holds.
        let mut agg_nodes: [(*const Expr, &Expr); MAX_AGGS] =
            [(core::ptr::null(), &Expr::Null); MAX_AGGS];
        let mut n_aggs = 0;
        for item in select.items {
            if let SelectItem::Expr { expression, .. } = item {
                collect_aggs(expression, &mut agg_nodes, &mut n_aggs)?;
            }
        }
        if let Some(h) = select.having {
            collect_aggs(h, &mut agg_nodes, &mut n_aggs)?;
        }
        if n_aggs > 0 || select.having.is_some() {
            let base = Chained { inner: &super::eval::NoColumns, outer };
            let hook_data = fromless_aggregate_hooks(
                select, &agg_nodes[..n_aggs], arena, params, &base, &hooks,
            )?;
            return Ok(hook_data.is_some());
        }
        if let Some(w) = select.where_clause {
            let base = Chained { inner: &super::eval::NoColumns, outer };
            return Ok(matches!(eval_full(w, arena, params, &base, &hooks)?, Datum::Bool(true)));
        }
        return Ok(true);
    };
    let scope = QueryScope::resolve_exec(storage, from, txid, arena, params)?;
    let mut found = false;
    scan_source(
        storage,
        &scope,
        from,
        txid,
        select.where_clause,
        arena,
        params,
        &hooks,
        outer,
        &mut |_| {
            found = true;
            Ok(false) // stop at the first row
        },
    )?;
    Ok(found)
}

/// A chain of query scopes from innermost outward, used to decide whether a
/// subquery references a column belonging to an enclosing query.
struct ScopeChain<'s, 'd> {
    scope: Option<&'s QueryScope<'d>>,
    parent: Option<&'s ScopeChain<'s, 'd>>,
}

impl ScopeChain<'_, '_> {
    /// True if the name resolves at this scope or any enclosing scope.
    fn resolves(&self, q: Option<&str>, name: &str) -> bool {
        if self.scope.is_some_and(|s| s.find_column(q, name).is_ok()) {
            return true;
        }
        self.parent.is_some_and(|p| p.resolves(q, name))
    }
}

/// Whether a top-level subquery node references any column from the enclosing
/// query — i.e. is correlated and must be re-evaluated per outer row. A node
/// unresolvable against its own (and any nested subquery's) scope is treated
/// as correlated; false positives only cost a redundant per-row evaluation.
fn subquery_node_correlated<'a>(node: &'a Expr<'a>, storage: &'a Storage, arena: &'a Arena) -> bool {
    let select = match node {
        Expr::Subquery(s) | Expr::InSubquery { select: s, .. } | Expr::Exists(s)
        | Expr::ArraySubquery(s) => s,
        _ => return false,
    };
    let scope = select
        .from
        .as_ref()
        .and_then(|f| QueryScope::resolve_schema(storage, f, 0, arena).ok());
    let chain = ScopeChain { scope: scope.as_ref(), parent: None };
    select_has_outer_ref(select, &chain, storage, arena)
}

/// Whether any column in this select (WHERE or projection) fails to resolve
/// within `chain` (which already includes this select's own scope).
fn select_has_outer_ref<'a>(
    select: &'a Select<'a>,
    chain: &ScopeChain,
    storage: &'a Storage,
    arena: &'a Arena,
) -> bool {
    if select.where_clause.is_some_and(|w| expr_has_outer_ref(w, chain, storage, arena)) {
        return true;
    }
    if select.having.is_some_and(|h| expr_has_outer_ref(h, chain, storage, arena)) {
        return true;
    }
    if select.group_by.iter().any(|g| expr_has_outer_ref(g, chain, storage, arena)) {
        return true;
    }
    select.items.iter().any(|it| match it {
        SelectItem::Expr { expression, .. } => expr_has_outer_ref(expression, chain, storage, arena),
        _ => false,
    })
}

/// Whether any column reference in `expression` resolves only in an enclosing scope
/// beyond `chain`. Nested subqueries push their own scope onto the chain, so a
/// column they provide themselves does not count as an outer reference.
fn expr_has_outer_ref<'a>(
    expression: &'a Expr<'a>,
    chain: &ScopeChain,
    storage: &'a Storage,
    arena: &'a Arena,
) -> bool {
    match expression {
        Expr::Column { qualifier, name } => !chain.resolves(*qualifier, name),
        Expr::Subquery(s) | Expr::Exists(s) => {
            let sscope = s
                .from
                .as_ref()
                .and_then(|f| QueryScope::resolve_schema(storage, f, 0, arena).ok());
            let child = ScopeChain { scope: sscope.as_ref(), parent: Some(chain) };
            select_has_outer_ref(s, &child, storage, arena)
        }
        Expr::InSubquery { operand, select, .. } => {
            let sscope = select
                .from
                .as_ref()
                .and_then(|f| QueryScope::resolve_schema(storage, f, 0, arena).ok());
            let child = ScopeChain { scope: sscope.as_ref(), parent: Some(chain) };
            select_has_outer_ref(select, &child, storage, arena)
                || expr_has_outer_ref(operand, chain, storage, arena)
        }
        _ => {
            let mut found = false;
            let _ = walk_children(expression, &mut |c| {
                if expr_has_outer_ref(c, chain, storage, arena) {
                    found = true;
                }
                Ok(())
            });
            found
        }
    }
}

/// Pre-evaluated uncorrelated subqueries plus the list of correlated subquery
/// nodes that must be re-evaluated per outer row.
struct OuterSubs<'a> {
    base: SubqueryValues<'a, 'a>,
    correlated: &'a [&'a Expr<'a>],
}

/// Splits a query's subqueries into uncorrelated (evaluated once here) and
/// correlated (deferred to per-row evaluation during the scan).
fn prepare_outer_subqueries<'a>(
    exprs: &[Option<&'a Expr<'a>>],
    storage: &'a Storage,
    txid: u32,
    arena: &'a Arena,
    params: &[Datum<'a>],
) -> Result<OuterSubs<'a>, SqlError> {
    let mut nodes: [Option<&Expr>; MAX_SUBQUERIES] = [None; MAX_SUBQUERIES];
    let mut n = 0;
    for expression in exprs.iter().flatten() {
        collect_subqueries(expression, &mut nodes, &mut n)?;
    }
    let mut uncorr: [Option<&Expr>; MAX_SUBQUERIES] = [None; MAX_SUBQUERIES];
    let mut n_un = 0;
    let mut corr: [Option<&Expr>; MAX_SUBQUERIES] = [None; MAX_SUBQUERIES];
    let mut n_corr = 0;
    for node in nodes[..n].iter().flatten() {
        if subquery_node_correlated(node, storage, arena) {
            corr[n_corr] = Some(*node);
            n_corr += 1;
        } else {
            uncorr[n_un] = Some(*node);
            n_un += 1;
        }
    }
    let base =
        eval_subquery_nodes(&uncorr[..n_un], storage, txid, arena, params, SUBQUERY_DEPTH, None)?;
    let correlated = arena
        .alloc_slice_with(n_corr, |i| corr[i].expect("set"))
        .map_err(|_| arena_full())?;
    Ok(OuterSubs { base, correlated })
}

/// Builds per-outer-row [`SubqueryValues`] by merging the pre-evaluated
/// uncorrelated results with correlated subqueries evaluated against `outer`.
/// The merged arrays live in caller-provided stack scratch (no arena growth
/// for the bookkeeping; only the subquery result values themselves use the
/// arena).
#[allow(clippy::too_many_arguments)]
fn merge_correlated<'a, 'b>(
    correlated: &[&'a Expr<'a>],
    base: &SubqueryValues<'a, 'a>,
    outer: &dyn ColumnLookup<'a>,
    storage: &'a Storage,
    txid: u32,
    arena: &'a Arena,
    params: &[Datum<'a>],
    scalars: &'b mut [(*const Expr<'a>, Datum<'a>, Datum<'a>); MAX_SUBQUERIES],
    lists: &'b mut [(*const Expr<'a>, &'a [Datum<'a>], bool, Datum<'a>); MAX_SUBQUERIES],
) -> Result<SubqueryValues<'b, 'a>, SqlError> {
    let mut ns = 0;
    for (p, v, w) in base.scalars {
        scalars[ns] = (*p, *v, *w);
        ns += 1;
    }
    let mut nl = 0;
    for (p, l, sn, w) in base.lists {
        lists[nl] = (*p, *l, *sn, *w);
        nl += 1;
    }
    for node in correlated {
        match node {
            Expr::Subquery(select) => {
                let (values, _, witness) =
                    run_subquery(select, storage, txid, arena, params, SUBQUERY_DEPTH, Some(outer))?;
                if values.len() > 1 {
                    return Err(sql_err!(
                        "21000",
                        "more than one row returned by a subquery used as an expression"
                    ));
                }
                scalars[ns] = (*node as *const _, values.first().copied().unwrap_or(Datum::Null), witness);
                ns += 1;
            }
            Expr::Exists(select) => {
                let found =
                    subquery_exists(select, storage, txid, arena, params, SUBQUERY_DEPTH, Some(outer))?;
                scalars[ns] = (*node as *const _, Datum::Bool(found), Datum::Bool(false));
                ns += 1;
            }
            Expr::ArraySubquery(select) => {
                let (values, _, witness) =
                    run_subquery(select, storage, txid, arena, params, SUBQUERY_DEPTH, Some(outer))?;
                let v = build_array_scalar(values, &witness, arena)?;
                scalars[ns] = (*node as *const _, v, v);
                ns += 1;
            }
            Expr::InSubquery { select, .. } => {
                let (values, saw_null, witness) =
                    run_subquery(select, storage, txid, arena, params, SUBQUERY_DEPTH, Some(outer))?;
                lists[nl] = (*node as *const _, values, saw_null, witness);
                nl += 1;
            }
            _ => unreachable!("correlated list holds only subquery nodes"),
        }
    }
    Ok(SubqueryValues { scalars: &scalars[..ns], lists: &lists[..nl] })
}

/// A representative zero value of a column type, used to coerce an IN operand
/// to the subquery's result type even over an empty or all-NULL set. Text /
/// bytea / numeric use a text witness, which `coerce_unknown` leaves untouched
/// (no spurious error), matching that these accept an unknown literal as-is.
fn type_witness(ct: ColType) -> Datum<'static> {
    match ct {
        ColType::Bool => Datum::Bool(false),
        ColType::Int2 | ColType::Int4 => Datum::Int4(0),
        ColType::Int8 => Datum::Int8(0),
        ColType::Time => Datum::Time(0),
        ColType::Timetz => Datum::Timetz(0, 0),
        ColType::Interval => Datum::Interval(crate::sql::types::Interval { months: 0, days: 0, micros: 0 }),
        ColType::Json => Datum::Json { text: "null", jsonb: false },
        ColType::Jsonb => Datum::Json { text: "null", jsonb: true },
        ColType::Array(element) => Datum::Array { element, raw: &[0, 0] },
        ColType::Float4 | ColType::Float8 => Datum::Float8(0.0),
        ColType::Date => Datum::Date(0),
        ColType::Timestamp => Datum::Timestamp(0),
        ColType::Timestamptz => Datum::Timestamptz(0),
        ColType::Uuid => Datum::Uuid([0; 16]),
        ColType::Text | ColType::Varchar | ColType::Bpchar | ColType::Bytea | ColType::Numeric => {
            Datum::Text("")
        }
        ColType::Range(kind) => Datum::Range { text: "empty", kind },
        ColType::Bit { varying } => Datum::Bit { bits: "", varying },
        ColType::Multirange(kind) => Datum::Multirange { text: "{}", kind },
    }
}

/// The type witness for a subquery's single result column, inferred from its
/// projection expression. Falls back to a text witness on any inference error
/// (harmless — the real evaluation surfaces genuine errors).
fn subquery_witness(item: &Expr, scope: Option<&QueryScope>) -> Datum<'static> {
    let inferred = match scope {
        Some(s) => super::exec::infer_type_res(item, &ScopeCols(s)),
        None => super::exec::infer_type_res(item, &super::exec::NoCols),
    };
    let ct = inferred
        .ok()
        .and_then(|(o, _)| super::exec::coltype_of_oid(o))
        .unwrap_or(ColType::Text);
    type_witness(ct)
}

/// Executes a subquery to a value list: exactly one select item, full
/// WHERE/aggregate support, no grouping/ordering (irrelevant for IN, and a
/// scalar has at most one row). Also returns a type witness for the result
/// column (see [`type_witness`]); scalar callers ignore it.
#[allow(clippy::too_many_arguments)]
fn run_subquery<'a>(
    select: &'a Select<'a>,
    storage: &'a Storage,
    txid: u32,
    arena: &'a Arena,
    params: &[Datum<'a>],
    depth: u32,
    outer: Option<&dyn ColumnLookup<'a>>,
) -> Result<(&'a [Datum<'a>], bool, Datum<'a>), SqlError> {
    if depth == 0 {
        return Err(sql_err!("54001", "subqueries nested too deeply"));
    }
    if let Some(tree) = select.set_body {
        return run_set_subquery(tree, select, storage, txid, arena, params);
    }
    if select.items.len() != 1 {
        return Err(sql_err!(sqlstate::SYNTAX_ERROR, "subquery must return exactly one column"));
    }
    // `SELECT *` is accepted when the source has exactly one column (resolved
    // below); until then a placeholder stands in (a wildcard carries no
    // subqueries or aggregates of its own).
    let wildcard = matches!(&select.items[0], SelectItem::Wildcard);
    let table_star = match &select.items[0] {
        SelectItem::TableWildcard(q) => Some(*q),
        _ => None,
    };
    let item: &Expr = match &select.items[0] {
        SelectItem::Expr { expression, .. } => expression,
        SelectItem::Wildcard | SelectItem::TableWildcard(_) | SelectItem::RecordStar(_) => {
            &Expr::Null
        }
    };
    // A window function needs rows materialized before it can be computed, so
    // its body belongs to the row-source executor just as a grouped one does.
    let mut win_probe: [&Expr; MAX_WINDOWS] = [&Expr::Null; MAX_WINDOWS];
    let mut n_win_probe = 0;
    collect_windows(item, &mut win_probe, &mut n_win_probe)?;
    for ob in select.order_by {
        collect_windows(ob.expression, &mut win_probe, &mut n_win_probe)?;
    }
    if !select.group_by.is_empty() || select.having.is_some() || select.distinct || n_win_probe > 0
    {
        // Grouped/DISTINCT/windowed subquery: the row-source executor already
        // handles grouping, HAVING, DISTINCT, and windows; collect its single
        // output column.
        let mut count = 0usize;
        select_into_rows(storage, txid, select, arena, params, outer, &mut |_| {
            count += 1;
            Ok(())
        })?;
        let out = arena.alloc_slice_with(count, |_| Datum::Null).map_err(|_| arena_full())?;
        let mut at = 0usize;
        let mut any_null = false;
        select_into_rows(storage, txid, select, arena, params, outer, &mut |vals| {
            if vals.len() != 1 {
                return Err(sql_err!(sqlstate::SYNTAX_ERROR, "subquery must return only one column"));
            }
            out[at] = vals[0];
            any_null |= vals[0].is_null();
            at += 1;
            Ok(())
        })?;
        let own_scope = select
            .from
            .as_ref()
            .and_then(|f| QueryScope::resolve_schema(storage, f, txid, arena).ok());
        let witness = match own_scope {
            Some(ref s) if !wildcard && table_star.is_none() => subquery_witness(item, Some(s)),
            _ => out.first().copied().unwrap_or(Datum::Null),
        };
        return Ok((&*out, any_null, witness));
    }

    // Inner subqueries first.
    let inner_subs = prepare_subqueries(
        &[Some(item), select.where_clause],
        storage,
        txid,
        arena,
        params,
        depth - 1,
        outer,
    )?;
    let hooks = EvalHooks {
        group: None,
        aggs: None,
        subs: Some(&inner_subs),
        windows: None, catalog: None, srf_index: None };

    let Some(from) = &select.from else {
        if wildcard {
            return Err(sql_err!(sqlstate::SYNTAX_ERROR, "SELECT * with no tables specified is not valid"));
        }
        // FROM-less: one row (outer columns still visible if correlated).
        // Aggregates fold over that single virtual row (zero when WHERE is
        // false) and still yield their one output row.
        let mut agg_nodes: [(*const Expr, &Expr); MAX_AGGS] =
            [(core::ptr::null(), &Expr::Null); MAX_AGGS];
        let mut n_aggs = 0;
        collect_aggs(item, &mut agg_nodes, &mut n_aggs)?;
        if n_aggs > 0 {
            let Some((ptrs, values)) =
                fromless_aggregate_hooks(select, &agg_nodes[..n_aggs], arena, params, &Chained { inner: &super::eval::NoColumns, outer }, &hooks)?
            else {
                return Ok((&[], false, subquery_witness(item, None)));
            };
            let agg_hooks = EvalHooks { aggs: Some((ptrs, values)), ..hooks };
            let base = Chained { inner: &super::eval::NoColumns, outer };
            let v = eval_full(item, arena, params, &base, &agg_hooks)?;
            let out = arena.alloc_slice_copy(&[v]).map_err(|_| arena_full())?;
            return Ok((&*out, v.is_null(), subquery_witness(item, None)));
        }
        let base = Chained { inner: &super::eval::NoColumns, outer };
        if let Some(w) = select.where_clause
            && !where_passes(w, arena, params, &base, &hooks)?
        {
            return Ok((&[], false, subquery_witness(item, None)));
        }
        let v = eval_full(item, arena, params, &base, &hooks)?;
        let out = arena.alloc_slice_copy(&[v]).map_err(|_| arena_full())?;
        return Ok((&*out, v.is_null(), subquery_witness(item, None)));
    };
    let scope = QueryScope::resolve_exec(storage, from, txid, arena, params)?;

    // `SELECT *` is a single-column subquery only if the source is exactly one
    // column; expand it to that column so the row-value path below applies.
    let item: &Expr = if wildcard {
        if scope.star_columns() != 1 {
            return Err(sql_err!(sqlstate::SYNTAX_ERROR, "subquery must return only one column"));
        }
        let name = scope.output_name(scope.star_entry(0));
        arena
            .alloc(Expr::Column { qualifier: None, name })
            .map_err(|_| arena_full())?
    } else if let Some(q) = table_star {
        let t = scope.table_index(q)?;
        let def = scope.defs[t].expect("resolved");
        if def.n_columns != 1 {
            return Err(sql_err!(sqlstate::SYNTAX_ERROR, "subquery must return only one column"));
        }
        arena
            .alloc(Expr::Column { qualifier: Some(q), name: def.columns()[0].name.as_str() })
            .map_err(|_| arena_full())?
    } else {
        item
    };

    // Aggregate subquery: one row.
    let mut agg_nodes: [(*const Expr, &Expr); MAX_AGGS] =
        [(core::ptr::null(), &Expr::Null); MAX_AGGS];
    let mut n_aggs = 0;
    collect_aggs(item, &mut agg_nodes, &mut n_aggs)?;
    if n_aggs > 0 {
        let agg_values = fold_aggregates(
            storage,
            &scope,
            from,
            txid,
            select.where_clause,
            &agg_nodes[..n_aggs],
            arena,
            params,
            &hooks,
            outer,
        )?;
        let ptrs = arena
            .alloc_slice_with(n_aggs, |i| agg_nodes[i].0)
            .map_err(|_| arena_full())?;
        let agg_hooks = EvalHooks {
            group: None,
            aggs: Some((&*ptrs, agg_values)),
            subs: hooks.subs,
        windows: None, catalog: None, srf_index: None };
        let schema = ScopeSchema(&scope);
        let base = Chained { inner: &schema, outer };
        let v = eval_full(item, arena, params, &base, &agg_hooks)?;
        let out = arena.alloc_slice_copy(&[v]).map_err(|_| arena_full())?;
        return Ok((&*out, v.is_null(), subquery_witness(item, Some(&scope))));
    }

    // Plain scan: collect item values (and ORDER BY keys). Two passes (count
    // then fill), then sort and apply OFFSET/LIMIT so a subquery's own ORDER BY
    // / LIMIT is honored (element order matters for ARRAY(...) and scalar).
    let n_keys = select.order_by.len();
    let mut count = 0usize;
    scan_source(
        storage,
        &scope,
        from,
        txid,
        select.where_clause,
        arena,
        params,
        &hooks,
        outer,
        &mut |_| {
            count += 1;
            Ok(true)
        },
    )?;
    let vals = arena.alloc_slice_with(count, |_| Datum::Null).map_err(|_| arena_full())?;
    let keys = arena.alloc_slice_with(count * n_keys, |_| Datum::Null).map_err(|_| arena_full())?;
    let mut at = 0usize;
    scan_source(
        storage,
        &scope,
        from,
        txid,
        select.where_clause,
        arena,
        params,
        &hooks,
        outer,
        &mut |row| {
            let chained_row = Chained { inner: row, outer };
            vals[at] = eval_full(item, arena, params, &chained_row, &hooks)?;
            for (k, o) in select.order_by.iter().enumerate() {
                // A positional `ORDER BY 1` sorts by the single output column.
                let key = match o.expression {
                    Expr::Int(_) => vals[at],
                    e => eval_full(e, arena, params, &chained_row, &hooks)?,
                };
                keys[at * n_keys + k] = key;
            }
            at += 1;
            Ok(true)
        },
    )?;

    // Stable insertion sort of row indices by the ORDER BY keys.
    let order = arena.alloc_slice_with(count, |i| i).map_err(|_| arena_full())?;
    if n_keys > 0 {
        for x in 1..count {
            let mut y = x;
            while y > 0 {
                let a = &keys[order[y - 1] * n_keys..order[y - 1] * n_keys + n_keys];
                let b = &keys[order[y] * n_keys..order[y] * n_keys + n_keys];
                if cmp_key_rows(a, b, select.order_by) == core::cmp::Ordering::Greater {
                    order.swap(y - 1, y);
                    y -= 1;
                } else {
                    break;
                }
            }
        }
    }

    // Apply OFFSET/LIMIT over the ordered rows.
    let offset = super::exec::eval_offset_pub(select.offset, arena, params)? as usize;
    let limit = super::exec::eval_limit_pub(select.limit, arena, params)?;
    let start = offset.min(count);
    let n = ((count - start) as u64).min(limit) as usize;
    let mut saw_null = false;
    let out = arena
        .alloc_slice_with(n, |i| {
            let v = vals[order[start + i]];
            if v.is_null() {
                saw_null = true;
            }
            v
        })
        .map_err(|_| arena_full())?;
    Ok((&*out, saw_null, subquery_witness(item, Some(&scope))))
}

/// Runs a set-operation query (UNION / INTERSECT / EXCEPT) in subquery position,
/// yielding its single output column as datums. Mirrors [`set_query`]'s type
/// unification and row combining, then decodes the lone column back to datums so
/// scalar / IN callers can consume them. Correlated columns are not visible to a
/// set-operation body (each leaf is materialized independently); an unresolved
/// reference surfaces loudly as a missing-column error from the leaf itself.
fn run_set_subquery<'a>(
    tree: &'a SetTree<'a>,
    outer_select: &'a Select<'a>,
    storage: &'a Storage,
    txid: u32,
    arena: &'a Arena,
    params: &[Datum<'a>],
) -> Result<(&'a [Datum<'a>], bool, Datum<'a>), SqlError> {
    let (rows, target, n_cols) = materialize_set_body(storage, txid, tree, arena, params)?;
    if n_cols != 1 {
        return Err(sql_err!(sqlstate::SYNTAX_ERROR, "subquery must return only one column"));
    }
    let offset = super::exec::eval_offset_pub(outer_select.offset, arena, params)?;
    let limit = super::exec::eval_limit_pub(outer_select.limit, arena, params)?;
    let start = (offset as usize).min(rows.len());
    let n = ((rows.len() - start) as u64).min(limit) as usize;
    let mut saw_null = false;
    let out = arena
        .alloc_slice_with(n, |i| {
            let v = super::exec::decode_projected_pub(rows[start + i], 0);
            if v.is_null() {
                saw_null = true;
            }
            v
        })
        .map_err(|_| arena_full())?;
    Ok((&*out, saw_null, type_witness(target[0])))
}

/// Builds the `Datum::Array` for an `ARRAY(subquery)` constructor from the
/// subquery's single-column `values`. The element type comes from the column's
/// type `witness` (so an empty subquery still yields a correctly-typed empty
/// array); each value is coerced to it before encoding.
fn build_array_scalar<'a>(
    values: &[Datum<'a>],
    witness: &Datum<'a>,
    arena: &'a Arena,
) -> Result<Datum<'a>, SqlError> {
    let element = super::types::ArrElem::from_datum(witness)
        .or_else(|| values.iter().find_map(super::types::ArrElem::from_datum))
        .unwrap_or(super::types::ArrElem::Text);
    let ct = element.to_coltype();
    let buffer = arena
        .alloc_slice_with(values.len(), |i| values[i])
        .map_err(|_| arena_full())?;
    for v in buffer.iter_mut() {
        if !v.is_null() {
            *v = super::eval::cast_to(*v, ct, arena)?;
        }
    }
    Ok(Datum::Array { element, raw: super::array::build(buffer, arena)? })
}

/// Streams the source once, folding every aggregate node's state.
/// Returns per-node result datums in the arena.
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

/// Evaluates a WHERE/HAVING predicate against a row, returning whether the row
/// passes (NULL and FALSE both filter it out); errors on a non-boolean result.
const MAX_CONJUNCTS: usize = 32;

/// The highest 0-based table index in `scope` whose column this expression
/// references — i.e. the join level at which the expression becomes fully
/// bound. `None` if it references a column outside `scope` (a correlated/outer
/// reference), a subquery, an aggregate, or any construct this analysis does
/// not fully cover, in which case the conjunct is left for the final WHERE.
/// The set of table indices (as a bitmask) an expression references. `None` if
/// it contains a construct not analyzable for pushdown (subquery, aggregate, …).
fn expr_tables(expression: &Expr, scope: &QueryScope) -> Option<u16> {
    use Expr::*;
    match expression {
        Null | Bool(_) | Int(_) | Float(_) | NumericLit(_) | Str(_) | Param(_) => Some(0),
        Column { qualifier, name } => match scope.find_column(*qualifier, name).ok()? {
            ResolvedColumn::Table(t, _) => Some(1 << t),
            // Merged USING/NATURAL column: reads every contributing table.
            ResolvedColumn::Merged(m) => {
                let mc = &scope.merged[m];
                Some(mc.parts[..mc.n_parts].iter().fold(0u16, |mask, &(t, _)| mask | (1 << t)))
            }
        },
        Unary { operand, .. } | IsNull { operand, .. } | Cast { operand, .. } => {
            expr_tables(operand, scope)
        }
        Binary { left, right, .. } => Some(expr_tables(left, scope)? | expr_tables(right, scope)?),
        Between { operand, low, high, .. } => {
            Some(expr_tables(operand, scope)? | expr_tables(low, scope)? | expr_tables(high, scope)?)
        }
        Like { operand, pattern, .. } | Match { operand, pattern, .. } => {
            Some(expr_tables(operand, scope)? | expr_tables(pattern, scope)?)
        }
        InList { operand, list, .. } => {
            let mut m = expr_tables(operand, scope)?;
            for e in *list {
                m |= expr_tables(e, scope)?;
            }
            Some(m)
        }
        Call { args, over: None, .. } if !expression.is_aggregate() => {
            let mut m = 0;
            for a in *args {
                m |= expr_tables(a, scope)?;
            }
            Some(m)
        }
        _ => None,
    }
}

/// A cost-based execution order for a cross-join's tables (an identity order is
/// returned when reordering does not apply). PostgreSQL reorders joins by
/// selectivity; pos3ql's nested loop otherwise follows FROM order, so a table
/// with no predicate binding it to the already-joined tables (e.g. an
/// unconstrained table in the middle of the FROM list) multiplies the
/// intermediate product and turns a k-way join O(N^k). The greedy heuristic
/// picks, at each step, the remaining table that "unlocks" the most WHERE
/// conjuncts (its columns, together with the already-chosen tables, fully bind a
/// conjunct so pushdown can prune there), breaking ties by FROM order. This
/// keeps selective and equi-joined tables early and pushes unconstrained tables
/// last, without changing results (join order is free for inner/cross joins).
fn join_order(scope: &QueryScope, where_clause: Option<&Expr>) -> [usize; MAX_JOIN_TABLES] {
    let mut order = core::array::from_fn(|i| i);
    let n = scope.n;
    if n < 3 {
        return order;
    }
    // Collect the WHERE conjuncts' table masks (only analyzable ones).
    let mut masks = [0u16; MAX_CONJUNCTS];
    let mut n_masks = 0;
    if let Some(w) = where_clause {
        let mut conjunct: [&Expr; MAX_CONJUNCTS] = [w; MAX_CONJUNCTS];
        let mut nc = 0;
        let conjuncts: &[&Expr] =
            if flatten_and(w, &mut conjunct, &mut nc) { &conjunct[..nc] } else { core::slice::from_ref(&w) };
        for &c in conjuncts {
            if let Some(m) = expr_tables(c, scope)
                && n_masks < MAX_CONJUNCTS
            {
                masks[n_masks] = m;
                n_masks += 1;
            }
        }
    }
    let mut chosen_mask = 0u16;
    for slot in order.iter_mut().take(n) {
        // Among not-yet-chosen tables, pick the one unlocking the most conjuncts
        // (a conjunct is unlocked when the table is its last unbound one).
        let mut best = usize::MAX;
        let mut best_score = -1i32;
        for t in 0..n {
            if chosen_mask & (1 << t) != 0 {
                continue;
            }
            let after = chosen_mask | (1 << t);
            let mut score = 0i32;
            for &m in &masks[..n_masks] {
                if m & !chosen_mask == (1 << t) {
                    score += 1;
                }
                // Slight preference for being connected at all (bounds growth).
                if m & !after == 0 && m & (1 << t) != 0 {
                    score += 1;
                }
            }
            if score > best_score {
                best_score = score;
                best = t;
            }
        }
        *slot = best;
        chosen_mask |= 1 << best;
    }
    order
}

/// Evaluates one WHERE conjunct to a filter decision (NULL and FALSE both
/// exclude the row; a non-boolean is a type error).
fn conjunct_passes<'a>(
    e: &Expr<'a>,
    arena: &'a Arena,
    params: &[Datum<'a>],
    row: &impl ColumnLookup<'a>,
    hooks: &EvalHooks<'_, 'a>,
) -> Result<bool, SqlError> {
    match eval_full(e, arena, params, row, hooks)? {
        Datum::Bool(true) => Ok(true),
        Datum::Bool(false) | Datum::Null => Ok(false),
        _ => Err(sql_err!(
            sqlstate::DATATYPE_MISMATCH,
            "argument of WHERE must be type boolean"
        )),
    }
}

/// Flattens a top-level `AND` chain into `out`, returning the count, or `None`
/// if it would overflow (caller then evaluates the predicate whole).
fn flatten_and<'e, 'a>(e: &'e Expr<'a>, out: &mut [&'e Expr<'a>], n: &mut usize) -> bool {
    if let Expr::Binary { operator: super::ast::BinaryOp::And, left, right } = e {
        return flatten_and(left, out, n) && flatten_and(right, out, n);
    }
    if *n == out.len() {
        return false;
    }
    out[*n] = e;
    *n += 1;
    true
}

/// Whether an expression cannot raise a runtime error (so evaluating it early
/// as a WHERE filter is always safe). Conservative: any arithmetic (which can
/// divide by zero or overflow), cast, function call, CASE, or subquery counts
/// as potentially-erroring.
fn is_error_safe(e: &Expr) -> bool {
    use super::ast::{BinaryOp::*, UnaryOp};
    // A constant subexpression cannot raise a *runtime* error: PostgreSQL folds
    // it at plan time and `check_constant_errors` surfaces any error eagerly
    // there, so by the time a row is filtered it is known good. This lets a
    // constant-false conjunct (e.g. `-2.25 <> -2.25`, whose unary minus would
    // otherwise mark it unsafe) filter the row before an erroring sibling runs.
    if e.is_constant() {
        return true;
    }
    match e {
        Expr::Null | Expr::Bool(_) | Expr::Int(_) | Expr::Float(_) | Expr::NumericLit(_)
        | Expr::Str(_) | Expr::Column { .. } | Expr::Param(_) | Expr::DefaultMarker => true,
        Expr::Binary { operator, left, right } => match operator {
            Add | Sub | Mul | Div | Mod => false,
            _ => is_error_safe(left) && is_error_safe(right),
        },
        Expr::Unary { operator, operand } => matches!(operator, UnaryOp::Not) && is_error_safe(operand),
        Expr::IsNull { operand, .. } => is_error_safe(operand),
        Expr::InList { operand, list, .. } => {
            is_error_safe(operand) && list.iter().all(|e| is_error_safe(e))
        }
        Expr::Between { operand, low, high, .. } => {
            is_error_safe(operand) && is_error_safe(low) && is_error_safe(high)
        }
        Expr::Like { operand, pattern, .. } | Expr::Match { operand, pattern, .. } => is_error_safe(operand) && is_error_safe(pattern),
        _ => false,
    }
}

/// Evaluates a WHERE predicate, short-circuiting a top-level AND chain
/// left-to-right. The conjuncts are already in PostgreSQL's cost order — the
/// scan reorders them once via [`reorder_qual`] before iterating rows — so a
/// cheap filtering conjunct runs before a costlier erroring one (and a cheap
/// erroring conjunct before a costlier filtering one), reproducing PostgreSQL's
/// error timing without re-sorting per row.
fn where_passes<'e, 'a>(
    predicate: &'e Expr<'a>,
    arena: &'a Arena,
    params: &[Datum<'a>],
    row: &impl ColumnLookup<'a>,
    hooks: &EvalHooks<'_, 'a>,
) -> Result<bool, SqlError> {
    let mut conjuncts: [&'e Expr<'a>; MAX_CONJUNCTS] = [predicate; MAX_CONJUNCTS];
    let mut n = 0;
    if !flatten_and(predicate, &mut conjuncts, &mut n) || n <= 1 {
        return conjunct_passes(predicate, arena, params, row, hooks);
    }
    for &c in &conjuncts[..n] {
        if !conjunct_passes(c, arena, params, row, hooks)? {
            return Ok(false);
        }
    }
    Ok(true)
}

/// Folds `col IS NOT NULL` to TRUE and `col IS NULL` to FALSE for a column with
/// a NOT NULL constraint, as PostgreSQL does using the constraint — so
/// `WHERE x/0 = 1 OR id IS NOT NULL` (id NOT NULL) drops the erroring branch.
/// Rewrites only the boolean spine (AND/OR/NOT/IS NULL); other nodes pass
/// through, since an IS NULL test appears as a boolean operand.
fn fold_null<'a>(
    e: &'a Expr<'a>,
    scope: &QueryScope<'a>,
    arena: &'a Arena,
) -> Result<&'a Expr<'a>, SqlError> {
    use super::ast::{BinaryOp, UnaryOp};
    match e {
        Expr::IsNull { operand: Expr::Column { qualifier, name }, negated }
            if scope
                .find_column(*qualifier, name)
                .ok()
                .and_then(|entry| match entry {
                    ResolvedColumn::Table(t, c) => {
                        scope.defs[t].map(|d| d.columns()[c].not_null)
                    }
                    // A merged USING/NATURAL column can be null even over
                    // NOT NULL parts (outer-join null rows) — never fold.
                    ResolvedColumn::Merged(_) => None,
                })
                .unwrap_or(false) =>
        {
            Ok(&*arena.alloc(Expr::Bool(*negated)).map_err(|_| arena_full())?)
        }
        Expr::Binary { operator: operator @ (BinaryOp::And | BinaryOp::Or), left, right } => {
            let (l, r) = (fold_null(left, scope, arena)?, fold_null(right, scope, arena)?);
            if core::ptr::eq(l, *left) && core::ptr::eq(r, *right) {
                Ok(e)
            } else {
                Ok(&*arena
                    .alloc(Expr::Binary { operator: *operator, left: l, right: r })
                    .map_err(|_| arena_full())?)
            }
        }
        Expr::Unary { operator: UnaryOp::Not, operand } => {
            let o = fold_null(operand, scope, arena)?;
            if core::ptr::eq(o, *operand) {
                Ok(e)
            } else {
                Ok(&*arena
                    .alloc(Expr::Unary { operator: UnaryOp::Not, operand: o })
                    .map_err(|_| arena_full())?)
            }
        }
        _ => Ok(e),
    }
}

/// Reorders a WHERE predicate's top-level AND conjuncts by PostgreSQL's
/// `order_qual_clauses` cost (cheapest first, stably), returning a rebuilt
/// left-deep AND. Done once per scan (not per row), so it can afford the
/// type-aware `qual_cost`. Constants and non-AND predicates pass through
/// unchanged.
fn reorder_qual<'a>(
    pred: &'a Expr<'a>,
    scope: &QueryScope<'a>,
    arena: &'a Arena,
) -> Result<&'a Expr<'a>, SqlError> {
    let mut conjunct: [&Expr; MAX_CONJUNCTS] = [pred; MAX_CONJUNCTS];
    let mut n = 0;
    if !flatten_and(pred, &mut conjunct, &mut n) || n == 0 {
        return Ok(pred);
    }
    // PostgreSQL rewrites `x BETWEEN a AND b` at parse time into `x >= a AND
    // x <= b` — two *independent* top-level conjuncts that order separately
    // (each is one comparison, cheaper than a compound clause).
    let mut expanded: [&Expr; MAX_CONJUNCTS] = [pred; MAX_CONJUNCTS];
    let mut m = 0usize;
    for &c in &conjunct[..n] {
        if let Expr::Between { operand, low, high, negated: false } = c {
            if m + 2 > MAX_CONJUNCTS {
                return Ok(pred);
            }
            expanded[m] = arena
                .alloc(Expr::Binary { operator: super::ast::BinaryOp::GtEq, left: operand, right: low })
                .map_err(|_| arena_full())?;
            expanded[m + 1] = arena
                .alloc(Expr::Binary { operator: super::ast::BinaryOp::LtEq, left: operand, right: high })
                .map_err(|_| arena_full())?;
            m += 2;
        } else {
            if m + 1 > MAX_CONJUNCTS {
                return Ok(pred);
            }
            expanded[m] = c;
            m += 1;
        }
    }
    let conjunct = expanded;
    let n = m;
    if n <= 1 {
        return Ok(conjunct[0]);
    }
    // PostgreSQL routes top-level *equality* conjuncts through its
    // equivalence-class machinery, which re-appends them to the qual list
    // AFTER every other conjunct; only then does `order_qual_clauses` run its
    // stable per-tuple-cost insertion sort (verified against the PostgreSQL 18
    // source and pinned empirically — `(a%a)=a AND (…OR…)` evaluates the OR
    // first on an exact cost tie, while `0 <> (…) AND (…OR…)` keeps written
    // order). The same calibrated cost model drives projection postponement.
    let is_equality = |c: &Expr| -> bool {
        matches!(c, Expr::Binary { operator: super::ast::BinaryOp::Eq, .. })
            || matches!(c, Expr::InList { list, negated: false, .. } if list.len() == 1)
    };
    let mut order = [0usize; MAX_CONJUNCTS];
    let mut at = 0usize;
    for (i, c) in conjunct[..n].iter().enumerate() {
        if !is_equality(c) {
            order[at] = i;
            at += 1;
        }
    }
    for (i, c) in conjunct[..n].iter().enumerate() {
        if is_equality(c) {
            order[at] = i;
            at += 1;
        }
    }
    let mut cost = [0u32; MAX_CONJUNCTS];
    for (i, c) in conjunct[..n].iter().enumerate() {
        cost[i] = postpone_cost(c, scope, arena);
    }
    for i in 1..n {
        let mut j = i;
        while j > 0 && cost[order[j - 1]] > cost[order[j]] {
            order.swap(j - 1, j);
            j -= 1;
        }
    }
    // Rebuild a left-deep AND in cost order.
    let mut acc = conjunct[order[0]];
    for &i in &order[1..n] {
        acc = arena
            .alloc(Expr::Binary { operator: super::ast::BinaryOp::And, left: acc, right: conjunct[i] })
            .map_err(|_| arena_full())?;
    }
    Ok(acc)
}



/// FROM-less `SELECT` (one virtual row, no columns). Item and WHERE
/// expressions may still contain subqueries — always uncorrelated here, since
/// there is no outer row to reference — so they are prepared once and injected
/// by node identity, exactly as the table path does.
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

/// Whether `name` is one of the supported set-returning functions.
fn is_srf_name(name: &str) -> bool {
    name.eq_ignore_ascii_case("_pg_expandarray")
        || name.eq_ignore_ascii_case("unnest")
        || name.eq_ignore_ascii_case("generate_series")
        || name.eq_ignore_ascii_case("regexp_matches")
        || name.eq_ignore_ascii_case("jsonb_object_keys")
        || name.eq_ignore_ascii_case("json_object_keys")
        || name.eq_ignore_ascii_case("jsonb_array_elements")
        || name.eq_ignore_ascii_case("json_array_elements")
        || name.eq_ignore_ascii_case("jsonb_array_elements_text")
        || name.eq_ignore_ascii_case("json_array_elements_text")
        || name.eq_ignore_ascii_case("regexp_split_to_table")
        || name.eq_ignore_ascii_case("generate_subscripts")
        || is_json_each_name(name)
}

/// The set-returning function call (if any) driving a single expression's
/// expansion — the outermost SRF reachable through wrapping expressions.
fn srf_in_expr<'a>(e: &'a Expr<'a>) -> Option<&'a Expr<'a>> {
    match e {
        Expr::Call { name, .. } if is_srf_name(name) => Some(e),
        Expr::Field { base, .. } => srf_in_expr(base),
        Expr::Cast { operand, .. } => srf_in_expr(operand),
        Expr::Unary { operand, .. } => srf_in_expr(operand),
        Expr::Binary { left, right, .. } => srf_in_expr(left).or_else(|| srf_in_expr(right)),
        Expr::Call { args, .. } => args.iter().find_map(|a| srf_in_expr(a)),
        _ => None,
    }
}

/// The SRF (if any) driving a single select item's expansion.
fn srf_in_item<'a>(item: &'a SelectItem<'a>) -> Option<&'a Expr<'a>> {
    match item {
        SelectItem::Expr { expression, .. } => srf_in_expr(expression),
        SelectItem::RecordStar(base) => srf_in_expr(base),
        SelectItem::Wildcard | SelectItem::TableWildcard(_) => None,
    }
}

/// Finds a set-returning function call among the SELECT items (the whole call
/// node, so the caller can compute its row count), or None for a single row.
fn find_srf<'a>(items: &'a [SelectItem<'a>]) -> Option<&'a Expr<'a>> {
    items.iter().find_map(srf_in_item)
}

/// The number of output rows a select list's set-returning functions expand to:
/// the maximum length over all of them (each shorter one NULL-pads), matching
/// PostgreSQL's lockstep evaluation. Returns 1 when there is no SRF.
fn srf_max_count<'a, R: ColumnLookup<'a>>(
    items: &'a [SelectItem<'a>],
    arena: &'a Arena,
    params: &[Datum<'a>],
    row: &R,
    hooks: &EvalHooks<'_, 'a>,
) -> Result<usize, SqlError> {
    let mut any = false;
    let mut max = 0usize;
    for item in items {
        if let Some(call) = srf_in_item(item) {
            max = max.max(srf_count(call, arena, params, row, hooks)?);
            any = true;
        }
    }
    Ok(if any { max } else { 1 })
}

/// Number of output rows a set-returning call yields for the current source row.
fn srf_count<'a, R: ColumnLookup<'a>>(
    call: &'a Expr<'a>,
    arena: &'a Arena,
    params: &[Datum<'a>],
    row: &R,
    hooks: &EvalHooks<'_, 'a>,
) -> Result<usize, SqlError> {
    let Expr::Call { name, args, .. } = call else {
        return Ok(1);
    };
    let as_i64 = |d: &Datum| -> Option<i64> {
        match d {
            Datum::Int4(v) => Some(*v as i64),
            Datum::Int8(v) => Some(*v),
            _ => None,
        }
    };
    if name.eq_ignore_ascii_case("generate_series") {
        if !(2..=3).contains(&args.len()) {
            return Err(sql_err!(sqlstate::UNDEFINED_FUNCTION, "generate_series(...) argument count"));
        }
        let start = eval_full(args[0], arena, params, row, hooks)?;
        let stop = eval_full(args[1], arena, params, row, hooks)?;
        let step = if args.len() == 3 {
            eval_full(args[2], arena, params, row, hooks)?
        } else {
            Datum::Int4(1)
        };
        if start.is_null() || stop.is_null() || step.is_null() {
            return Ok(0);
        }
        if let (Some(s), Some(e), Some(st)) = (as_i64(&start), as_i64(&stop), as_i64(&step)) {
            if st == 0 {
                return Err(sql_err!(sqlstate::INVALID_PARAMETER_VALUE, "step size cannot equal zero"));
            }
            let n = if st > 0 {
                if e < s { 0 } else { (e - s) / st + 1 }
            } else if e > s {
                0
            } else {
                (s - e) / (-st) + 1
            };
            return Ok(n as usize);
        }
        // Temporal series: date/timestamp[tz] bounds with an interval step.
        // Coerce bare string literals for the stop and step, as PostgreSQL's
        // function resolution does.
        let Some((base, kind)) = super::eval::timestamp_series_start(&start) else {
            return Err(sql_err!(
                sqlstate::FEATURE_NOT_SUPPORTED,
                "generate_series is supported for integer and timestamp arguments"
            ));
        };
        let stop = super::eval::cast_to(stop, kind.coltype(), arena)?;
        let step = super::eval::cast_to(step, ColType::Interval, arena)?;
        let (Some((stop_micros, _)), Datum::Interval(step_iv)) =
            (super::eval::timestamp_series_start(&stop), step)
        else {
            return Ok(0);
        };
        super::eval::timestamp_series_count(base, stop_micros, step_iv)
    } else if name.eq_ignore_ascii_case("regexp_matches") {
        // Number of matches: 0/1 without the `g` flag, else all non-overlapping.
        if !(2..=3).contains(&args.len()) {
            return Err(sql_err!(sqlstate::UNDEFINED_FUNCTION, "regexp_matches(...) argument count"));
        }
        let string = eval_full(args[0], arena, params, row, hooks)?;
        let pattern = eval_full(args[1], arena, params, row, hooks)?;
        let (Datum::Text(string), Datum::Text(pattern)) = (string, pattern) else {
            return Ok(0);
        };
        let flags = if args.len() == 3 {
            match eval_full(args[2], arena, params, row, hooks)? {
                Datum::Text(f) => f,
                Datum::Null => return Ok(0),
                _ => "",
            }
        } else {
            ""
        };
        let (global, ci) = super::eval::regexp_flags(flags)?;
        let mut spans = [(-1i64, -1i64); super::regex::MAX_GROUPS];
        let mut from = 0usize;
        let mut n = 0usize;
        while let Some(((mstart, mend), _)) =
            super::regex::find_captures(pattern, string, from, ci, &mut spans)?
        {
            n += 1;
            if !global {
                break;
            }
            from = if mend > mstart { mend } else { mend + 1 };
            if from > string.len() {
                break;
            }
        }
        Ok(n)
    } else if name.eq_ignore_ascii_case("jsonb_object_keys")
        || name.eq_ignore_ascii_case("json_object_keys")
    {
        let text = match eval_full(args[0], arena, params, row, hooks)? {
            Datum::Json { text, .. } => text,
            Datum::Text(s) => s,
            Datum::Null => return Ok(0),
            _ => return Err(super::json::object_keys_error(name, super::json::Kind::Scalar)),
        };
        let kind = super::json::kind_of(text);
        if kind != super::json::Kind::Object {
            return Err(super::json::object_keys_error(name, kind));
        }
        if name.eq_ignore_ascii_case("jsonb_object_keys") {
            return match super::json::parse(text, arena)? {
                super::json::Json::Object(members) => Ok(members.len()),
                _ => Err(super::json::object_keys_error(name, kind)),
            };
        }
        Ok(super::json::object_members_source(text, arena)?.len())
    } else if name.eq_ignore_ascii_case("jsonb_array_elements")
        || name.eq_ignore_ascii_case("json_array_elements")
        || name.eq_ignore_ascii_case("jsonb_array_elements_text")
        || name.eq_ignore_ascii_case("json_array_elements_text")
    {
        let jsonb = name.eq_ignore_ascii_case("jsonb_array_elements")
            || name.eq_ignore_ascii_case("jsonb_array_elements_text");
        let text = match eval_full(args[0], arena, params, row, hooks)? {
            Datum::Json { text, .. } => text,
            Datum::Text(s) => s,
            Datum::Null => return Ok(0),
            _ => return Err(super::json::array_elements_error(name, jsonb, super::json::Kind::Scalar)),
        };
        let kind = super::json::kind_of(text);
        if kind != super::json::Kind::Array {
            return Err(super::json::array_elements_error(name, jsonb, kind));
        }
        if jsonb {
            return match super::json::parse(text, arena)? {
                super::json::Json::Array(items) => Ok(items.len()),
                _ => Err(super::json::array_elements_error(name, jsonb, kind)),
            };
        }
        Ok(super::json::array_elements_source(text, arena)?.len())
    } else if is_json_each_name(name) {
        let jsonb = name.eq_ignore_ascii_case("jsonb_each")
            || name.eq_ignore_ascii_case("jsonb_each_text");
        let as_text = name.eq_ignore_ascii_case("json_each_text")
            || name.eq_ignore_ascii_case("jsonb_each_text");
        let text = match eval_full(args[0], arena, params, row, hooks)? {
            Datum::Json { text, .. } => text,
            Datum::Text(s) => s,
            Datum::Null => return Ok(0),
            _ => return Err(sql_err!(sqlstate::INVALID_PARAMETER_VALUE, "cannot deconstruct a scalar")),
        };
        Ok(super::eval::json_each_pairs(text, jsonb, as_text, arena)?.len())
    } else if name.eq_ignore_ascii_case("regexp_split_to_table") {
        let (src, pat) = match (
            eval_full(args[0], arena, params, row, hooks)?,
            eval_full(args[1], arena, params, row, hooks)?,
        ) {
            (Datum::Text(s), Datum::Text(p)) => (s, p),
            _ => return Ok(0),
        };
        let ci = if args.len() == 3 {
            match eval_full(args[2], arena, params, row, hooks)? {
                Datum::Text(f) => super::eval::regexp_flags(f)?.1,
                _ => return Ok(0),
            }
        } else {
            false
        };
        Ok(super::eval::regex_split_pub(src, pat, ci, arena)?.len())
    } else if name.eq_ignore_ascii_case("generate_subscripts") {
        let raw = match eval_full(args[0], arena, params, row, hooks)? {
            Datum::Array { raw, .. } => raw,
            _ => return Ok(0),
        };
        let dim = match eval_full(args[1], arena, params, row, hooks)? {
            Datum::Int4(v) => v as i64,
            Datum::Int8(v) => v,
            _ => return Ok(0),
        };
        Ok(if dim == 1 { super::array::len(raw) } else { 0 })
    } else {
        // unnest / _pg_expandarray over an array.
        match eval_full(args[0], arena, params, row, hooks)? {
            Datum::Array { raw, .. } => Ok(super::array::len(raw)),
            Datum::Null => Ok(0),
            _ => Ok(1),
        }
    }
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
struct ScopeCols<'s, 'd>(&'s QueryScope<'d>);
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
fn record_star_width(base: &Expr, scope: &QueryScope) -> usize {
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

/// With correlated subqueries in WHERE, the filter cannot run inside
/// `scan_source` (their values change per row): re-evaluate the correlated
/// nodes against this row, then apply WHERE under the merged hooks. With no
/// correlated subqueries this is a no-op (the scan already filtered).
#[expect(clippy::too_many_arguments, reason = "query pipeline plumbing")]
fn row_passes_correlated_where<'a>(
    correlated: &'a [&'a Expr<'a>],
    where_clause: Option<&'a Expr<'a>>,
    storage: &'a Storage,
    txid: u32,
    arena: &'a Arena,
    params: &[Datum<'a>],
    hooks: &EvalHooks<'_, 'a>,
    row: &JoinRow<'_, 'a, '_>,
) -> Result<bool, SqlError> {
    if correlated.is_empty() {
        return Ok(true);
    }
    let Some(w) = where_clause else {
        return Ok(true);
    };
    let mut sc: [(*const Expr, Datum, Datum); MAX_SUBQUERIES] =
        [(core::ptr::null(), Datum::Null, Datum::Null); MAX_SUBQUERIES];
    let mut ls: [(*const Expr, &[Datum], bool, Datum); MAX_SUBQUERIES] =
        [(core::ptr::null(), &[], false, Datum::Null); MAX_SUBQUERIES];
    let base = hooks.subs.expect("outer subqueries prepared");
    let row_subs =
        merge_correlated(correlated, base, row, storage, txid, arena, params, &mut sc, &mut ls)?;
    let h = EvalHooks { subs: Some(&row_subs), ..*hooks };
    where_passes(w, arena, params, row, &h)
}

/// The outer-row lookup for a correlated subquery re-evaluated per GROUP:
/// a reference resolves to a grouping key's value for this group (a key
/// collapsed by the current grouping set reads NULL). Anything else is
/// PostgreSQL's ungrouped-column error (42803).
struct GroupRow<'g, 'a> {
    group_by: &'a [&'a Expr<'a>],
    keys: &'g [Datum<'a>],
}

impl<'a> ColumnLookup<'a> for GroupRow<'_, 'a> {
    fn lookup(&self, qualifier: Option<&str>, name: &str) -> Result<Datum<'a>, SqlError> {
        for (g, v) in self.group_by.iter().zip(self.keys) {
            if let Expr::Column { qualifier: gq, name: gn } = g
                && *gn == name
                && (qualifier.is_none() || gq.is_none() || *gq == qualifier)
            {
                return Ok(*v);
            }
        }
        Err(sql_err!(
            "42803",
            "subquery uses ungrouped column \"{}{}{}\" from outer query",
            qualifier.unwrap_or(""),
            if qualifier.is_some() { "." } else { "" },
            name
        ))
    }
}

/// Aggregates and emits the output rows for a single grouping-set `mask` (bit
/// *i* set = `group_by[i]` participates; a cleared bit collapses that column to
/// NULL so every row shares one group and the output column reads NULL). Returns
/// the surviving rows (visible columns followed by hidden ORDER BY key columns),
/// unsorted — the caller concatenates the sets and sorts once.
#[expect(clippy::too_many_arguments, reason = "query pipeline plumbing")]
fn groups_for_mask<'a>(
    storage: &'a Storage,
    scope: &QueryScope<'a>,
    from: &'a FromClause<'a>,
    txid: u32,
    statement: &'a Select<'a>,
    agg_nodes: &[(*const Expr<'a>, &'a Expr<'a>)],
    arena: &'a Arena,
    params: &[Datum<'a>],
    hooks: &EvalHooks<'_, 'a>,
    correlated: &'a [&'a Expr<'a>],
    outer: Option<&dyn ColumnLookup<'a>>,
    mask: u64,
    row_count: usize,
    agg_ptrs: &'a [*const Expr<'a>],
    order_exprs: &[Option<&'a Expr<'a>>],
    width: usize,
    n_order: usize,
) -> Result<&'a [&'a [u8]], SqlError> {
    let n_keys = statement.group_by.len();
    // WHERE with correlated subqueries is applied per row in the callbacks.
    let scan_where = if correlated.is_empty() { statement.where_clause } else { None };

    // Pass 2: encode group keys per row (columns outside this set → NULL).
    let empty: &[u8] = &[];
    let keys: &mut [(&[u8], u32)] = arena
        .alloc_slice_with(row_count, |_| (empty, 0u32))
        .map_err(|_| arena_full())?;
    {
        let mut at = 0usize;
        scan_source(
            storage, scope, from, txid, scan_where, arena, params, hooks,
            outer,
            &mut |row| {
                if !row_passes_correlated_where(
                    correlated, statement.where_clause, storage, txid, arena, params, hooks, row,
                )? {
                    return Ok(true);
                }
                // Correlated subqueries inside the grouping keys re-evaluate
                // against each input row.
                let mut sc: [(*const Expr, Datum, Datum); MAX_SUBQUERIES] =
                    [(core::ptr::null(), Datum::Null, Datum::Null); MAX_SUBQUERIES];
                let mut ls: [(*const Expr, &[Datum], bool, Datum); MAX_SUBQUERIES] =
                    [(core::ptr::null(), &[], false, Datum::Null); MAX_SUBQUERIES];
                let row_subs;
                let row_hooks_store;
                let row_hooks: &EvalHooks = if correlated.is_empty() {
                    hooks
                } else {
                    row_subs = merge_correlated(
                        correlated,
                        hooks.subs.expect("outer subqueries prepared"),
                        row,
                        storage,
                        txid,
                        arena,
                        params,
                        &mut sc,
                        &mut ls,
                    )?;
                    row_hooks_store = EvalHooks { subs: Some(&row_subs), ..*hooks };
                    &row_hooks_store
                };
                let mut key_vals = [Datum::Null; MAX_PROJ];
                for (k, g) in statement.group_by.iter().enumerate() {
                    if mask & (1u64 << k) != 0 {
                        key_vals[k] = eval_full(g, arena, params, row, row_hooks)?;
                    }
                }
                keys[at].0 = super::exec::encode_projected_pub(&key_vals[..n_keys], arena)?;
                keys[at].1 = at as u32;
                at += 1;
                Ok(true)
            },
        )?;
    }
    keys.sort_unstable();

    let n_groups = {
        let mut g = 0usize;
        for i in 0..keys.len() {
            if i == 0 || keys[i].0 != keys[i - 1].0 {
                g += 1;
            }
        }
        if keys.is_empty() && mask == 0 {
            1 // grand total (no active grouping columns): one row even over zero input rows
        } else {
            g
        }
    };
    let group_of: &mut [u32] = arena
        .alloc_slice_with(row_count, |_| 0u32)
        .map_err(|_| arena_full())?;
    let rep_of: &mut [u32] = arena
        .alloc_slice_with(n_groups, |_| 0u32)
        .map_err(|_| arena_full())?;
    {
        let mut g = 0usize;
        for i in 0..keys.len() {
            if i > 0 && keys[i].0 != keys[i - 1].0 {
                g += 1;
            }
            group_of[keys[i].1 as usize] = g as u32;
            rep_of[g] = keys[i].1;
        }
    }

    // Correlated subquery nodes appearing in the select list, HAVING, or the
    // ORDER BY keys re-evaluate per group (their outer references resolve to
    // the group's keys); the rest were WHERE-level and already applied.
    let mut group_correlated_buffer: [&Expr; MAX_SUBQUERIES] = [&Expr::Null; MAX_SUBQUERIES];
    let mut n_group_correlated = 0usize;
    for &node in correlated {
        let in_group_clauses = statement.items.iter().any(|item| {
            matches!(item, SelectItem::Expr { expression, .. }
                if expr_contains_node(expression, node as *const Expr))
        }) || statement.having.is_some_and(|h| expr_contains_node(h, node as *const Expr))
            || order_exprs.iter().take(n_order).any(|oe| {
                oe.is_some_and(|o| expr_contains_node(o, node as *const Expr))
            });
        if in_group_clauses && n_group_correlated < MAX_SUBQUERIES {
            group_correlated_buffer[n_group_correlated] = node;
            n_group_correlated += 1;
        }
    }
    let group_correlated = &group_correlated_buffer[..n_group_correlated];

    let n_aggs = agg_nodes.len();
    let states: &mut [AggState] = arena
        .alloc_slice_with(n_groups * n_aggs.max(1), |_| AggState::default())
        .map_err(|_| arena_full())?;
    for g in 0..n_groups {
        for (i, (_, node)) in agg_nodes.iter().enumerate() {
            states[g * n_aggs.max(1) + i].init(node)?;
        }
    }
    if n_aggs > 0 {
        let mut at = 0usize;
        scan_source(
            storage, scope, from, txid, scan_where, arena, params, hooks,
            outer,
            &mut |row| {
                if !row_passes_correlated_where(
                    correlated, statement.where_clause, storage, txid, arena, params, hooks, row,
                )? {
                    return Ok(true);
                }
                // Correlated subqueries inside aggregate arguments re-evaluate
                // against each input row.
                let mut sc: [(*const Expr, Datum, Datum); MAX_SUBQUERIES] =
                    [(core::ptr::null(), Datum::Null, Datum::Null); MAX_SUBQUERIES];
                let mut ls: [(*const Expr, &[Datum], bool, Datum); MAX_SUBQUERIES] =
                    [(core::ptr::null(), &[], false, Datum::Null); MAX_SUBQUERIES];
                let row_subs;
                let row_hooks_store;
                let row_hooks: &EvalHooks = if correlated.is_empty() {
                    hooks
                } else {
                    row_subs = merge_correlated(
                        correlated,
                        hooks.subs.expect("outer subqueries prepared"),
                        row,
                        storage,
                        txid,
                        arena,
                        params,
                        &mut sc,
                        &mut ls,
                    )?;
                    row_hooks_store = EvalHooks { subs: Some(&row_subs), ..*hooks };
                    &row_hooks_store
                };
                let g = group_of.get(at).copied().unwrap_or(0) as usize;
                for (i, (_, node)) in agg_nodes.iter().enumerate() {
                    states[g * n_aggs + i].update(node, arena, params, row, row_hooks)?;
                }
                at += 1;
                Ok(true)
            },
        )?;
    }

    let out_rows: &mut [&[u8]] = arena
        .alloc_slice_with(n_groups, |_| empty)
        .map_err(|_| arena_full())?;
    let mut survivors = 0usize;
    for g in 0..n_groups {
        let mut key_vals = [Datum::Null; MAX_PROJ];
        if !keys.is_empty() {
            let rep = keys
                .iter()
                .find(|(_, index)| group_of[*index as usize] as usize == g)
                .expect("group non-empty");
            for (k, slot) in key_vals.iter_mut().enumerate().take(n_keys) {
                *slot = super::exec::decode_projected_pub(rep.0, k);
            }
        }
        let mut agg_vals = [Datum::Null; MAX_AGGS];
        for i in 0..n_aggs {
            agg_vals[i] = states[g * n_aggs.max(1) + i].finish(arena)?;
        }
        let mut sc: [(*const Expr, Datum, Datum); MAX_SUBQUERIES] =
            [(core::ptr::null(), Datum::Null, Datum::Null); MAX_SUBQUERIES];
        let mut ls: [(*const Expr, &[Datum], bool, Datum); MAX_SUBQUERIES] =
            [(core::ptr::null(), &[], false, Datum::Null); MAX_SUBQUERIES];
        let merged_subs;
        let group_subs = if group_correlated.is_empty() {
            hooks.subs
        } else {
            let group_row = GroupRow { group_by: statement.group_by, keys: &key_vals[..n_keys] };
            merged_subs = merge_correlated(
                group_correlated,
                hooks.subs.expect("outer subqueries prepared"),
                &group_row,
                storage,
                txid,
                arena,
                params,
                &mut sc,
                &mut ls,
            )?;
            Some(&merged_subs)
        };
        let group_hooks = EvalHooks {
            group: Some((statement.group_by, &key_vals[..n_keys], mask)),
            aggs: Some((agg_ptrs, &agg_vals[..n_aggs])),
            subs: group_subs,
        windows: None, catalog: None, srf_index: None };
        let schema = ScopeSchema(scope);
        if let Some(h) = statement.having {
            match eval_full(h, arena, params, &schema, &group_hooks)? {
                Datum::Bool(true) => {}
                Datum::Bool(false) | Datum::Null => continue,
                _ => {
                    return Err(sql_err!(
                        sqlstate::DATATYPE_MISMATCH,
                        "argument of HAVING must be type boolean"
                    ))
                }
            }
        }
        let mut full = [Datum::Null; MAX_PROJ];
        for (n, item) in statement.items.iter().enumerate() {
            let SelectItem::Expr { expression, .. } = item else { unreachable!() };
            full[n] = eval_full(expression, arena, params, &schema, &group_hooks)?;
        }
        for (k, oe) in order_exprs.iter().take(n_order).enumerate() {
            full[width + k] = eval_full(
                oe.expect("resolved"),
                arena,
                params,
                &schema,
                &group_hooks,
            )?;
        }
        out_rows[survivors] =
            super::exec::encode_projected_pub(&full[..width + n_order], arena)?;
        survivors += 1;
    }
    Ok(&out_rows[..survivors])
}

/// GROUP BY / plain-aggregate execution: single scan collecting encoded
/// (key, agg-argument) pairs is avoided by running one scan per phase —
/// group keys with row-by-row aggregate folding, sort-based.
/// The row-producing half of grouped/aggregate execution: runs the scans,
/// folds aggregates per group, applies HAVING, and returns the surviving
/// output rows (self-describing-encoded, `width` visible columns followed by
/// `n_order` hidden ORDER BY key columns) sorted by any ORDER BY. The caller
/// applies LIMIT/OFFSET and emits. Shared by the wire path (`grouped_select`)
/// and the row-source path (`select_into_rows`).
#[expect(clippy::too_many_arguments, reason = "query pipeline plumbing")]
fn grouped_rows<'a>(
    storage: &'a Storage,
    scope: &QueryScope<'a>,
    from: &'a FromClause<'a>,
    txid: u32,
    statement: &'a Select<'a>,
    agg_nodes: &[(*const Expr<'a>, &'a Expr<'a>)],
    arena: &'a Arena,
    params: &[Datum<'a>],
    hooks: &EvalHooks<'_, 'a>,
    correlated: &'a [&'a Expr<'a>],
    outer: Option<&dyn ColumnLookup<'a>>,
) -> Result<(&'a [&'a [u8]], usize), SqlError> {
    // Validate: non-aggregate select items must be GROUP BY expressions.
    for item in statement.items {
        let SelectItem::Expr { expression, .. } = item else {
            return Err(sql_err!(
                "42803",
                "SELECT * must appear in the GROUP BY clause or be used in an aggregate function"
            ));
        };
        if !expr_is_grouped(expression, statement.group_by) {
            return Err(sql_err!(
                "42803",
                "column must appear in the GROUP BY clause or be used in an aggregate function"
            ));
        }
    }

    // Pass 1: count rows, so group storage can be arena-allocated. WHERE
    // with correlated subqueries is applied per row here too, so every pass
    // sees the same filtered sequence.
    let scan_where = if correlated.is_empty() { statement.where_clause } else { None };
    let mut row_count = 0usize;
    scan_source(
        storage, scope, from, txid, scan_where, arena, params, hooks,
        outer,
        &mut |row| {
            if !row_passes_correlated_where(
                correlated, statement.where_clause, storage, txid, arena, params, hooks, row,
            )? {
                return Ok(true);
            }
            row_count += 1;
            Ok(true)
        },
    )?;

    let n_keys = statement.group_by.len();
    let width = statement.items.len();
    let n_order = statement.order_by.len();
    let n_aggs = agg_nodes.len();
    // Aggregate-call node addresses, resolved once (shared by every set).
    let agg_ptrs: &[*const Expr] = arena
        .alloc_slice_with(n_aggs, |i| agg_nodes[i].0)
        .map_err(|_| arena_full())?;
    // ORDER BY over groups: ordinals resolve to select items; keys evaluate
    // under the group hooks (so aggregates work). Resolved once.
    let mut order_arr: [Option<&Expr>; MAX_PROJ] = [None; MAX_PROJ];
    for (k, ob) in statement.order_by.iter().enumerate() {
        order_arr[k] = Some(resolve_order_target(ob.expression, statement.items, scope, arena)?);
    }
    let order_exprs = &order_arr[..n_order];

    // Grouping sets: the explicit mask list, or a single implicit set of all
    // grouping columns for a plain GROUP BY / plain aggregate.
    let all_mask = if n_keys >= 64 { u64::MAX } else { (1u64 << n_keys) - 1 };
    let single = [all_mask];
    let masks: &[u64] = if statement.grouping_sets.is_empty() {
        &single[..]
    } else {
        statement.grouping_sets
    };
    if masks.len() > super::parser::MAX_GROUPING_SETS {
        return Err(sql_err!(sqlstate::PROGRAM_LIMIT_EXCEEDED, "too many grouping sets"));
    }

    // Aggregate each set independently, then concatenate (a single set is a
    // straight copy). ORDER BY applies across the combined result, so it is
    // deferred until after concatenation.
    let empty_rows: &[&[u8]] = &[];
    let mut per_set: [&[&[u8]]; super::parser::MAX_GROUPING_SETS] =
        [empty_rows; super::parser::MAX_GROUPING_SETS];
    let mut total = 0usize;
    for (si, &mask) in masks.iter().enumerate() {
        let rows = groups_for_mask(
            storage, scope, from, txid, statement, agg_nodes, arena, params, hooks, correlated,
            outer, mask, row_count, agg_ptrs, order_exprs, width, n_order,
        )?;
        per_set[si] = rows;
        total += rows.len();
    }

    let empty: &[u8] = &[];
    let out_rows: &mut [&[u8]] = arena
        .alloc_slice_with(total, |_| empty)
        .map_err(|_| arena_full())?;
    let mut at = 0usize;
    for rows in &per_set[..masks.len()] {
        for &r in rows.iter() {
            out_rows[at] = r;
            at += 1;
        }
    }

    let mut live = out_rows.len();
    if statement.distinct {
        // DISTINCT over the grouped output. ORDER BY keys must be select-list
        // members (as in PostgreSQL); then dedupe on the visible prefix.
        for oe in order_exprs.iter() {
            let target = oe.expect("resolved");
            let in_list = statement.items.iter().any(|item| {
                matches!(item, SelectItem::Expr { expression, .. } if **expression == *target)
            });
            if !in_list {
                return Err(sql_err!(
                    "42P10",
                    "for SELECT DISTINCT, ORDER BY expressions must appear in select list"
                ));
            }
        }
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
            for (k, ob) in statement.order_by.iter().enumerate() {
                let ka = super::exec::decode_projected_pub(a, width + k);
                let kb = super::exec::decode_projected_pub(b, width + k);
                // NULL placement follows NULLS FIRST/LAST (absolute, not
                // affected by ASC/DESC); only the value comparison reverses
                // for DESC.
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

    Ok((out_rows, width))
}

/// GROUP BY / plain-aggregate execution to the wire: produce the grouped rows,
/// then page with LIMIT/OFFSET and emit.
#[expect(clippy::too_many_arguments, reason = "query pipeline plumbing")]
fn grouped_select<'a>(
    storage: &'a Storage,
    scope: &QueryScope<'a>,
    from: &'a FromClause<'a>,
    txid: u32,
    statement: &'a Select<'a>,
    agg_nodes: &[(*const Expr<'a>, &'a Expr<'a>)],
    arena: &'a Arena,
    params: &[Datum<'a>],
    hooks: &EvalHooks<'_, 'a>,
    correlated: &'a [&'a Expr<'a>],
    limit: u64,
    offset: u64,
    responder: &mut Responder,
) -> Outcome {
    let (out_rows, width) = match grouped_rows(
        storage, scope, from, txid, statement, agg_nodes, arena, params, hooks, correlated, None,
    ) {
        Ok(x) => x,
        Err(e) => return sql_fail(e),
    };
    let mut emitted = 0u64;
    for row in out_rows.iter().skip(offset as usize) {
        if emitted >= limit {
            break;
        }
        let mut out = [Datum::Null; MAX_PROJ];
        for (i, slot) in out.iter_mut().take(width).enumerate() {
            *slot = super::exec::decode_projected_pub(row, i);
        }
        responder.data_row(&out[..width])?;
        emitted += 1;
    }
    let tag = stack_format!(48, "SELECT {}", emitted);
    responder.command_complete(tag.as_str())?;
    sql_ok()
}

/// Does this item expression consist only of grouped expressions,
/// aggregates, and constants?
fn expr_is_grouped(expression: &Expr, group_by: &[&Expr]) -> bool {
    if group_by.iter().any(|g| **g == *expression) || expression.is_aggregate() {
        return true;
    }
    match expression {
        Expr::Column { .. } | Expr::WholeRow(_) => false,
        Expr::Null | Expr::Bool(_) | Expr::Int(_) | Expr::Float(_) | Expr::NumericLit(_) | Expr::Str(_)
        | Expr::BitLit(_) | Expr::Param(_) | Expr::DefaultMarker | Expr::Subquery(_) | Expr::Exists(_)
        | Expr::ArraySubquery(_) => true,
        Expr::Unary { operand, .. }
        | Expr::Cast { operand, .. }
        | Expr::IsNull { operand, .. } => expr_is_grouped(operand, group_by),
        Expr::Binary { left, right, .. } => {
            expr_is_grouped(left, group_by) && expr_is_grouped(right, group_by)
        }
        Expr::Call { args, .. } => args.iter().all(|a| expr_is_grouped(a, group_by)),
        Expr::InList { operand, list, .. } => {
            expr_is_grouped(operand, group_by)
                && list.iter().all(|e| expr_is_grouped(e, group_by))
        }
        Expr::Between { operand, low, high, .. } => {
            expr_is_grouped(operand, group_by)
                && expr_is_grouped(low, group_by)
                && expr_is_grouped(high, group_by)
        }
        Expr::Like { operand, pattern, .. } | Expr::Match { operand, pattern, .. } => {
            expr_is_grouped(operand, group_by) && expr_is_grouped(pattern, group_by)
        }
        Expr::Case { operand, whens, otherwise } => {
            operand.is_none_or(|o| expr_is_grouped(o, group_by))
                && whens
                    .iter()
                    .all(|(c, r)| expr_is_grouped(c, group_by) && expr_is_grouped(r, group_by))
                && otherwise.is_none_or(|o| expr_is_grouped(o, group_by))
        }
        Expr::InSubquery { operand, .. } => expr_is_grouped(operand, group_by),
        Expr::Array(items) => items.iter().all(|e| expr_is_grouped(e, group_by)),
        Expr::Subscript { base, index } => {
            expr_is_grouped(base, group_by) && expr_is_grouped(index, group_by)
        }
        Expr::Field { base, .. } => expr_is_grouped(base, group_by),
        Expr::AnyAll { operand, array, .. } => {
            expr_is_grouped(operand, group_by) && expr_is_grouped(array, group_by)
        }
    }
}

/// DISTINCT / ORDER BY: materialize projected rows plus hidden key
/// columns, dedupe on the visible prefix, sort by the hidden keys, page.
/// The row-producing half of DISTINCT / ORDER BY execution: materialize
/// projected rows (with hidden ORDER BY key columns), dedupe on the visible
/// prefix for DISTINCT, and sort by the hidden keys. Returns `(rows, width)`;
/// the caller pages with LIMIT/OFFSET and emits. Shared by the wire path
/// (`materialized_select`) and the row-source path (`select_into_rows`).
/// Evaluation cost of a select-list expression in half-operator units,
/// approximating PostgreSQL's `cost_qual_eval`: each operator or function
/// application costs 2, an implicit numeric-family coercion costs 2, an IN
/// list costs 1 per element (PostgreSQL charges half an operator per element),
/// and AND/OR/IS NULL cost nothing. Used for the sort/limit projection
/// postponement decision (threshold empirically pinned against PostgreSQL 18.4:
/// items costing more than 10 operators are projected above the Sort + Limit).
/// PostgreSQL's `find_duplicate_ors` canonicalization: AND terms common to
/// every arm of an OR are factored out in front — `(A AND B) OR (A AND C)`
/// becomes `A AND (B OR C)`, and when an arm consists *only* of common terms
/// the whole OR collapses to them (`(A AND B) OR A` ≡ `A`), so the dropped
/// arms' other conjuncts are never evaluated.
fn factor_common_or_terms<'a>(
    e: &'a Expr<'a>,
    arena: &'a Arena,
) -> Result<&'a Expr<'a>, SqlError> {
    use super::ast::BinaryOp;
    const MAX_PARTS: usize = 16;
    fn flatten_or<'a>(x: &'a Expr<'a>, out: &mut [&'a Expr<'a>; MAX_PARTS], n: &mut usize) -> bool {
        if let Expr::Binary { operator: BinaryOp::Or, left, right } = x {
            return flatten_or(left, out, n) && flatten_or(right, out, n);
        }
        if *n == MAX_PARTS {
            return false;
        }
        out[*n] = x;
        *n += 1;
        true
    }
    fn and_terms<'a>(x: &'a Expr<'a>, out: &mut [&'a Expr<'a>; MAX_PARTS], n: &mut usize) -> bool {
        if let Expr::Binary { operator: BinaryOp::And, left, right } = x {
            return and_terms(left, out, n) && and_terms(right, out, n);
        }
        if *n == MAX_PARTS {
            return false;
        }
        out[*n] = x;
        *n += 1;
        true
    }
    let dummy = e;
    let mut arms: [&Expr; MAX_PARTS] = [dummy; MAX_PARTS];
    let mut n_arms = 0;
    if !flatten_or(e, &mut arms, &mut n_arms) || n_arms < 2 {
        return Ok(e);
    }
    // Terms of the first arm that appear in every other arm.
    let mut common: [&Expr; MAX_PARTS] = [dummy; MAX_PARTS];
    let mut n_common = 0;
    if !and_terms(arms[0], &mut common, &mut n_common) {
        return Ok(e);
    }
    let mut kept = 0usize;
    'term: for i in 0..n_common {
        for arm in &arms[1..n_arms] {
            let mut terms: [&Expr; MAX_PARTS] = [dummy; MAX_PARTS];
            let mut nt = 0;
            if !and_terms(arm, &mut terms, &mut nt) {
                return Ok(e);
            }
            if !terms[..nt].iter().any(|t| **t == *common[i]) {
                continue 'term;
            }
        }
        common[kept] = common[i];
        kept += 1;
    }
    if kept == 0 {
        return Ok(e);
    }
    // Residue of each arm (its terms minus the common ones). An empty residue
    // means that arm is implied by the common terms: the OR collapses.
    let mut residues: [&Expr; MAX_PARTS] = [dummy; MAX_PARTS];
    let mut n_res = 0;
    for arm in &arms[..n_arms] {
        let mut terms: [&Expr; MAX_PARTS] = [dummy; MAX_PARTS];
        let mut nt = 0;
        let _ = and_terms(arm, &mut terms, &mut nt);
        let mut residue: Option<&Expr> = None;
        for &t in &terms[..nt] {
            if common[..kept].iter().any(|c| **c == *t) {
                continue;
            }
            residue = Some(match residue {
                None => t,
                Some(acc) => arena
                    .alloc(Expr::Binary { operator: BinaryOp::And, left: acc, right: t })
                    .map_err(|_| arena_full())?,
            });
        }
        match residue {
            None => {
                // This arm is exactly the common terms: OR collapses to them.
                let mut acc = common[0];
                for &c in &common[1..kept] {
                    acc = arena
                        .alloc(Expr::Binary { operator: BinaryOp::And, left: acc, right: c })
                        .map_err(|_| arena_full())?;
                }
                return Ok(acc);
            }
            Some(x) => {
                residues[n_res] = x;
                n_res += 1;
            }
        }
    }
    // AND(common..., OR(residues...)).
    let mut or_acc = residues[0];
    for &x in &residues[1..n_res] {
        or_acc = arena
            .alloc(Expr::Binary { operator: BinaryOp::Or, left: or_acc, right: x })
            .map_err(|_| arena_full())?;
    }
    let mut acc = common[0];
    for &c in &common[1..kept] {
        acc = arena
            .alloc(Expr::Binary { operator: BinaryOp::And, left: acc, right: c })
            .map_err(|_| arena_full())?;
    }
    Ok(&*arena
        .alloc(Expr::Binary { operator: BinaryOp::And, left: acc, right: or_acc })
        .map_err(|_| arena_full())?)
}

/// The plan-time boolean value of a condition, when PostgreSQL's
/// `eval_const_expressions` can decide it: a constant subtree, or an AND/OR
/// settled by one constant side. `None` = not decidable at plan time.
fn plan_time_bool(e: &Expr, arena: &Arena) -> Option<bool> {
    use super::ast::BinaryOp;
    if e.is_constant() {
        return match super::eval::eval(e, arena, super::eval::NO_PARAMS, &super::eval::NoColumns) {
            Ok(Datum::Bool(b)) => Some(b),
            _ => None,
        };
    }
    match e {
        Expr::Binary { operator: BinaryOp::And, left, right } => {
            if plan_time_bool(left, arena) == Some(false)
                || plan_time_bool(right, arena) == Some(false)
            {
                Some(false)
            } else {
                None
            }
        }
        Expr::Binary { operator: BinaryOp::Or, left, right } => {
            if plan_time_bool(left, arena) == Some(true)
                || plan_time_bool(right, arena) == Some(true)
            {
                Some(true)
            } else {
                None
            }
        }
        Expr::Unary { operator: super::ast::UnaryOp::Not, operand } => {
            plan_time_bool(operand, arena).map(|b| !b)
        }
        _ => None,
    }
}

/// PostgreSQL's plan-time boolean simplification applied to a qual: an AND arm
/// folding TRUE (or an OR arm folding FALSE) is dropped, and a decided
/// connective collapses to its constant. This exposes a nested AND to the
/// top-level conjunct ordering — `(a AND b) OR const-false` orders `a`/`b` by
/// cost just as PostgreSQL does after simplifying the OR away.
fn simplify_qual<'a>(e: &'a Expr<'a>, arena: &'a Arena) -> Result<&'a Expr<'a>, SqlError> {
    use super::ast::BinaryOp;
    if let Some(b) = plan_time_bool(e, arena) {
        return Ok(if b { &Expr::Bool(true) } else { &Expr::Bool(false) });
    }
    match e {
        Expr::Binary { operator: operator @ (BinaryOp::And | BinaryOp::Or), left, right } => {
            let keep_true = matches!(operator, BinaryOp::And);
            let l = simplify_qual(left, arena)?;
            let r = simplify_qual(right, arena)?;
            // The decided-connective cases returned above, so at most one side
            // is the droppable constant here.
            if *l == Expr::Bool(keep_true) {
                return Ok(r);
            }
            if *r == Expr::Bool(keep_true) {
                return Ok(l);
            }
            let rebuilt: &Expr = if core::ptr::eq(l, *left) && core::ptr::eq(r, *right) {
                e
            } else {
                arena
                    .alloc(Expr::Binary { operator: *operator, left: l, right: r })
                    .map_err(|_| arena_full())?
            };
            if matches!(operator, BinaryOp::Or) {
                return factor_common_or_terms(rebuilt, arena);
            }
            Ok(rebuilt)
        }
        // NOT pushes through the connectives (De Morgan), exposing the pieces
        // to top-level conjunct ordering exactly as PostgreSQL's
        // `canonicalize_qual` does: `NOT (x OR y IS NOT NULL)` becomes
        // `NOT x AND y IS NULL`, so the cheap null test can filter first.
        Expr::Unary { operator: super::ast::UnaryOp::Not, operand } => {
            let negated: &Expr = match *operand {
                Expr::Binary { operator: BinaryOp::Or, left, right } => {
                    let nl = arena
                        .alloc(Expr::Unary { operator: super::ast::UnaryOp::Not, operand: left })
                        .map_err(|_| arena_full())?;
                    let nr = arena
                        .alloc(Expr::Unary { operator: super::ast::UnaryOp::Not, operand: right })
                        .map_err(|_| arena_full())?;
                    arena
                        .alloc(Expr::Binary { operator: BinaryOp::And, left: nl, right: nr })
                        .map_err(|_| arena_full())?
                }
                Expr::Binary { operator: BinaryOp::And, left, right } => {
                    let nl = arena
                        .alloc(Expr::Unary { operator: super::ast::UnaryOp::Not, operand: left })
                        .map_err(|_| arena_full())?;
                    let nr = arena
                        .alloc(Expr::Unary { operator: super::ast::UnaryOp::Not, operand: right })
                        .map_err(|_| arena_full())?;
                    arena
                        .alloc(Expr::Binary { operator: BinaryOp::Or, left: nl, right: nr })
                        .map_err(|_| arena_full())?
                }
                Expr::Unary { operator: super::ast::UnaryOp::Not, operand: inner } => inner,
                Expr::IsNull { operand: inner, negated } => arena
                    .alloc(Expr::IsNull { operand: inner, negated: !negated })
                    .map_err(|_| arena_full())?,
                _ => return Ok(e),
            };
            simplify_qual(negated, arena)
        }
        _ => Ok(e),
    }
}

fn postpone_cost(e: &Expr, scope: &QueryScope, arena: &Arena) -> u32 {
    use Expr::*;
    // PostgreSQL costs the *plan-time-folded* expression: a fully-constant
    // subtree has become a Const by then and costs nothing.
    if e.is_constant() {
        return 0;
    }
    let oid_of = |x: &Expr| -> Option<i32> {
        super::exec::infer_type_res(x, &ScopeCols(scope)).ok().map(|t| t.0)
    };
    // Numeric-family promotion rank: the lower-ranked operand is the one
    // PostgreSQL casts (int → numeric → float8).
    let rank = |o: i32| -> Option<u32> {
        use super::types::oid;
        Some(match o {
            oid::INT2 => 0,
            oid::INT4 => 1,
            oid::INT8 => 2,
            oid::NUMERIC => 3,
            oid::FLOAT4 => 4,
            oid::FLOAT8 => 5,
            _ => return None,
        })
    };
    // One implicit cast (2 half-ops) when a numeric-family pair mixes types —
    // free when the coerced side is a constant, which PostgreSQL folds into a
    // pre-cast Const at plan time.
    let coercion = |l: &Expr, r: &Expr| -> u32 {
        match (oid_of(l).and_then(rank), oid_of(r).and_then(rank)) {
            (Some(a), Some(b)) if a != b => {
                let coerced = if a < b { l } else { r };
                if coerced.is_constant() { 0 } else { 2 }
            }
            _ => 0,
        }
    };
    match e {
        Null | Bool(_) | Int(_) | Float(_) | NumericLit(_) | Str(_) | BitLit(_) | Param(_)
        | DefaultMarker | Column { .. } | WholeRow(_) => 0,
        Unary { operator: super::ast::UnaryOp::Not, operand } => postpone_cost(operand, scope, arena),
        Unary { operand, .. } => postpone_cost(operand, scope, arena) + 2,
        IsNull { operand, .. } => postpone_cost(operand, scope, arena),
        Cast { operand, .. } => postpone_cost(operand, scope, arena) + 2,
        Binary { operator: super::ast::BinaryOp::And | super::ast::BinaryOp::Or, left, right } => {
            postpone_cost(left, scope, arena) + postpone_cost(right, scope, arena)
        }
        Binary { left, right, .. } => {
            postpone_cost(left, scope, arena) + postpone_cost(right, scope, arena) + 2 + coercion(left, right)
        }
        Between { operand, low, high, .. } => {
            postpone_cost(operand, scope, arena)
                + postpone_cost(low, scope, arena)
                + postpone_cost(high, scope, arena)
                + 4
                + coercion(operand, low)
                + coercion(operand, high)
        }
        InList { operand, list, .. } => {
            // PostgreSQL rewrites a one-element IN to plain `=` (one operator);
            // longer lists cost half an operator per element (= ANY(array)).
            let applications = if list.len() <= 1 { 2 } else { list.len() as u32 };
            postpone_cost(operand, scope, arena)
                + list.iter().map(|x| postpone_cost(x, scope, arena)).sum::<u32>()
                + applications
        }
        Like { operand, pattern, .. } | Match { operand, pattern, .. } => {
            postpone_cost(operand, scope, arena) + postpone_cost(pattern, scope, arena) + 2
        }
        Call { name, args, .. } => {
            // GREATEST/LEAST/COALESCE unify their arguments' types, and
            // PostgreSQL charges one operator for each argument it has to cast
            // (a constant is pre-cast at plan time, as in the CASE arm below).
            // GREATEST and LEAST are a MinMaxExpr, which costs one operator of
            // its own; COALESCE is a CoalesceExpr, which like CASE costs
            // nothing beyond its casts.
            let unifying = name.eq_ignore_ascii_case("greatest")
                || name.eq_ignore_ascii_case("least")
                || name.eq_ignore_ascii_case("coalesce");
            let node = if name.eq_ignore_ascii_case("coalesce") { 0 } else { 2 };
            let mut c = args.iter().map(|a| postpone_cost(a, scope, arena)).sum::<u32>() + node;
            if unifying {
                let unified = oid_of(e).and_then(rank);
                for a in *args {
                    if a.is_constant() {
                        continue;
                    }
                    match (oid_of(a).and_then(rank), unified) {
                        (Some(x), Some(y)) if x != y => c += 2,
                        _ => {}
                    }
                }
            }
            c
        }
        Case { operand, whens, otherwise } => {
            let mut c = operand.map_or(0, |o| postpone_cost(o, scope, arena));
            // Non-constant branch results whose type differs from the CASE's
            // unified result type carry an implicit cast, which PostgreSQL
            // counts (a constant result is pre-cast at plan time).
            let case_rank = oid_of(e).and_then(rank);
            let result_cast = |result: &Expr| -> u32 {
                if result.is_constant() {
                    return 0;
                }
                match (oid_of(result).and_then(rank), case_rank) {
                    (Some(a), Some(b)) if a != b => 2,
                    _ => 0,
                }
            };
            for (cond, result) in whens.iter() {
                // PostgreSQL's plan-time simplification drops a WHEN whose
                // condition folds to constant FALSE, and truncates the CASE at
                // one folding to constant TRUE.
                match plan_time_bool(cond, arena) {
                    Some(false) => continue,
                    Some(true) => {
                        c += postpone_cost(result, scope, arena) + result_cast(result);
                        return c;
                    }
                    None => {}
                }
                c += postpone_cost(cond, scope, arena) + postpone_cost(result, scope, arena);
                c += result_cast(result);
                // The simple form compares the operand per WHEN.
                if operand.is_some() {
                    c += 2;
                }
            }
            if let Some(o) = otherwise {
                c += postpone_cost(o, scope, arena) + result_cast(o);
            }
            c
        }
        Array(items) => items.iter().map(|x| postpone_cost(x, scope, arena)).sum(),
        Subscript { base, index } => postpone_cost(base, scope, arena) + postpone_cost(index, scope, arena),
        Field { base, .. } => postpone_cost(base, scope, arena),
        AnyAll { operand, array, .. } => {
            let elements = if let Array(items) = array { items.len() as u32 } else { 20 };
            postpone_cost(operand, scope, arena) + postpone_cost(array, scope, arena) + elements
        }
        // Subqueries carry a subplan's cost in PostgreSQL and are postponed.
        Subquery(_) | Exists(_) | ArraySubquery(_) | InSubquery { .. } => 1000,
    }
}


/// Order helpers exported for update/delete WHERE-subquery support.
pub fn subquery_hooks<'a>(
    exprs: &[Option<&'a Expr<'a>>],
    storage: &'a Storage,
    txid: u32,
    arena: &'a Arena,
    params: &[Datum<'a>],
) -> Result<SubqueryValues<'a, 'a>, SqlError> {
    prepare_subqueries(exprs, storage, txid, arena, params, SUBQUERY_DEPTH, None)
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

/// Synthesizes a `TableDef` for a derived table (`FROM (SELECT ...) exposed`)
/// from the subquery's output column names and inferred types. Schema only —
/// no rows are produced, so it needs neither a txid nor bound parameters.
fn synth_derived_def<'a>(
    storage: &'a Storage,
    sub: &'a Select<'a>,
    exposed: &'a str,
    col_alias: Option<&'a [&'a str]>,
    txid: u32,
    arena: &'a Arena,
) -> Result<&'a TableDef, SqlError> {
    let mut descriptors = [ColDesc::new("", 0, 0); MAX_PROJ];
    let n_cols = match sub.set_body {
        Some(tree) => describe_set_body(storage, tree, txid, &mut descriptors, arena)?,
        None => match &sub.from {
            Some(f) => {
                let ss = QueryScope::resolve_schema(storage, f, txid, arena)?;
                let n = describe_scope_items(sub.items, &ss, &mut descriptors)?;
                // A bare scalar/array subquery item (possibly correlated) has
                // no static type from the scope and describes as text; infer
                // its real type from the inner select's projection so the
                // derived-table column is typed correctly.
                let mut slot = 0usize;
                for item in sub.items {
                    match item {
                        SelectItem::Wildcard => slot += ss.star_columns(),
                        SelectItem::TableWildcard(q) => {
                            slot += ss.defs[ss.table_index(q)?].expect("resolved").n_columns;
                        }
                        SelectItem::RecordStar(base) => slot += record_star_width(base, &ss),
                        SelectItem::Expr { expression, .. } => {
                            if slot < n
                                && descriptors[slot].type_oid == super::types::oid::TEXT
                                && let Expr::Subquery(inner_sub) = &**expression
                                && let Some(SelectItem::Expr { expression: inner, .. }) =
                                    inner_sub.items.first()
                            {
                                let inner_scope = inner_sub.from.as_ref().and_then(|inf| {
                                    QueryScope::resolve_schema(storage, inf, txid, arena).ok()
                                });
                                let witness = subquery_witness(
                                    inner,
                                    inner_scope.as_ref().or(Some(&ss)),
                                );
                                if !witness.is_null() {
                                    descriptors[slot] = ColDesc::new(
                                        descriptors[slot].name,
                                        witness.type_oid(),
                                        -1,
                                    );
                                }
                            }
                            slot += 1;
                        }
                    }
                }
                n
            }
            None => describe_items(sub.items, None, &mut descriptors)?,
        },
    };
    if n_cols > MAX_COLUMNS {
        return Err(sql_err!(
            "54011",
            "derived table \"{}\" has too many columns",
            exposed
        ));
    }
    // A column-alias list renames the output columns; PostgreSQL requires it to
    // supply no more names than the derived table has columns.
    if let Some(aliases) = col_alias {
        if aliases.len() > n_cols {
            return Err(sql_err!(
                "42P10",
                "table \"{}\" has {} columns available but {} columns specified",
                exposed,
                n_cols,
                aliases.len()
            ));
        }
        for (i, alias) in aliases.iter().enumerate() {
            descriptors[i].name = alias;
        }
    }
    let blank = ColumnMeta {
        name: SqlName::parse("").expect("empty name is valid"),
        ctype: ColType::Bool,
        type_mod: -1,
        not_null: false,
        unique: false,
        primary: false,
        auto_increment: false,
        default_value: None,
    };
    let mut columns = [blank; MAX_COLUMNS];
    for i in 0..n_cols {
        let ct = super::exec::coltype_of_oid(descriptors[i].type_oid).ok_or_else(|| {
            sql_err!(
                sqlstate::FEATURE_NOT_SUPPORTED,
                "derived table column \"{}\" type (oid {}) is not supported",
                descriptors[i].name,
                descriptors[i].type_oid
            )
        })?;
        columns[i] = ColumnMeta {
            name: SqlName::parse(descriptors[i].name)?,
            ctype: ct,
            ..blank
        };
    }
    let def = TableDef {
        name: SqlName::parse(exposed)?,
        columns,
        n_columns: n_cols,
        ..TableDef::empty()
    };
    Ok(&*arena.alloc(def).map_err(|_| arena_full())?)
}

/// Synthesizes the single-column `TableDef` for a supported table function
/// (`FROM func(args) alias`). The output column is named after the alias (or the
/// function name), so a bare reference to the alias resolves to the value.
fn table_func_def<'a>(
    tref: &'a TableRef<'a>,
    arena: &'a Arena,
    params: &[Datum<'a>],
) -> Result<&'a TableDef, SqlError> {
    let is_gs = tref.table.eq_ignore_ascii_case("generate_series");
    let is_unnest = tref.table.eq_ignore_ascii_case("unnest");
    let is_re = tref.table.eq_ignore_ascii_case("regexp_matches");
    let is_keys = tref.table.eq_ignore_ascii_case("jsonb_object_keys")
        || tref.table.eq_ignore_ascii_case("json_object_keys");
    let is_elems = tref.table.eq_ignore_ascii_case("jsonb_array_elements")
        || tref.table.eq_ignore_ascii_case("json_array_elements")
        || tref.table.eq_ignore_ascii_case("jsonb_array_elements_text")
        || tref.table.eq_ignore_ascii_case("json_array_elements_text");
    let is_each = is_json_each_name(tref.table);
    let is_rstt = tref.table.eq_ignore_ascii_case("regexp_split_to_table");
    let is_gsub = tref.table.eq_ignore_ascii_case("generate_subscripts");
    if !is_gs && !is_unnest && !is_re && !is_keys && !is_elems && !is_each && !is_rstt && !is_gsub {
        return Err(sql_err!(
            sqlstate::FEATURE_NOT_SUPPORTED,
            "table function \"{}\" is not supported",
            tref.table
        ));
    }
    let base_name = if is_gs {
        "generate_series"
    } else if is_re {
        "regexp_matches"
    } else if is_keys || is_elems || is_each || is_rstt || is_gsub {
        tref.table
    } else {
        "unnest"
    };
    let name = tref.alias.unwrap_or(base_name);
    let blank = ColumnMeta {
        name: SqlName::parse("").expect("empty name is valid"),
        ctype: ColType::Bool,
        type_mod: -1,
        not_null: false,
        unique: false,
        primary: false,
        auto_increment: false,
        default_value: None,
    };
    // Each supported function's output columns: `key`/`value` for the `each`
    // family (two columns), a single column named per the function otherwise.
    // generate_series yields int8; regexp_matches yields text[]; unnest yields
    // the array's element type; array_elements' default column is `value`.
    let mut default_cols: [(&str, ColType); 2] = [("", ColType::Bool); 2];
    let n_default = if is_each {
        let value_type = if tref.table.eq_ignore_ascii_case("json_each") {
            ColType::Json
        } else if tref.table.eq_ignore_ascii_case("jsonb_each") {
            ColType::Jsonb
        } else {
            ColType::Text // json_each_text / jsonb_each_text
        };
        default_cols[0] = ("key", ColType::Text);
        default_cols[1] = ("value", value_type);
        2
    } else {
        let single_type = if is_gs {
            // Integer series → int8; a date/timestamp start makes it temporal.
            match tref.func_args.and_then(|a| a.first()) {
                Some(e) => match super::eval::eval(e, arena, params, &super::eval::NoColumns)? {
                    Datum::Timestamp(_) => ColType::Timestamp,
                    Datum::Timestamptz(_) | Datum::Date(_) => ColType::Timestamptz,
                    _ => ColType::Int8,
                },
                None => ColType::Int8,
            }
        } else if is_gsub {
            ColType::Int4
        } else if is_re {
            ColType::Array(super::types::ArrElem::Text)
        } else if is_keys || is_rstt {
            ColType::Text
        } else if is_elems {
            if tref.table.eq_ignore_ascii_case("json_array_elements") {
                ColType::Json
            } else if tref.table.eq_ignore_ascii_case("jsonb_array_elements") {
                ColType::Jsonb
            } else {
                ColType::Text
            }
        } else {
            let args = tref.func_args.unwrap_or(&[]);
            match args.first() {
                Some(e) => match super::eval::eval(e, arena, params, &super::eval::NoColumns)? {
                    Datum::Array { element, .. } => element.to_coltype(),
                    _ => ColType::Text,
                },
                None => ColType::Text,
            }
        };
        // A single-column function's default column name is `value` for
        // array_elements, else the (aliased) function name.
        default_cols[0] = (if is_elems { "value" } else { name }, single_type);
        1
    };
    // `WITH ORDINALITY` appends a `bigint` ordinality column.
    let n_out = if tref.with_ordinality { n_default + 1 } else { n_default };
    // Column aliases rename the columns positionally; too many is an error.
    if let Some(aliases) = tref.col_alias
        && aliases.len() > n_out
    {
        return Err(sql_err!(
            "42P10",
            "table \"{}\" has {} columns available but {} columns specified",
            name,
            n_out,
            aliases.len()
        ));
    }
    let mut columns = [blank; MAX_COLUMNS];
    for (i, (default_name, ctype)) in default_cols[..n_default].iter().enumerate() {
        let col_name = tref
            .col_alias
            .and_then(|a| a.get(i).copied())
            .unwrap_or(default_name);
        columns[i] = ColumnMeta { name: SqlName::parse(col_name)?, ctype: *ctype, ..blank };
    }
    if tref.with_ordinality {
        let col_name = tref
            .col_alias
            .and_then(|a| a.get(n_default).copied())
            .unwrap_or("ordinality");
        columns[n_default] = ColumnMeta { name: SqlName::parse(col_name)?, ctype: ColType::Int8, ..blank };
    }
    let def = TableDef {
        name: SqlName::parse(name)?,
        columns,
        n_columns: n_out,
        ..TableDef::empty()
    };
    Ok(&*arena.alloc(def).map_err(|_| arena_full())?)
}

/// Whether `name` is one of the two-column `json_each` set-returning functions.
fn is_json_each_name(name: &str) -> bool {
    name.eq_ignore_ascii_case("json_each")
        || name.eq_ignore_ascii_case("jsonb_each")
        || name.eq_ignore_ascii_case("json_each_text")
        || name.eq_ignore_ascii_case("jsonb_each_text")
}

/// Materializes a table function's rows. Currently `generate_series(start, stop
/// [, step])` over integers; the arguments are evaluated as constants (a lateral
/// argument referencing an outer column surfaces loudly as an unresolved
/// column). Each row is one `int8` value, projected-encoded.
fn table_func_rows<'a>(
    tref: &'a TableRef<'a>,
    arena: &'a Arena,
    params: &[Datum<'a>],
) -> Result<&'a [&'a [u8]], SqlError> {
    let args = tref.func_args.expect("table function carries arguments");
    // json_each / jsonb_each[_text]: one (key, value) row per object member.
    if is_json_each_name(tref.table) {
        let jsonb = tref.table.eq_ignore_ascii_case("jsonb_each")
            || tref.table.eq_ignore_ascii_case("jsonb_each_text");
        let as_text = tref.table.eq_ignore_ascii_case("json_each_text")
            || tref.table.eq_ignore_ascii_case("jsonb_each_text");
        let text = match super::eval::eval(args[0], arena, params, &super::eval::NoColumns)? {
            Datum::Json { text, .. } => text,
            Datum::Text(s) => s,
            Datum::Null => return Ok(&[]),
            _ => return Err(sql_err!(sqlstate::INVALID_PARAMETER_VALUE, "cannot deconstruct a scalar")),
        };
        let pairs = super::eval::json_each_pairs(text, jsonb, as_text, arena)?;
        const EMPTY: &[u8] = &[];
        let rows = arena.alloc_slice_with(pairs.len(), |_| EMPTY).map_err(|_| arena_full())?;
        for (slot, (key, value)) in rows.iter_mut().zip(pairs.iter()) {
            *slot = super::exec::encode_projected_pub(&[Datum::Text(key), *value], arena)?;
        }
        return Ok(&*rows);
    }
    // regexp_split_to_table(string, pattern [, flags]): one text row per piece.
    if tref.table.eq_ignore_ascii_case("regexp_split_to_table") {
        if !(2..=3).contains(&args.len()) {
            return Err(sql_err!(sqlstate::UNDEFINED_FUNCTION, "regexp_split_to_table(...) argument count"));
        }
        let (src, pat) = match (
            super::eval::eval(args[0], arena, params, &super::eval::NoColumns)?,
            super::eval::eval(args[1], arena, params, &super::eval::NoColumns)?,
        ) {
            (Datum::Text(s), Datum::Text(p)) => (s, p),
            (Datum::Null, _) | (_, Datum::Null) => return Ok(&[]),
            (a, _) => return Err(super::eval::type_mismatch_pub("regexp_split_to_table", &a)),
        };
        let case_insensitive = if args.len() == 3 {
            match super::eval::eval(args[2], arena, params, &super::eval::NoColumns)? {
                Datum::Text(f) => super::eval::regexp_flags(f)?.1,
                Datum::Null => return Ok(&[]),
                _ => false,
            }
        } else {
            false
        };
        let pieces = super::eval::regex_split_pub(src, pat, case_insensitive, arena)?;
        const EMPTY: &[u8] = &[];
        let rows = arena.alloc_slice_with(pieces.len(), |_| EMPTY).map_err(|_| arena_full())?;
        for (slot, piece) in rows.iter_mut().zip(pieces.iter()) {
            *slot = super::exec::encode_projected_pub(&[*piece], arena)?;
        }
        return Ok(&*rows);
    }
    // generate_subscripts(array, dim): the 1-based indices of `array` along
    // `dim`; empty for a dim other than 1 (arrays are one-dimensional here).
    if tref.table.eq_ignore_ascii_case("generate_subscripts") {
        if args.len() != 2 {
            return Err(sql_err!(sqlstate::UNDEFINED_FUNCTION, "generate_subscripts(...) argument count"));
        }
        let raw = match super::eval::eval(args[0], arena, params, &super::eval::NoColumns)? {
            Datum::Array { raw, .. } => raw,
            Datum::Null => return Ok(&[]),
            a => return Err(super::eval::type_mismatch_pub("generate_subscripts", &a)),
        };
        let dim = match super::eval::eval(args[1], arena, params, &super::eval::NoColumns)? {
            Datum::Int4(v) => v as i64,
            Datum::Int8(v) => v,
            Datum::Null => return Ok(&[]),
            a => return Err(super::eval::type_mismatch_pub("generate_subscripts", &a)),
        };
        let count = if dim == 1 { super::array::len(raw) } else { 0 };
        const EMPTY: &[u8] = &[];
        let rows = arena.alloc_slice_with(count, |_| EMPTY).map_err(|_| arena_full())?;
        for (i, slot) in rows.iter_mut().enumerate() {
            *slot = super::exec::encode_projected_pub(&[Datum::Int4((i + 1) as i32)], arena)?;
        }
        return Ok(&*rows);
    }
    // regexp_matches(string, pattern [, flags]): one row per match, each a
    // text[] of the capture groups (or the whole match when there are no groups).
    if tref.table.eq_ignore_ascii_case("regexp_matches") {
        if !(2..=3).contains(&args.len()) {
            return Err(sql_err!(sqlstate::UNDEFINED_FUNCTION, "regexp_matches(...) argument count"));
        }
        let string = super::eval::eval(args[0], arena, params, &super::eval::NoColumns)?;
        let pattern = super::eval::eval(args[1], arena, params, &super::eval::NoColumns)?;
        let (Datum::Text(string), Datum::Text(pattern)) = (string, pattern) else {
            return Ok(&[]);
        };
        let flags = if args.len() == 3 {
            match super::eval::eval(args[2], arena, params, &super::eval::NoColumns)? {
                Datum::Text(f) => f,
                Datum::Null => return Ok(&[]),
                _ => "",
            }
        } else {
            ""
        };
        let (global, ci) = super::eval::regexp_flags(flags)?;
        // Collect each match's encoded text[] row.
        const EMPTY: &[u8] = &[];
        let mut rows = [EMPTY; super::parser::MAX_LIST];
        let mut n = 0usize;
        let mut spans = [(-1i64, -1i64); super::regex::MAX_GROUPS];
        let mut from = 0usize;
        while let Some(((mstart, mend), ng)) =
            super::regex::find_captures(pattern, string, from, ci, &mut spans)?
        {
            let mut elems = [Datum::Null; super::regex::MAX_GROUPS];
            let count = if ng == 0 {
                elems[0] = Datum::Text(&string[mstart..mend]);
                1
            } else {
                for (i, span) in spans[..ng].iter().enumerate() {
                    elems[i] = if span.0 < 0 {
                        Datum::Null
                    } else {
                        Datum::Text(&string[span.0 as usize..span.1 as usize])
                    };
                }
                ng
            };
            let arr = Datum::Array {
                element: super::types::ArrElem::Text,
                raw: super::array::build(&elems[..count], arena)?,
            };
            if n == super::parser::MAX_LIST {
                return Err(sql_err!(sqlstate::PROGRAM_LIMIT_EXCEEDED, "too many regexp_matches rows"));
            }
            rows[n] = super::exec::encode_projected_pub(&[arr], arena)?;
            n += 1;
            if !global {
                break;
            }
            from = if mend > mstart { mend } else { mend + 1 };
            if from > string.len() {
                break;
            }
        }
        let out = arena.alloc_slice_with(n, |i| rows[i]).map_err(|_| arena_full())?;
        return Ok(&*out);
    }
    // jsonb_object_keys(obj) / json_object_keys(obj): one text row per key.
    if tref.table.eq_ignore_ascii_case("jsonb_object_keys")
        || tref.table.eq_ignore_ascii_case("json_object_keys")
    {
        let jsonb = tref.table.eq_ignore_ascii_case("jsonb_object_keys");
        let text = match super::eval::eval(args[0], arena, params, &super::eval::NoColumns)? {
            Datum::Json { text, .. } => text,
            Datum::Text(s) => s,
            Datum::Null => return Ok(&[]),
            _ => return Err(super::json::object_keys_error(tref.table, super::json::Kind::Scalar)),
        };
        let kind = super::json::kind_of(text);
        if kind != super::json::Kind::Object {
            return Err(super::json::object_keys_error(tref.table, kind));
        }
        const EMPTY: &[u8] = &[];
        // jsonb: normalized/sorted keys; json: source order with duplicates.
        if jsonb {
            let super::json::Json::Object(members) = super::json::parse(text, arena)? else {
                return Err(super::json::object_keys_error(tref.table, kind));
            };
            let rows = arena.alloc_slice_with(members.len(), |_| EMPTY).map_err(|_| arena_full())?;
            for (slot, (key, _)) in rows.iter_mut().zip(members.iter()) {
                *slot = super::exec::encode_projected_pub(&[Datum::Text(key)], arena)?;
            }
            return Ok(&*rows);
        }
        let members = super::json::object_members_source(text, arena)?;
        let rows = arena.alloc_slice_with(members.len(), |_| EMPTY).map_err(|_| arena_full())?;
        for (slot, (key, _)) in rows.iter_mut().zip(members.iter()) {
            *slot = super::exec::encode_projected_pub(&[Datum::Text(key)], arena)?;
        }
        return Ok(&*rows);
    }
    // jsonb_array_elements / json_array_elements[_text]: one row per element.
    if tref.table.eq_ignore_ascii_case("jsonb_array_elements")
        || tref.table.eq_ignore_ascii_case("json_array_elements")
        || tref.table.eq_ignore_ascii_case("jsonb_array_elements_text")
        || tref.table.eq_ignore_ascii_case("json_array_elements_text")
    {
        let jsonb = tref.table.eq_ignore_ascii_case("jsonb_array_elements")
            || tref.table.eq_ignore_ascii_case("jsonb_array_elements_text");
        let as_text = tref.table.eq_ignore_ascii_case("jsonb_array_elements_text")
            || tref.table.eq_ignore_ascii_case("json_array_elements_text");
        let text = match super::eval::eval(args[0], arena, params, &super::eval::NoColumns)? {
            Datum::Json { text, .. } => text,
            Datum::Text(s) => s,
            Datum::Null => return Ok(&[]),
            _ => return Err(super::json::array_elements_error(tref.table, jsonb, super::json::Kind::Scalar)),
        };
        let kind = super::json::kind_of(text);
        if kind != super::json::Kind::Array {
            return Err(super::json::array_elements_error(tref.table, jsonb, kind));
        }
        const EMPTY: &[u8] = &[];
        if jsonb {
            let super::json::Json::Array(items) = super::json::parse(text, arena)? else {
                return Err(super::json::array_elements_error(tref.table, jsonb, kind));
            };
            let rows = arena.alloc_slice_with(items.len(), |_| EMPTY).map_err(|_| arena_full())?;
            for (slot, element) in rows.iter_mut().zip(items.iter()) {
                let datum = if as_text {
                    match *element {
                        super::json::Json::Str(s) => {
                            Datum::Text(super::json::decode_string(s, arena)?)
                        }
                        super::json::Json::Null => Datum::Null,
                        _ => Datum::Text(super::eval::json_to_text_pub(element, arena)?),
                    }
                } else {
                    Datum::Json { text: super::eval::json_to_text_pub(element, arena)?, jsonb }
                };
                *slot = super::exec::encode_projected_pub(&[datum], arena)?;
            }
            return Ok(&*rows);
        }
        // json: each element's verbatim source text.
        let items = super::json::array_elements_source(text, arena)?;
        let rows = arena.alloc_slice_with(items.len(), |_| EMPTY).map_err(|_| arena_full())?;
        for (slot, element) in rows.iter_mut().zip(items.iter()) {
            let datum = if as_text {
                match super::json::parse(element, arena)? {
                    super::json::Json::Str(s) => Datum::Text(super::json::decode_string(s, arena)?),
                    super::json::Json::Null => Datum::Null,
                    _ => Datum::Text(element),
                }
            } else {
                Datum::Json { text: element, jsonb }
            };
            *slot = super::exec::encode_projected_pub(&[datum], arena)?;
        }
        return Ok(&*rows);
    }
    // unnest(array): one row per element.
    if tref.table.eq_ignore_ascii_case("unnest") {
        let (element, raw) = match super::eval::eval(args[0], arena, params, &super::eval::NoColumns)? {
            Datum::Array { element, raw } => (element, raw),
            Datum::Null => return Ok(&[]),
            _ => return Err(sql_err!(sqlstate::UNDEFINED_FUNCTION, "unnest requires an array argument")),
        };
        let count = super::array::len(raw);
        const EMPTY: &[u8] = &[];
        let rows = arena.alloc_slice_with(count, |_| EMPTY).map_err(|_| arena_full())?;
        for (i, slot) in rows.iter_mut().enumerate() {
            let v = super::array::get(raw, element, i).unwrap_or(Datum::Null);
            *slot = super::exec::encode_projected_pub(&[v], arena)?;
        }
        return Ok(&*rows);
    }
    if args.len() != 2 && args.len() != 3 {
        return Err(sql_err!(
            sqlstate::UNDEFINED_FUNCTION,
            "generate_series expects 2 or 3 arguments"
        ));
    }
    // Temporal series: a date/timestamp start with an interval step.
    let start_val = super::eval::eval(args[0], arena, params, &super::eval::NoColumns)?;
    if let Some((base, kind)) = super::eval::timestamp_series_start(&start_val) {
        if args.len() != 3 {
            return Err(sql_err!(sqlstate::UNDEFINED_FUNCTION, "generate_series over timestamps requires a step"));
        }
        // Coerce bare string literals for the stop and step (function resolution).
        let stop_val = super::eval::cast_to(
            super::eval::eval(args[1], arena, params, &super::eval::NoColumns)?,
            kind.coltype(),
            arena,
        )?;
        let step_val = super::eval::cast_to(
            super::eval::eval(args[2], arena, params, &super::eval::NoColumns)?,
            ColType::Interval,
            arena,
        )?;
        let (Some((stop_micros, _)), Datum::Interval(step_iv)) =
            (super::eval::timestamp_series_start(&stop_val), step_val)
        else {
            return Ok(&[]);
        };
        let count = super::eval::timestamp_series_count(base, stop_micros, step_iv)?;
        const EMPTY: &[u8] = &[];
        let rows = arena.alloc_slice_with(count, |_| EMPTY).map_err(|_| arena_full())?;
        let mut v = base;
        for slot in rows.iter_mut() {
            *slot = super::exec::encode_projected_pub(&[kind.datum(v)], arena)?;
            v = super::datetime::add_interval(v, step_iv);
        }
        return Ok(&*rows);
    }
    if start_val.is_null() {
        return Ok(&[]);
    }
    let as_i64 = |e: &'a Expr<'a>| -> Result<i64, SqlError> {
        match super::eval::eval(e, arena, params, &super::eval::NoColumns)? {
            Datum::Int4(v) => Ok(v as i64),
            Datum::Int8(v) => Ok(v),
            _ => Err(sql_err!(sqlstate::UNDEFINED_FUNCTION, "generate_series requires integer arguments")),
        }
    };
    let start = as_i64(args[0])?;
    let stop = as_i64(args[1])?;
    let step = if args.len() == 3 { as_i64(args[2])? } else { 1 };
    if step == 0 {
        return Err(sql_err!(sqlstate::INVALID_PARAMETER_VALUE, "step size cannot equal zero"));
    }
    let count = if step > 0 {
        if stop < start { 0 } else { ((stop - start) / step) as usize + 1 }
    } else if stop > start {
        0
    } else {
        ((start - stop) / (-step)) as usize + 1
    };
    const EMPTY: &[u8] = &[];
    let rows = arena.alloc_slice_with(count, |_| EMPTY).map_err(|_| arena_full())?;
    let mut v = start;
    for slot in rows.iter_mut() {
        *slot = super::exec::encode_projected_pub(&[Datum::Int8(v)], arena)?;
        v += step;
    }
    Ok(&*rows)
}
