//! Grouped and aggregate execution: `GROUP BY`, grouping sets, and `HAVING`.
//!
//! One scan collects the distinct group keys, a second folds each group's
//! aggregates, and `HAVING` filters the result — sort-based throughout, so no
//! hash table is needed and no key encoding has to survive between phases.
//! `GROUPING SETS` (and the `ROLLUP` / `CUBE` spellings) run this per set, with
//! the columns a set collapses reading NULL.

use crate::mem::arena::Arena;
use crate::pg::respond::Responder;
use crate::sql::ast::{Expr, FromClause, Select, SelectItem};
use crate::sql::eval::{
    compare_datums, eval_full, sqlstate, ColumnLookup, EvalHooks, SqlError,
};
use crate::sql::types::Datum;
use crate::sql_err;
use crate::stack_format;

use crate::sql::exec::MAX_PROJ;

use super::aggregate::AggState;
use super::materialize::visible_prefix;
use super::plan::where_passes;
use super::subquery::merge_correlated;
use super::{
    arena_full, expr_contains_node, resolve_order_target, scan_source, sql_fail, sql_ok, JoinRow,
    Outcome, QueryScope, ScopeSchema, MAX_AGGS, MAX_SUBQUERIES,
};
use crate::storage::Storage;

/// With correlated subqueries in WHERE, the filter cannot run inside
/// `scan_source` (their values change per row): re-evaluate the correlated
/// nodes against this row, then apply WHERE under the merged hooks. With no
/// correlated subqueries this is a no-op (the scan already filtered).
#[expect(clippy::too_many_arguments, reason = "query pipeline plumbing")]
pub(super) fn row_passes_correlated_where<'a>(
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
pub(super) fn groups_for_mask<'a>(
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
                keys[at].0 = crate::sql::exec::encode_projected_pub(&key_vals[..n_keys], arena)?;
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
                *slot = crate::sql::exec::decode_projected_pub(rep.0, k);
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
            crate::sql::exec::encode_projected_pub(&full[..width + n_order], arena)?;
        survivors += 1;
    }
    Ok(&out_rows[..survivors])
}

/// The row-producing half of grouped/aggregate execution: runs the scans,
/// folds aggregates per group, applies HAVING, and returns the surviving
/// output rows (self-describing-encoded, `width` visible columns followed by
/// `n_order` hidden ORDER BY key columns) sorted by any ORDER BY. The caller
/// applies LIMIT/OFFSET and emits. Shared by the wire path (`grouped_select`)
/// and the row-source path (`select_into_rows`). A single scan collecting
/// encoded (key, agg-argument) pairs is avoided in favour of one scan per
/// phase — group keys, then row-by-row aggregate folding — and is sort-based.
#[expect(clippy::too_many_arguments, reason = "query pipeline plumbing")]
pub(super) fn grouped_rows<'a>(
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
        if let Some(column) = ungrouped_column(expression, statement.group_by) {
            return Err(ungrouped_error(column, scope));
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
    if masks.len() > crate::sql::parser::MAX_GROUPING_SETS {
        return Err(sql_err!(sqlstate::PROGRAM_LIMIT_EXCEEDED, "too many grouping sets"));
    }

    // Aggregate each set independently, then concatenate (a single set is a
    // straight copy). ORDER BY applies across the combined result, so it is
    // deferred until after concatenation.
    let empty_rows: &[&[u8]] = &[];
    let mut per_set: [&[&[u8]]; crate::sql::parser::MAX_GROUPING_SETS] =
        [empty_rows; crate::sql::parser::MAX_GROUPING_SETS];
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
                let ka = crate::sql::exec::decode_projected_pub(a, width + k);
                let kb = crate::sql::exec::decode_projected_pub(b, width + k);
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
pub(super) fn grouped_select<'a>(
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
            *slot = crate::sql::exec::decode_projected_pub(row, i);
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
/// PostgreSQL's ungrouped-column error, naming the column as `table.column`
/// however the query spelled it.
fn ungrouped_error(column: &Expr, scope: &QueryScope) -> SqlError {
    let (qualifier, name) = match column {
        Expr::Column { qualifier, name } => (*qualifier, *name),
        _ => (None, ""),
    };
    let table = match scope.find_column(qualifier, name) {
        Ok(super::ResolvedColumn::Table(t, _)) => Some(scope.names[t]),
        _ => qualifier,
    };
    match table {
        Some(table) => sql_err!(
            "42803",
            "column \"{}.{}\" must appear in the GROUP BY clause or be used in an aggregate function",
            table,
            name
        ),
        None => sql_err!(
            "42803",
            "column \"{}\" must appear in the GROUP BY clause or be used in an aggregate function",
            name
        ),
    }
}

/// The first column reference in `expression` that no grouping key covers, or
/// `None` when every one of them is. PostgreSQL names that column in the
/// ungrouped-column error, so this reports which one rather than only whether
/// there was one; a whole expression matching a grouping key is covered
/// outright, and an aggregate's arguments are covered by the aggregate.
fn ungrouped_column<'e, 'a>(
    expression: &'e Expr<'a>,
    group_by: &[&Expr<'a>],
) -> Option<&'e Expr<'a>> {
    if group_by.iter().any(|g| **g == *expression) || expression.is_aggregate() {
        return None;
    }
    let first =
        |parts: &[&'e Expr<'a>]| parts.iter().find_map(|e| ungrouped_column(e, group_by));
    match expression {
        Expr::Column { .. } | Expr::WholeRow(_) => Some(expression),
        Expr::Null | Expr::Bool(_) | Expr::Int(_) | Expr::Float(_) | Expr::NumericLit(_) | Expr::Str(_)
        | Expr::BitLit(_) | Expr::Param(_) | Expr::DefaultMarker | Expr::Subquery(_) | Expr::Exists(_)
        | Expr::ArraySubquery(_) => None,
        Expr::Unary { operand, .. }
        | Expr::Cast { operand, .. }
        | Expr::IsNull { operand, .. }
        | Expr::InSubquery { operand, .. } => ungrouped_column(operand, group_by),
        Expr::Binary { left, right, .. } => first(&[left, right]),
        Expr::Call { args, .. } => args.iter().find_map(|a| ungrouped_column(a, group_by)),
        Expr::InList { operand, list, .. } => ungrouped_column(operand, group_by)
            .or_else(|| list.iter().find_map(|e| ungrouped_column(e, group_by))),
        Expr::Between { operand, low, high, .. } => first(&[operand, low, high]),
        Expr::Like { operand, pattern, .. } | Expr::Match { operand, pattern, .. } => {
            first(&[operand, pattern])
        }
        Expr::Case { operand, whens, otherwise } => operand
            .and_then(|o| ungrouped_column(o, group_by))
            .or_else(|| {
                whens.iter().find_map(|(c, r)| first(&[c, r]))
            })
            .or_else(|| otherwise.and_then(|o| ungrouped_column(o, group_by))),
        Expr::Array(items) => items.iter().find_map(|e| ungrouped_column(e, group_by)),
        Expr::Subscript { base, index } => first(&[base, index]),
        Expr::Field { base, .. } => ungrouped_column(base, group_by),
        Expr::AnyAll { operand, array, .. } => first(&[operand, array]),
    }
}
