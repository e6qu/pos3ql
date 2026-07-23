//! Subqueries: evaluating a nested SELECT where a scalar, a row, or a set is
//! expected.
//!
//! Two shapes, split by whether the inner query mentions the outer one. An
//! uncorrelated subquery is evaluated once, before the outer scan starts, and
//! its result substituted as a constant. A correlated one is re-evaluated per
//! outer row, with the outer row's columns chained onto the inner scope.

use crate::mem::arena::Arena;
use crate::sql::ast::{Expr, Select, SelectItem, SetTree};
use crate::sql::eval::{
    eval_full, sqlstate, ColumnLookup, EvalHooks, SqlError, SubqueryValues,
};
use crate::sql::types::{ColType, Datum, RecordField};
use crate::sql_err;
use crate::stack_format;
use crate::storage::Storage;

use crate::sql::exec::MAX_PROJ;
use super::setops::materialize_set_body;
use super::{
    arena_full, cmp_key_rows, collect_aggs, collect_windows, fold_aggregates,
    fromless_aggregate_hooks, scan_source, select_into_rows,
    where_passes, Chained, QueryScope, ScopeCols, ScopeSchema, MAX_AGGS, MAX_SUBQUERIES,
    MAX_WINDOWS, SUBQUERY_DEPTH,
};

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

pub(super) fn walk_children<'a>(
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
        Expr::Case { operand, whens, otherwise, .. } => {
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

/// `(a, b) IN (SELECT x, y FROM ...)`: PostgreSQL matches the row constructor
/// on the left against a row built from the subquery's columns. Rewriting the
/// subquery to project a single `row(...)` lets the one-column machinery below
/// and the record equality operator handle it unchanged; the arity check is the
/// one PostgreSQL reports, in its own words.
fn row_projected<'a>(
    operand: &'a Expr<'a>,
    select: &'a Select<'a>,
    arena: &'a Arena,
) -> Result<(&'a Select<'a>, usize), SqlError> {
    let arity = match operand {
        Expr::Call { name, args, .. } if name.eq_ignore_ascii_case("row") => args.len(),
        _ => 1,
    };
    // A set-operation body carries its columns in the leaves, all of equal
    // arity (checked where the branches are combined), so the first one speaks
    // for the whole tree.
    let columns = match select.set_body {
        Some(tree) => set_leaf(tree).items.len(),
        None => select.items.len(),
    };
    if columns < arity {
        return Err(sql_err!(sqlstate::SYNTAX_ERROR, "subquery has too few columns"));
    }
    if columns > arity {
        return Err(sql_err!(sqlstate::SYNTAX_ERROR, "subquery has too many columns"));
    }
    // A plain subquery is rewritten to project the row itself. A set operation
    // keeps its columns — records have no storage type to be encoded as, and
    // its branches must be combined and deduplicated column-wise anyway — so
    // the rows are assembled into records after materialization instead.
    if arity == 1 || select.set_body.is_some() {
        return Ok((select, arity));
    }
    Ok((row_select(select, arena)?, arity))
}

/// The leftmost leaf of a set tree, which fixes the arity of the whole tree.
fn set_leaf<'a>(tree: &'a SetTree<'a>) -> &'a Select<'a> {
    match tree {
        SetTree::Select(select) => select,
        SetTree::Op { left, .. } => set_leaf(left),
    }
}

/// Replaces a select's projection list with the single row it forms.
fn row_select<'a>(select: &'a Select<'a>, arena: &'a Arena) -> Result<&'a Select<'a>, SqlError> {
    let mut args = [&Expr::Null; MAX_PROJ];
    for (slot, item) in args.iter_mut().zip(select.items) {
        match item {
            SelectItem::Expr { expression, .. } => *slot = expression,
            _ => {
                return Err(sql_err!(
                    sqlstate::FEATURE_NOT_SUPPORTED,
                    "a wildcard is not supported in a row-comparison subquery"
                ))
            }
        }
    }
    let args = arena.alloc_slice_copy(&args[..select.items.len()]).map_err(|_| arena_full())?;
    let call = &*arena
        .alloc(Expr::Call {
            name: "row",
            args,
            star: false,
            distinct: false,
            order_by: &[],
            over: None,
            filter: None,
        })
        .map_err(|_| arena_full())?;
    let items = arena
        .alloc_slice_copy(&[SelectItem::Expr { expression: call, alias: None }])
        .map_err(|_| arena_full())?;
    let mut rewritten = *select;
    rewritten.items = items;
    Ok(&*arena.alloc(rewritten).map_err(|_| arena_full())?)
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
                    run_subquery(select, storage, txid, arena, params, depth, outer, 1)?;
                if values.len() > 1 {
                    return Err(sql_err!(
                        crate::sql::eval::sqlstate::CARDINALITY_VIOLATION,
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
                    run_subquery(select, storage, txid, arena, params, depth, outer, 1)?;
                let v = build_array_scalar(values, &witness, arena)?;
                scalars_tmp[n_scalars] = (*node as *const _, v, v);
                n_scalars += 1;
            }
            Expr::InSubquery { operand, select, .. } => {
                let (select, arity) = row_projected(operand, select, arena)?;
                let (values, saw_null, witness) =
                    run_subquery(select, storage, txid, arena, params, depth, outer, arity)?;
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
        return Err(sql_err!(sqlstate::STATEMENT_TOO_COMPLEX, "subqueries nested too deeply"));
    }
    if let Some(tree) = select.set_body {
        let (vals, _, _) = run_set_subquery(tree, select, storage, txid, arena, params, 1)?;
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
            let base = Chained { inner: &crate::sql::eval::NoColumns, outer };
            let hook_data = fromless_aggregate_hooks(
                select, &agg_nodes[..n_aggs], arena, params, &base, &hooks,
            )?;
            return Ok(hook_data.is_some());
        }
        if let Some(w) = select.where_clause {
            let base = Chained { inner: &crate::sql::eval::NoColumns, outer };
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
pub(super) struct OuterSubs<'a> {
    pub(super) base: SubqueryValues<'a, 'a>,
    pub(super) correlated: &'a [&'a Expr<'a>],
}

/// Splits a query's subqueries into uncorrelated (evaluated once here) and
/// correlated (deferred to per-row evaluation during the scan).
pub(super) fn prepare_outer_subqueries<'a>(
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
pub(super) fn merge_correlated<'a, 'b>(
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
                    run_subquery(select, storage, txid, arena, params, SUBQUERY_DEPTH, Some(outer), 1)?;
                if values.len() > 1 {
                    return Err(sql_err!(
                        crate::sql::eval::sqlstate::CARDINALITY_VIOLATION,
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
                    run_subquery(select, storage, txid, arena, params, SUBQUERY_DEPTH, Some(outer), 1)?;
                let v = build_array_scalar(values, &witness, arena)?;
                scalars[ns] = (*node as *const _, v, v);
                ns += 1;
            }
            Expr::InSubquery { operand, select, .. } => {
                let (select, arity) = row_projected(operand, select, arena)?;
                let (values, saw_null, witness) =
                    run_subquery(select, storage, txid, arena, params, SUBQUERY_DEPTH, Some(outer), arity)?;
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
        ColType::Text | ColType::Varchar | ColType::Bpchar | ColType::Name | ColType::Bytea | ColType::Numeric => {
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
pub(crate) fn subquery_witness(item: &Expr, scope: Option<&QueryScope>) -> Datum<'static> {
    let inferred = match scope {
        Some(s) => crate::sql::exec::infer_type_res(item, &ScopeCols(s)),
        None => crate::sql::exec::infer_type_res(item, &crate::sql::exec::NoCols),
    };
    let ct = inferred
        .ok()
        .and_then(|(o, _)| crate::sql::exec::coltype_of_oid(o))
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
    row_arity: usize,
) -> Result<(&'a [Datum<'a>], bool, Datum<'a>), SqlError> {
    if depth == 0 {
        return Err(sql_err!(sqlstate::STATEMENT_TOO_COMPLEX, "subqueries nested too deeply"));
    }
    if let Some(tree) = select.set_body {
        return run_set_subquery(tree, select, storage, txid, arena, params, row_arity);
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
                fromless_aggregate_hooks(select, &agg_nodes[..n_aggs], arena, params, &Chained { inner: &crate::sql::eval::NoColumns, outer }, &hooks)?
            else {
                return Ok((&[], false, subquery_witness(item, None)));
            };
            let agg_hooks = EvalHooks { aggs: Some((ptrs, values)), ..hooks };
            let base = Chained { inner: &crate::sql::eval::NoColumns, outer };
            let v = eval_full(item, arena, params, &base, &agg_hooks)?;
            let out = arena.alloc_slice_copy(&[v]).map_err(|_| arena_full())?;
            return Ok((&*out, v.is_null(), subquery_witness(item, None)));
        }
        let base = Chained { inner: &crate::sql::eval::NoColumns, outer };
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
    let offset = crate::sql::exec::eval_offset_pub(select.offset, arena, params)? as usize;
    let limit = crate::sql::exec::eval_limit_pub(select.limit, arena, params)?;
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
    row_arity: usize,
) -> Result<(&'a [Datum<'a>], bool, Datum<'a>), SqlError> {
    let (rows, target, n_cols) = materialize_set_body(storage, txid, tree, arena, params)?;
    if n_cols != row_arity {
        return Err(sql_err!(sqlstate::SYNTAX_ERROR, "subquery must return only one column"));
    }
    if row_arity > 1 {
        return set_record_rows(rows, &target[..n_cols], outer_select, arena, params);
    }
    let (start, n) = set_window(rows.len(), outer_select, arena, params)?;
    let mut saw_null = false;
    let out = arena
        .alloc_slice_with(n, |i| {
            let v = crate::sql::exec::decode_projected_pub(rows[start + i], 0);
            if v.is_null() {
                saw_null = true;
            }
            v
        })
        .map_err(|_| arena_full())?;
    Ok((&*out, saw_null, type_witness(target[0])))
}

/// The row-comparison form of the above: each materialized row becomes a
/// `Datum::Record` of its columns, so `(a, b) IN (SELECT x, y ... UNION ...)`
/// compares against the same record the left-hand `ROW(...)` builds. The
/// branches were already combined and deduplicated column-wise, which is where
/// a record — having no storage type — could not have gone.
fn set_record_rows<'a>(
    rows: &'a [&'a [u8]],
    target: &[ColType],
    outer_select: &'a Select<'a>,
    arena: &'a Arena,
    params: &[Datum<'a>],
) -> Result<(&'a [Datum<'a>], bool, Datum<'a>), SqlError> {
    let (start, n) = set_window(rows.len(), outer_select, arena, params)?;
    let mut names = [""; MAX_PROJ];
    for (column, name) in names[..target.len()].iter_mut().enumerate() {
        let text = stack_format!(12, "f{}", column + 1);
        *name = arena.alloc_str(text.as_str()).map_err(|_| arena_full())?;
    }
    let record = |values: &mut dyn FnMut(usize) -> Datum<'a>| -> Result<Datum<'a>, SqlError> {
        let mut fields = [RecordField { name: "", type_oid: 0, value: Datum::Null }; MAX_PROJ];
        for (column, field) in fields[..target.len()].iter_mut().enumerate() {
            let value = values(column);
            *field = RecordField { name: names[column], type_oid: value.type_oid(), value };
        }
        let fields = arena.alloc_slice_copy(&fields[..target.len()]).map_err(|_| arena_full())?;
        Ok(Datum::Record(&*fields))
    };
    let out = arena.alloc_slice_with(n, |_| Datum::Null).map_err(|_| arena_full())?;
    for (i, slot) in out.iter_mut().enumerate() {
        *slot = record(&mut |column| {
            crate::sql::exec::decode_projected_pub(rows[start + i], column)
        })?;
    }
    // The witness types the IN operand before any comparison, so it is built
    // from the column types rather than from a row that may not exist.
    let witness = record(&mut |column| type_witness(target[column]))?;
    // A record itself is never null here; a null *field* makes an individual
    // membership comparison unknown, which `membership_eq` decides.
    Ok((&*out, false, witness))
}

/// The `OFFSET` / `LIMIT` trailing a set operation, as a `(start, count)`
/// window over the materialized rows.
fn set_window<'a>(
    total: usize,
    outer_select: &'a Select<'a>,
    arena: &'a Arena,
    params: &[Datum<'a>],
) -> Result<(usize, usize), SqlError> {
    let offset = crate::sql::exec::eval_offset_pub(outer_select.offset, arena, params)?;
    let limit = crate::sql::exec::eval_limit_pub(outer_select.limit, arena, params)?;
    let start = (offset as usize).min(total);
    Ok((start, ((total - start) as u64).min(limit) as usize))
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
    let element = crate::sql::types::ArrElem::from_datum(witness)
        .or_else(|| values.iter().find_map(crate::sql::types::ArrElem::from_datum))
        .unwrap_or(crate::sql::types::ArrElem::Text);
    let ct = element.to_coltype();
    let buffer = arena
        .alloc_slice_with(values.len(), |i| values[i])
        .map_err(|_| arena_full())?;
    for v in buffer.iter_mut() {
        if !v.is_null() {
            *v = crate::sql::eval::cast_to(*v, ct, arena)?;
        }
    }
    Ok(Datum::Array { element, raw: crate::sql::array::build(buffer, arena)? })
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
