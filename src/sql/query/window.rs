//! Window-function execution: frame computation and per-row projection.
//!
//! Materializes a query's partitioned/ordered rows, computes each row's frame
//! bounds (ROWS/RANGE/GROUPS, with EXCLUDE), evaluates the window aggregates
//! and value functions over that frame, and projects the final rows. The entry
//! points `rewrite_grouped_windows`, `project_window_rows`, `window_select`,
//! `dedup_window_rows`, and `cmp_key_rows` are called from the query pipeline.

use crate::mem::arena::Arena;
use crate::pg::respond::Responder;
use crate::sql::ast::{
    BinaryOp, Collation, Expr, FrameBound, FrameUnits, FromClause, OrderBy, Select, SelectItem,
    TableRef, WindowFrame,
};
use crate::sql::eval::{
    ColumnLookup, EvalHooks, SqlError, SubqueryValues, compare_datums, compare_datums_collated,
    eval_full, sqlstate,
};
use crate::sql::exec::MAX_PROJ;
use crate::sql::types::Datum;
use crate::storage::Storage;
use crate::{sql_err, stack_format};

use super::group::row_passes_correlated_where;
use super::subquery::{correlated_in_expression, correlated_scan_conjuncts};
use super::{
    AggState, GroupedRewrite, MAX_AGGS, MAX_JOIN_TABLES, MAX_SUBQUERIES, MAX_WIN_KEYS, MAX_WINDOWS,
    Outcome, QueryScope, arena_full, collect_grouped_aggs, merge_correlated, pax_column_demand,
    project_row, record_star_width, resolve_order_target, rewrite_grouped_expr,
    scan_source_recycling_with_pax_columns, scan_source_with_pax_columns, sql_fail, sql_ok,
    window_row,
};

fn window_scan_where<'a>(
    statement: &'a Select<'a>,
    correlated: &[&'a Expr<'a>],
    arena: &'a Arena,
) -> Result<Option<&'a Expr<'a>>, SqlError> {
    let mut where_correlated = [&Expr::Null; MAX_SUBQUERIES];
    let count =
        correlated_in_expression(statement.where_clause, correlated, &mut where_correlated)?;
    correlated_scan_conjuncts(statement.where_clause, &where_correlated[..count], arena)
}

fn window_pax_columns<'a>(
    statement: &'a Select<'a>,
    scope: &QueryScope<'a>,
    from: &'a FromClause<'a>,
    win_nodes: &[&'a Expr<'a>],
) -> super::PaxReadDemand {
    let mut expressions = [&Expr::Null; MAX_PROJ * 3 + MAX_WINDOWS + 3];
    let mut count = 0usize;
    for item in statement.items {
        match item {
            SelectItem::Expr { expression, .. } => {
                expressions[count] = expression;
                count += 1;
            }
            SelectItem::Wildcard | SelectItem::TableWildcard(_) | SelectItem::RecordStar(_) => {
                return super::PaxReadDemand::full_row(
                    super::scan::PaxFullRowReason::WildcardProjection,
                );
            }
        }
    }
    for expression in win_nodes {
        expressions[count] = expression;
        count += 1;
    }
    if let Some(expression) = statement.where_clause {
        expressions[count] = expression;
        count += 1;
    }
    for order in statement.order_by {
        expressions[count] = order.expression;
        count += 1;
    }
    for expression in statement.distinct_on {
        expressions[count] = expression;
        count += 1;
    }
    if let Some(expression) = statement.limit {
        expressions[count] = expression;
        count += 1;
    }
    if let Some(expression) = statement.offset {
        expressions[count] = expression;
        count += 1;
    }
    pax_column_demand(scope, from, &expressions[..count])
}

/// Windows over a grouped query: PostgreSQL evaluates window functions after
/// GROUP BY / HAVING, over the grouped rows. Rewrites the statement into the
/// equivalent two-level form — an inner grouped select exposing every
/// grouping key and aggregate as a named column, and an outer select (window
/// calls, DISTINCT, ORDER BY, LIMIT) over it as a derived table — so the
/// existing grouped and window executors compose.
pub(crate) fn rewrite_grouped_windows<'a>(
    statement: &'a Select<'a>,
    storage: &'a Storage,
    txid: u32,
    arena: &'a Arena,
) -> Result<&'a Select<'a>, SqlError> {
    let mut agg_nodes: [(*const Expr, &Expr); MAX_AGGS] =
        [(core::ptr::null(), &Expr::Null); MAX_AGGS];
    let mut n_aggs = 0;
    for item in statement.items {
        if let SelectItem::Expr { expression, .. } = item {
            collect_grouped_aggs(expression, &mut agg_nodes, &mut n_aggs, storage, txid)?;
        }
    }
    for ob in statement.order_by {
        collect_grouped_aggs(ob.expression, &mut agg_nodes, &mut n_aggs, storage, txid)?;
    }

    // The inner select: one named column per grouping key and per aggregate.
    let n_keys = statement.group_by.len();
    let mut inner_items = [SelectItem::Wildcard; MAX_PROJ];
    let mut group_names: [&str; MAX_PROJ] = [""; MAX_PROJ];
    let mut agg_names: [&str; MAX_AGGS] = [""; MAX_AGGS];
    for (i, g) in statement.group_by.iter().enumerate() {
        let name = arena
            .alloc_str(stack_format!(16, "?g{}", i).as_str())
            .map_err(|_| arena_full())?;
        group_names[i] = name;
        inner_items[i] = SelectItem::Expr {
            expression: g,
            alias: Some(name),
        };
    }
    for i in 0..n_aggs {
        let name = arena
            .alloc_str(stack_format!(16, "?a{}", i).as_str())
            .map_err(|_| arena_full())?;
        agg_names[i] = name;
        inner_items[n_keys + i] = SelectItem::Expr {
            expression: agg_nodes[i].1,
            alias: Some(name),
        };
    }
    let inner = Select {
        items: arena
            .alloc_slice_copy(&inner_items[..n_keys + n_aggs])
            .map_err(|_| arena_full())?,
        distinct: false,
        distinct_on: &[],
        from: statement.from,
        where_clause: statement.where_clause,
        group_by: statement.group_by,
        grouping_set_quantifier: statement.grouping_set_quantifier,
        grouping_sets: statement.grouping_sets,
        having: statement.having,
        order_by: &[],
        limit: None,
        offset: None,
        with_ties: false,
        with: &[],
        set_body: None,
        locking: &[],
    };
    let inner = arena.alloc(inner).map_err(|_| arena_full())?;

    let group_names: &[&str] = arena
        .alloc_slice_copy(&group_names[..n_keys])
        .map_err(|_| arena_full())?;
    let agg_names: &[&str] = arena
        .alloc_slice_copy(&agg_names[..n_aggs])
        .map_err(|_| arena_full())?;
    let agg_nodes: &[(*const Expr, &Expr)] = arena
        .alloc_slice_copy(&agg_nodes[..n_aggs])
        .map_err(|_| arena_full())?;
    let scope = statement
        .from
        .as_ref()
        .and_then(|f| QueryScope::resolve_schema(storage, f, txid, arena).ok());
    let context = GroupedRewrite {
        group_by: statement.group_by,
        group_names,
        aggs: agg_nodes,
        agg_names,
        scope: scope.as_ref(),
    };
    let mut outer_items = [SelectItem::Wildcard; MAX_PROJ];
    for (i, item) in statement.items.iter().enumerate() {
        outer_items[i] = match item {
            SelectItem::Expr { expression, alias } => SelectItem::Expr {
                expression: rewrite_grouped_expr(expression, &context, arena)?,
                // The rewritten expression would otherwise rename the output
                // column (`?g0`); pin the original name.
                alias: Some(alias.unwrap_or(crate::sql::exec::derived_name(expression))),
            },
            other => *other,
        };
    }
    let mut outer_order = [OrderBy {
        expression: &Expr::Null,
        descending: false,
        nulls_first: false,
    }; MAX_PROJ];
    for (i, ob) in statement.order_by.iter().enumerate() {
        // Ordinals resolve against the (unchanged) select list; expressions
        // rewrite like the items.
        let expression = if matches!(ob.expression, Expr::Int(_)) {
            ob.expression
        } else {
            rewrite_grouped_expr(ob.expression, &context, arena)?
        };
        outer_order[i] = OrderBy { expression, ..*ob };
    }
    let from = FromClause {
        base: TableRef {
            schema: None,
            table: "",
            alias: Some("?grouped"),
            subquery: Some(inner),
            func_args: None,
            func_argument_names: &[],
            func_variadic: false,
            rows_from: None,
            col_alias: None,
            inheritance: crate::sql::ast::RelationInheritance::Descendants,
            sample: None,
            cte: None,
            with_ordinality: false,
            lateral: false,
            authorization_role: None,
        },
        joins: &[],
    };
    let outer = Select {
        items: arena
            .alloc_slice_copy(&outer_items[..statement.items.len()])
            .map_err(|_| arena_full())?,
        distinct: statement.distinct,
        distinct_on: &[],
        from: Some(from),
        where_clause: None,
        group_by: &[],
        grouping_set_quantifier: crate::sql::ast::GroupingSetQuantifier::All,
        grouping_sets: &[],
        having: None,
        order_by: arena
            .alloc_slice_copy(&outer_order[..statement.order_by.len()])
            .map_err(|_| arena_full())?,
        limit: statement.limit,
        offset: statement.offset,
        with_ties: statement.with_ties,
        with: &[],
        set_body: None,
        locking: &[],
    };
    Ok(&*arena.alloc(outer).map_err(|_| arena_full())?)
}

/// Evaluates a ROWS/GROUPS frame offset to a non-negative count.
#[allow(clippy::too_many_arguments)]
fn frame_offset_count<'a>(
    e: &'a Expr<'a>,
    scope: &QueryScope<'a>,
    rows: &[&'a [Datum<'a>]],
    offs: &[usize],
    row_index: usize,
    starting: bool,
    arena: &'a Arena,
    params: &[Datum<'a>],
    hooks: &EvalHooks<'_, 'a>,
) -> Result<usize, SqlError> {
    let r = window_row(scope, rows[row_index], offs);
    let v = eval_full(e, arena, params, &r, hooks)?;
    let n = match v {
        Datum::Int4(x) => x as i64,
        Datum::Int8(x) => x,
        _ => {
            return Err(sql_err!(
                sqlstate::INVALID_PARAMETER_VALUE,
                "frame offset must be an integer"
            ));
        }
    };
    if n < 0 {
        return Err(sql_err!(
            crate::sql::eval::sqlstate::INVALID_PRECEDING_OR_FOLLOWING_SIZE,
            "frame {} offset must not be negative",
            if starting { "starting" } else { "ending" }
        ));
    }
    Ok(n as usize)
}

/// Whether a RANGE offset value is negative (PostgreSQL rejects it with
/// 22013, "invalid preceding or following size in window function").
fn range_offset_negative(v: &Datum) -> bool {
    match v {
        Datum::Int4(x) => *x < 0,
        Datum::Int8(x) => *x < 0,
        Datum::Float4(x) => *x < 0.0,
        Datum::Float8(x) => *x < 0.0,
        Datum::Numeric(n) => n.sign == crate::sql::numeric::Sign::Neg,
        Datum::Interval(iv) => iv.months < 0 || iv.days < 0 || iv.micros < 0,
        _ => false,
    }
}

/// The inclusive row-index range (into the sorted partition `p[..m]`) an
/// explicit frame selects for the row at sorted position `j`; None when the
/// frame is empty. Bound semantics verified against PostgreSQL 18.4.
#[allow(clippy::too_many_arguments)]
fn frame_range<'a>(
    storage: &Storage,
    frame: &WindowFrame<'a>,
    ord: &[OrderBy<'a>],
    scope: &QueryScope<'a>,
    rows: &[&'a [Datum<'a>]],
    offs: &[usize],
    p: &[usize],
    j: usize,
    arena: &'a Arena,
    params: &[Datum<'a>],
    hooks: &EvalHooks<'_, 'a>,
) -> Result<Option<(usize, usize)>, SqlError> {
    let m = p.len();
    // Peers under the window ORDER BY (every row is a peer with no ORDER BY).
    let is_peer = |a: usize, b: usize| -> Result<bool, SqlError> {
        ord.iter().try_fold(true, |acc, o| {
            Ok::<bool, SqlError>(
                acc && {
                    let ra = window_row(scope, rows[p[a]], offs);
                    let va = eval_full(o.expression, arena, params, &ra, hooks)?;
                    let rb = window_row(scope, rows[p[b]], offs);
                    let vb = eval_full(o.expression, arena, params, &rb, hooks)?;
                    match (va.is_null(), vb.is_null()) {
                        (true, true) => true,
                        (true, false) | (false, true) => false,
                        (false, false) => compare_datums_collated(
                            storage,
                            scope.expression_collation(o.expression)?,
                            &va,
                            &vb,
                        )?
                        .is_eq(),
                    }
                },
            )
        })
    };
    let peer_start = |from: usize| -> Result<usize, SqlError> {
        let mut s = from;
        while s > 0 && is_peer(s - 1, from)? {
            s -= 1;
        }
        Ok(s)
    };
    let peer_end = |from: usize| -> Result<usize, SqlError> {
        let mut e = from;
        while e + 1 < m && is_peer(e + 1, from)? {
            e += 1;
        }
        Ok(e)
    };

    // RANGE with a value offset compares the single ORDER BY key.
    let range_edge = |bound: &FrameBound<'a>, starting: bool| -> Result<isize, SqlError> {
        let (offset_expr, preceding) = match bound {
            FrameBound::UnboundedPreceding => return Ok(0),
            FrameBound::UnboundedFollowing => return Ok(m as isize - 1),
            FrameBound::CurrentRow => {
                return Ok(if starting {
                    peer_start(j)? as isize
                } else {
                    peer_end(j)? as isize
                });
            }
            FrameBound::Preceding(e) => (*e, true),
            FrameBound::Following(e) => (*e, false),
        };
        if ord.len() != 1 {
            return Err(sql_err!(
                sqlstate::WINDOWING_ERROR,
                "RANGE with offset PRECEDING/FOLLOWING requires exactly one ORDER BY column"
            ));
        }
        let o = &ord[0];
        let key_of = |i: usize| -> Result<Datum<'a>, SqlError> {
            let r = window_row(scope, rows[p[i]], offs);
            eval_full(o.expression, arena, params, &r, hooks)
        };
        let r = window_row(scope, rows[p[j]], offs);
        let off = eval_full(offset_expr, arena, params, &r, hooks)?;
        if range_offset_negative(&off) {
            return Err(sql_err!(
                crate::sql::eval::sqlstate::INVALID_PRECEDING_OR_FOLLOWING_SIZE,
                "invalid preceding or following size in window function"
            ));
        }
        let key_j = key_of(j)?;
        // A NULL current key frames its peer group (nulls are peers).
        if key_j.is_null() {
            return Ok(if starting {
                peer_start(j)? as isize
            } else {
                peer_end(j)? as isize
            });
        }
        // The frame edge value: preceding moves against the sort direction.
        let towards_smaller = preceding != o.descending;
        let op = if towards_smaller {
            BinaryOp::Sub
        } else {
            BinaryOp::Add
        };
        let edge = crate::sql::eval::arithmetic(op, key_j, off, false, false, arena)?;
        // In-frame: key between edge and key_j (inclusive), in sort order.
        let in_frame = |i: usize| -> Result<bool, SqlError> {
            let k = key_of(i)?;
            if k.is_null() {
                return Ok(false);
            }
            let c = compare_datums_collated(
                storage,
                scope.expression_collation(o.expression)?,
                &k,
                &edge,
            )?;
            Ok(if towards_smaller {
                c.is_ge()
            } else {
                c.is_le()
            })
        };
        if starting {
            // First row (scanning forward) inside the frame edge.
            for i in 0..m {
                let k = key_of(i)?;
                if k.is_null() {
                    continue;
                }
                let c = compare_datums_collated(
                    storage,
                    scope.expression_collation(o.expression)?,
                    &k,
                    &edge,
                )?;
                let inside = if preceding {
                    in_frame(i)?
                } else {
                    // Starting FOLLOWING: first row at/after the edge in sort
                    // direction.
                    if o.descending { c.is_le() } else { c.is_ge() }
                };
                let _ = c;
                if inside {
                    return Ok(i as isize);
                }
            }
            Ok(m as isize)
        } else {
            // Last row (scanning backward) inside the frame edge.
            for i in (0..m).rev() {
                let k = key_of(i)?;
                if k.is_null() {
                    continue;
                }
                let c = compare_datums_collated(
                    storage,
                    scope.expression_collation(o.expression)?,
                    &k,
                    &edge,
                )?;
                let inside = if preceding {
                    // Ending PRECEDING: last row at/before the edge.
                    if o.descending { c.is_ge() } else { c.is_le() }
                } else {
                    in_frame(i)?
                };
                let _ = c;
                if inside {
                    return Ok(i as isize);
                }
            }
            Ok(-1)
        }
    };

    let (start, end): (isize, isize) = match frame.units {
        FrameUnits::Rows => {
            let s: isize = match &frame.start {
                FrameBound::UnboundedPreceding => 0,
                FrameBound::Preceding(e) => {
                    j as isize
                        - frame_offset_count(
                            e, scope, rows, offs, p[j], true, arena, params, hooks,
                        )? as isize
                }
                FrameBound::CurrentRow => j as isize,
                FrameBound::Following(e) => {
                    j as isize
                        + frame_offset_count(
                            e, scope, rows, offs, p[j], true, arena, params, hooks,
                        )? as isize
                }
                FrameBound::UnboundedFollowing => unreachable!("rejected at parse"),
            };
            let e: isize = match &frame.end {
                FrameBound::UnboundedPreceding => unreachable!("rejected at parse"),
                FrameBound::Preceding(e) => {
                    j as isize
                        - frame_offset_count(
                            e, scope, rows, offs, p[j], false, arena, params, hooks,
                        )? as isize
                }
                FrameBound::CurrentRow => j as isize,
                FrameBound::Following(e) => {
                    j as isize
                        + frame_offset_count(
                            e, scope, rows, offs, p[j], false, arena, params, hooks,
                        )? as isize
                }
                FrameBound::UnboundedFollowing => m as isize - 1,
            };
            (s, e)
        }
        FrameUnits::Groups => {
            if ord.is_empty() {
                return Err(sql_err!(
                    sqlstate::WINDOWING_ERROR,
                    "GROUPS mode requires an ORDER BY clause"
                ));
            }
            // This row's peer-group index (groups counted from the front).
            let gj = {
                let mut g = 0usize;
                let mut i = 0usize;
                while i < j {
                    if !is_peer(i, i + 1)? {
                        g += 1;
                    }
                    i += 1;
                }
                g
            };
            let group_start = |target: isize| -> Result<Option<usize>, SqlError> {
                if target < 0 {
                    return Ok(Some(0));
                }
                let mut g = 0usize;
                let mut i = 0usize;
                loop {
                    if g == target as usize {
                        return Ok(Some(i));
                    }
                    // advance to next group
                    let e = peer_end(i)?;
                    if e + 1 >= m {
                        return Ok(None);
                    }
                    i = e + 1;
                    g += 1;
                }
            };
            let group_end = |target: isize| -> Result<Option<usize>, SqlError> {
                if target < 0 {
                    return Ok(None);
                }
                match group_start(target)? {
                    Some(i) => Ok(Some(peer_end(i)?)),
                    None => Ok(Some(m - 1)),
                }
            };
            let s: isize = match &frame.start {
                FrameBound::UnboundedPreceding => 0,
                FrameBound::Preceding(e) => {
                    let k =
                        frame_offset_count(e, scope, rows, offs, p[j], true, arena, params, hooks)?;
                    group_start(gj as isize - k as isize)?.unwrap_or(0) as isize
                }
                FrameBound::CurrentRow => peer_start(j)? as isize,
                FrameBound::Following(e) => {
                    let k =
                        frame_offset_count(e, scope, rows, offs, p[j], true, arena, params, hooks)?;
                    match group_start(gj as isize + k as isize)? {
                        Some(x) => x as isize,
                        None => m as isize, // past the last group: empty
                    }
                }
                FrameBound::UnboundedFollowing => unreachable!("rejected at parse"),
            };
            let e: isize = match &frame.end {
                FrameBound::UnboundedPreceding => unreachable!("rejected at parse"),
                FrameBound::Preceding(e) => {
                    let k = frame_offset_count(
                        e, scope, rows, offs, p[j], false, arena, params, hooks,
                    )?;
                    match group_end(gj as isize - k as isize)? {
                        Some(x) => x as isize,
                        None => -1, // before the first group: empty
                    }
                }
                FrameBound::CurrentRow => peer_end(j)? as isize,
                FrameBound::Following(e) => {
                    let k = frame_offset_count(
                        e, scope, rows, offs, p[j], false, arena, params, hooks,
                    )?;
                    group_end(gj as isize + k as isize)?.map_or(m as isize - 1, |x| x as isize)
                }
                FrameBound::UnboundedFollowing => m as isize - 1,
            };
            (s, e)
        }
        FrameUnits::Range => {
            let uses_offset = matches!(
                (&frame.start, &frame.end),
                (FrameBound::Preceding(_) | FrameBound::Following(_), _)
                    | (_, FrameBound::Preceding(_) | FrameBound::Following(_))
            );
            if uses_offset && ord.is_empty() {
                return Err(sql_err!(
                    sqlstate::WINDOWING_ERROR,
                    "RANGE with offset PRECEDING/FOLLOWING requires exactly one ORDER BY column"
                ));
            }
            (
                range_edge(&frame.start, true)?,
                range_edge(&frame.end, false)?,
            )
        }
    };
    let start = start.max(0);
    let end = end.min(m as isize - 1);
    if start > end || start >= m as isize || end < 0 {
        return Ok(None);
    }
    Ok(Some((start as usize, end as usize)))
}

#[allow(clippy::too_many_arguments)]
fn window_keys_equal<'a>(
    storage: &Storage,
    expressions: &[&Expr<'a>],
    scope: &QueryScope<'a>,
    rows: &[&'a [Datum<'a>]],
    offsets: &[usize],
    left: usize,
    right: usize,
    arena: &'a Arena,
    params: &[Datum<'a>],
    hooks: &EvalHooks<'_, 'a>,
) -> Result<bool, SqlError> {
    for expression in expressions {
        let left_row = window_row(scope, rows[left], offsets);
        let left_value = eval_full(expression, arena, params, &left_row, hooks)?;
        let right_row = window_row(scope, rows[right], offsets);
        let right_value = eval_full(expression, arena, params, &right_row, hooks)?;
        if !matches!((left_value.is_null(), right_value.is_null()), (true, true))
            && (left_value.is_null()
                || right_value.is_null()
                || !compare_datums_collated(
                    storage,
                    scope.expression_collation(expression)?,
                    &left_value,
                    &right_value,
                )?
                .is_eq())
        {
            return Ok(false);
        }
    }
    Ok(true)
}

/// The current row's peer-group bounds (sorted-partition indices) under the
/// window ORDER BY; the row alone when there is no ORDER BY.
#[allow(clippy::too_many_arguments)]
fn peer_bounds<'a>(
    storage: &Storage,
    ord: &[OrderBy<'a>],
    scope: &QueryScope<'a>,
    rows: &[&'a [Datum<'a>]],
    offs: &[usize],
    p: &[usize],
    j: usize,
    arena: &'a Arena,
    params: &[Datum<'a>],
    hooks: &EvalHooks<'_, 'a>,
) -> Result<(usize, usize), SqlError> {
    if ord.is_empty() {
        // No ORDER BY: every partition row is a peer.
        return Ok((0, p.len() - 1));
    }
    let is_peer = |a: usize, b: usize| -> Result<bool, SqlError> {
        ord.iter().try_fold(true, |acc, o| {
            Ok::<bool, SqlError>(
                acc && {
                    let ra = window_row(scope, rows[p[a]], offs);
                    let va = eval_full(o.expression, arena, params, &ra, hooks)?;
                    let rb = window_row(scope, rows[p[b]], offs);
                    let vb = eval_full(o.expression, arena, params, &rb, hooks)?;
                    match (va.is_null(), vb.is_null()) {
                        (true, true) => true,
                        (true, false) | (false, true) => false,
                        (false, false) => compare_datums_collated(
                            storage,
                            scope.expression_collation(o.expression)?,
                            &va,
                            &vb,
                        )?
                        .is_eq(),
                    }
                },
            )
        })
    };
    let mut s = j;
    while s > 0 && is_peer(s - 1, j)? {
        s -= 1;
    }
    let mut e = j;
    while e + 1 < p.len() && is_peer(e + 1, j)? {
        e += 1;
    }
    Ok((s, e))
}

/// Whether sorted-partition index `i` is removed from row `j`'s frame by the
/// frame's EXCLUDE clause (`peers` = row `j`'s peer-group bounds).
fn frame_excludes(
    exclusion: crate::sql::ast::FrameExclusion,
    j: usize,
    peers: (usize, usize),
    i: usize,
) -> bool {
    use crate::sql::ast::FrameExclusion::*;
    match exclusion {
        NoOthers => false,
        CurrentRow => i == j,
        Group => i >= peers.0 && i <= peers.1,
        Ties => i != j && i >= peers.0 && i <= peers.1,
    }
}

/// Computes one window function's value for every materialized row, returned as
/// a slice indexed by materialized-row order.
#[allow(clippy::too_many_arguments, clippy::needless_range_loop)]
fn compute_window<'a>(
    storage: &Storage,
    txid: u32,
    node: &'a Expr<'a>,
    rows: &[&'a [Datum<'a>]],
    scope: &QueryScope<'a>,
    offs: &[usize],
    arena: &'a Arena,
    params: &[Datum<'a>],
    hooks: &EvalHooks<'_, 'a>,
) -> Result<&'a [Datum<'a>], SqlError> {
    let Expr::Call {
        name,
        args,
        over: Some(spec),
        ..
    } = node
    else {
        return Err(sql_err!(sqlstate::INTERNAL_ERROR, "not a window function"));
    };
    let n = rows.len();
    let out = arena
        .alloc_slice_with(n, |_| Datum::Null)
        .map_err(|_| arena_full())?;

    // Assign each row a partition id by comparing PARTITION BY keys.
    let group_of = arena
        .alloc_slice_with(n, |_| 0usize)
        .map_err(|_| arena_full())?;
    let reps = arena
        .alloc_slice_with(n, |_| 0usize)
        .map_err(|_| arena_full())?;
    let mut n_groups = 0usize;
    for i in 0..n {
        let mut gid = None;
        for g in 0..n_groups {
            if window_keys_equal(
                storage,
                spec.partition_by,
                scope,
                rows,
                offs,
                i,
                reps[g],
                arena,
                params,
                hooks,
            )? {
                gid = Some(g);
                break;
            }
        }
        match gid {
            Some(g) => group_of[i] = g,
            None => {
                reps[n_groups] = i;
                group_of[i] = n_groups;
                n_groups += 1;
            }
        }
    }

    let is_ranking = matches!(
        *name,
        "row_number" | "rank" | "dense_rank" | "percent_rank" | "cume_dist"
    );
    let is_offset = matches!(*name, "lag" | "lead");

    let part = arena
        .alloc_slice_with(n, |_| 0usize)
        .map_err(|_| arena_full())?;
    for g in 0..n_groups {
        // Collect this partition's row indices, then sort by ORDER BY.
        let mut m = 0usize;
        for i in 0..n {
            if group_of[i] == g {
                part[m] = i;
                m += 1;
            }
        }
        let ord = &spec.order_by;
        if !ord.is_empty() {
            // Insertion sort (stable) by the ORDER BY keys — partitions are
            // small and this avoids a fallible comparator.
            for x in 1..m {
                let mut y = x;
                while y > 0 {
                    let c = cmp_order(
                        ord,
                        scope,
                        rows,
                        offs,
                        part[y - 1],
                        part[y],
                        arena,
                        params,
                        hooks,
                        storage,
                    )?;
                    if c == core::cmp::Ordering::Greater {
                        part.swap(y - 1, y);
                        y -= 1;
                    } else {
                        break;
                    }
                }
            }
        }
        let p = &part[..m];

        if is_ranking {
            // Peer flags on the sorted partition: `same[j]` means row `p[j]`
            // ties `p[j-1]` on the ORDER BY keys (with no ORDER BY the whole
            // partition is one peer group). `rank`/`percent_rank`/`cume_dist`
            // all read from these boundaries.
            let same = arena
                .alloc_slice_with(m, |_| false)
                .map_err(|_| arena_full())?;
            for j in 1..m {
                same[j] = spec.order_by.iter().try_fold(true, |acc, o| {
                    Ok::<bool, SqlError>(
                        acc && {
                            let ra = window_row(scope, rows[p[j - 1]], offs);
                            let va = eval_full(o.expression, arena, params, &ra, hooks)?;
                            let rb = window_row(scope, rows[p[j]], offs);
                            let vb = eval_full(o.expression, arena, params, &rb, hooks)?;
                            match (va.is_null(), vb.is_null()) {
                                (true, true) => true,
                                (true, false) | (false, true) => false,
                                (false, false) => compare_datums_collated(
                                    storage,
                                    scope.expression_collation(o.expression)?,
                                    &va,
                                    &vb,
                                )?
                                .is_eq(),
                            }
                        },
                    )
                })?;
            }
            let mut rank = 1i64;
            let mut dense = 1i64;
            for j in 0..m {
                if j > 0 && !same[j] {
                    rank = j as i64 + 1;
                    dense += 1;
                }
                out[p[j]] = match *name {
                    "row_number" => Datum::Int8(j as i64 + 1),
                    "rank" => Datum::Int8(rank),
                    "dense_rank" => Datum::Int8(dense),
                    // percent_rank: (rank - 1) / (rows - 1); a lone row is 0.
                    "percent_rank" => Datum::Float8(if m <= 1 {
                        0.0
                    } else {
                        (rank - 1) as f64 / (m as f64 - 1.0)
                    }),
                    // cume_dist: fraction of rows at or before this peer group
                    // (the peer group's last index, one-based, over the count).
                    _ => {
                        let mut end = j;
                        while end + 1 < m && same[end + 1] {
                            end += 1;
                        }
                        Datum::Float8((end + 1) as f64 / m as f64)
                    }
                };
            }
        } else if is_offset {
            let sign: isize = if *name == "lag" { -1 } else { 1 };
            let offset: isize = if args.len() >= 2 {
                let r = window_row(scope, rows[p[0]], offs);
                match eval_full(args[1], arena, params, &r, hooks)? {
                    Datum::Int4(v) => v as isize,
                    Datum::Int8(v) => v as isize,
                    _ => 1,
                }
            } else {
                1
            };
            for j in 0..m {
                let src = j as isize + sign * offset;
                out[p[j]] = if src >= 0 && (src as usize) < m {
                    let r = window_row(scope, rows[p[src as usize]], offs);
                    eval_full(args[0], arena, params, &r, hooks)?
                } else if args.len() >= 3 {
                    let r = window_row(scope, rows[p[j]], offs);
                    eval_full(args[2], arena, params, &r, hooks)?
                } else {
                    Datum::Null
                };
            }
        } else if let Some(frame) = &spec.frame
            && matches!(*name, "first_value" | "last_value" | "nth_value")
        {
            // Value functions over an explicit frame: per row, the value at
            // the frame's start / end / nth position (NULL on an empty or
            // too-short frame).
            for j in 0..m {
                let range = frame_range(
                    storage,
                    frame,
                    spec.order_by,
                    scope,
                    rows,
                    offs,
                    p,
                    j,
                    arena,
                    params,
                    hooks,
                )?;
                let peers = if frame.exclusion == crate::sql::ast::FrameExclusion::NoOthers {
                    (j, j)
                } else {
                    peer_bounds(
                        storage,
                        spec.order_by,
                        scope,
                        rows,
                        offs,
                        p,
                        j,
                        arena,
                        params,
                        hooks,
                    )?
                };
                let excluded = |i: usize| frame_excludes(frame.exclusion, j, peers, i);
                out[p[j]] = match (range, *name) {
                    (None, _) => Datum::Null,
                    (Some((fs, fe)), "first_value") => match (fs..=fe).find(|&i| !excluded(i)) {
                        Some(i) => {
                            let r = window_row(scope, rows[p[i]], offs);
                            eval_full(args[0], arena, params, &r, hooks)?
                        }
                        None => Datum::Null,
                    },
                    (Some((fs, fe)), "last_value") => {
                        match (fs..=fe).rev().find(|&i| !excluded(i)) {
                            Some(i) => {
                                let r = window_row(scope, rows[p[i]], offs);
                                eval_full(args[0], arena, params, &r, hooks)?
                            }
                            None => Datum::Null,
                        }
                    }
                    (Some((fs, fe)), _) => {
                        let r = window_row(scope, rows[p[j]], offs);
                        let nth = match eval_full(args[1], arena, params, &r, hooks)? {
                            Datum::Int4(v) => v as i64,
                            Datum::Int8(v) => v,
                            _ => 1,
                        };
                        let target = if nth >= 1 {
                            (fs..=fe).filter(|&i| !excluded(i)).nth(nth as usize - 1)
                        } else {
                            None
                        };
                        match target {
                            Some(i) => {
                                let r = window_row(scope, rows[p[i]], offs);
                                eval_full(args[0], arena, params, &r, hooks)?
                            }
                            None => Datum::Null,
                        }
                    }
                };
            }
        } else if matches!(*name, "first_value" | "last_value" | "nth_value" | "ntile") {
            // Value/positional window functions over the default frame
            // (UNBOUNDED PRECEDING TO CURRENT ROW when there is an ORDER BY,
            // else the whole partition).
            let peer_end = |from: usize| -> Result<usize, SqlError> {
                // Index of the last row peered with `from` under the ORDER BY
                // (itself when there is no ORDER BY).
                if spec.order_by.is_empty() {
                    return Ok(m - 1);
                }
                let mut e = from;
                while e + 1 < m {
                    let same = spec.order_by.iter().try_fold(true, |acc, o| {
                        Ok::<bool, SqlError>(
                            acc && {
                                let ra = window_row(scope, rows[p[e]], offs);
                                let va = eval_full(o.expression, arena, params, &ra, hooks)?;
                                let rb = window_row(scope, rows[p[e + 1]], offs);
                                let vb = eval_full(o.expression, arena, params, &rb, hooks)?;
                                match (va.is_null(), vb.is_null()) {
                                    (true, true) => true,
                                    (true, false) | (false, true) => false,
                                    (false, false) => compare_datums_collated(
                                        storage,
                                        scope.expression_collation(o.expression)?,
                                        &va,
                                        &vb,
                                    )?
                                    .is_eq(),
                                }
                            },
                        )
                    })?;
                    if same {
                        e += 1;
                    } else {
                        break;
                    }
                }
                Ok(e)
            };
            match *name {
                "ntile" => {
                    let buckets = {
                        let r = window_row(scope, rows[p[0]], offs);
                        match eval_full(args[0], arena, params, &r, hooks)? {
                            Datum::Int4(v) => v as i64,
                            Datum::Int8(v) => v,
                            _ => 1,
                        }
                    }
                    .max(1);
                    let base = m as i64 / buckets;
                    let larger = m as i64 % buckets; // first `larger` buckets get one extra row
                    let mut index = 0usize;
                    for bucket in 1..=buckets {
                        let size = base + if bucket <= larger { 1 } else { 0 };
                        for _ in 0..size {
                            out[p[index]] = Datum::Int8(bucket);
                            index += 1;
                        }
                    }
                }
                "first_value" => {
                    // Frame start is always the partition start.
                    let r = window_row(scope, rows[p[0]], offs);
                    let value = eval_full(args[0], arena, params, &r, hooks)?;
                    for &row_index in p {
                        out[row_index] = value;
                    }
                }
                "last_value" => {
                    // Frame end is the current row's peer-group end.
                    let mut j = 0usize;
                    while j < m {
                        let end = peer_end(j)?;
                        let r = window_row(scope, rows[p[end]], offs);
                        let value = eval_full(args[0], arena, params, &r, hooks)?;
                        for &row_index in &p[j..=end] {
                            out[row_index] = value;
                        }
                        j = end + 1;
                    }
                }
                _ => {
                    // nth_value(expr, n): the nth row of the frame (1-based from
                    // the frame start); NULL until the frame has reached it.
                    let nth = {
                        let r = window_row(scope, rows[p[0]], offs);
                        match eval_full(args[1], arena, params, &r, hooks)? {
                            Datum::Int4(v) => v as usize,
                            Datum::Int8(v) => v as usize,
                            _ => 1,
                        }
                    };
                    let nth_value = if nth >= 1 && nth <= m {
                        let r = window_row(scope, rows[p[nth - 1]], offs);
                        Some(eval_full(args[0], arena, params, &r, hooks)?)
                    } else {
                        None
                    };
                    let mut j = 0usize;
                    while j < m {
                        let end = peer_end(j)?;
                        // The frame includes rows p[0..=end]; nth is present iff
                        // nth-1 <= end.
                        let value = match nth_value {
                            Some(v) if nth >= 1 && nth - 1 <= end => v,
                            _ => Datum::Null,
                        };
                        for &row_index in &p[j..=end] {
                            out[row_index] = value;
                        }
                        j = end + 1;
                    }
                }
            }
        } else if let Some(frame) = &spec.frame {
            // Aggregate over an explicit frame: computed per row (an empty
            // frame aggregates zero rows — count 0, sum NULL).
            if frame.exclusion == crate::sql::ast::FrameExclusion::NoOthers {
                let mut moving_state = AggState::default();
                if moving_state.init_moving(node, storage, txid, &super::ScopeCols(scope), arena)? {
                    let mut active: Option<(usize, usize)> = None;
                    for j in 0..m {
                        let range = frame_range(
                            storage,
                            frame,
                            spec.order_by,
                            scope,
                            rows,
                            offs,
                            p,
                            j,
                            arena,
                            params,
                            hooks,
                        )?;
                        let Some((start, end)) = range else {
                            moving_state = AggState::default();
                            let selected = moving_state.init_moving(
                                node,
                                storage,
                                txid,
                                &super::ScopeCols(scope),
                                arena,
                            )?;
                            debug_assert!(selected);
                            active = None;
                            out[p[j]] = moving_state.finish(arena, hooks.catalog)?;
                            continue;
                        };
                        let mut rebuild = false;
                        if let Some((old_start, old_end)) = active {
                            if start < old_start || end < old_end {
                                rebuild = true;
                            } else {
                                let remove_end = start.min(old_end.saturating_add(1));
                                for index in old_start..remove_end {
                                    let row = window_row(scope, rows[p[index]], offs);
                                    if !moving_state
                                        .remove_moving(node, arena, params, &row, hooks)?
                                    {
                                        rebuild = true;
                                        break;
                                    }
                                }
                                if !rebuild {
                                    let add_start = if start > old_end {
                                        start
                                    } else {
                                        old_end.saturating_add(1)
                                    };
                                    for index in add_start..=end {
                                        let row = window_row(scope, rows[p[index]], offs);
                                        moving_state.update(node, arena, params, &row, hooks)?;
                                    }
                                }
                            }
                        } else {
                            for index in start..=end {
                                let row = window_row(scope, rows[p[index]], offs);
                                moving_state.update(node, arena, params, &row, hooks)?;
                            }
                        }
                        if rebuild {
                            moving_state = AggState::default();
                            let selected = moving_state.init_moving(
                                node,
                                storage,
                                txid,
                                &super::ScopeCols(scope),
                                arena,
                            )?;
                            debug_assert!(selected);
                            for index in start..=end {
                                let row = window_row(scope, rows[p[index]], offs);
                                moving_state.update(node, arena, params, &row, hooks)?;
                            }
                        }
                        active = Some((start, end));
                        out[p[j]] = moving_state.finish(arena, hooks.catalog)?;
                    }
                    continue;
                }
            }
            for j in 0..m {
                let range = frame_range(
                    storage,
                    frame,
                    spec.order_by,
                    scope,
                    rows,
                    offs,
                    p,
                    j,
                    arena,
                    params,
                    hooks,
                )?;
                let peers = if frame.exclusion == crate::sql::ast::FrameExclusion::NoOthers {
                    (j, j)
                } else {
                    peer_bounds(
                        storage,
                        spec.order_by,
                        scope,
                        rows,
                        offs,
                        p,
                        j,
                        arena,
                        params,
                        hooks,
                    )?
                };
                let mut st = AggState::default();
                st.init(node, storage, txid, &super::ScopeCols(scope), arena)?;
                if let Some((fs, fe)) = range {
                    for i in fs..=fe {
                        if frame_excludes(frame.exclusion, j, peers, i) {
                            continue;
                        }
                        let r = window_row(scope, rows[p[i]], offs);
                        st.update(node, arena, params, &r, hooks)?;
                    }
                }
                out[p[j]] = st.finish(arena, hooks.catalog)?;
            }
        } else {
            // Aggregate window function. Default frame:
            //  - no ORDER BY: the whole partition (same value for every row);
            //  - with ORDER BY: RANGE UNBOUNDED PRECEDING TO CURRENT ROW, i.e.
            //    a running aggregate where peers (equal ORDER BY keys) share the
            //    value at the end of their peer group.
            if spec.order_by.is_empty() {
                let mut st = AggState::default();
                st.init(node, storage, txid, &super::ScopeCols(scope), arena)?;
                for &ri in p {
                    let r = window_row(scope, rows[ri], offs);
                    st.update(node, arena, params, &r, hooks)?;
                }
                let v = st.finish(arena, hooks.catalog)?;
                for &ri in p {
                    out[ri] = v;
                }
            } else {
                // Peer-group boundaries, then recompute the running aggregate at
                // each boundary and assign it to the whole peer group.
                let mut j = 0usize;
                while j < m {
                    let mut e = j;
                    while e + 1 < m {
                        let same = spec.order_by.iter().try_fold(true, |acc, o| {
                            Ok::<bool, SqlError>(
                                acc && {
                                    let ra = window_row(scope, rows[p[e]], offs);
                                    let va = eval_full(o.expression, arena, params, &ra, hooks)?;
                                    let rb = window_row(scope, rows[p[e + 1]], offs);
                                    let vb = eval_full(o.expression, arena, params, &rb, hooks)?;
                                    match (va.is_null(), vb.is_null()) {
                                        (true, true) => true,
                                        (true, false) | (false, true) => false,
                                        (false, false) => compare_datums_collated(
                                            storage,
                                            scope.expression_collation(o.expression)?,
                                            &va,
                                            &vb,
                                        )?
                                        .is_eq(),
                                    }
                                },
                            )
                        })?;
                        if same {
                            e += 1;
                        } else {
                            break;
                        }
                    }
                    // Frame is p[0..=e]; aggregate and assign to peers p[j..=e].
                    let mut st = AggState::default();
                    st.init(node, storage, txid, &super::ScopeCols(scope), arena)?;
                    for &ri in &p[..=e] {
                        let r = window_row(scope, rows[ri], offs);
                        st.update(node, arena, params, &r, hooks)?;
                    }
                    let v = st.finish(arena, hooks.catalog)?;
                    for &ri in &p[j..=e] {
                        out[ri] = v;
                    }
                    j = e + 1;
                }
            }
        }
    }
    Ok(&*out)
}

/// Resolves one ORDER BY key's direction given both decoded values.
fn order_by_key<'a>(
    storage: &Storage,
    collation: Collation,
    va: &Datum<'a>,
    vb: &Datum<'a>,
    o: &OrderBy<'a>,
) -> Result<core::cmp::Ordering, SqlError> {
    use core::cmp::Ordering;
    let base = match (va.is_null(), vb.is_null()) {
        (true, true) => Ordering::Equal,
        (true, false) => {
            if o.nulls_first {
                Ordering::Less
            } else {
                Ordering::Greater
            }
        }
        (false, true) => {
            if o.nulls_first {
                Ordering::Greater
            } else {
                Ordering::Less
            }
        }
        (false, false) => compare_datums_collated(storage, collation, va, vb)?,
    };
    Ok(if o.descending && !va.is_null() && !vb.is_null() {
        base.reverse()
    } else {
        base
    })
}

/// Compares two rows by a window ORDER BY spec (ASC/DESC, NULLS FIRST/LAST).
#[allow(clippy::too_many_arguments)]
fn cmp_order<'a>(
    ord: &[OrderBy<'a>],
    scope: &QueryScope<'a>,
    rows: &[&'a [Datum<'a>]],
    offs: &[usize],
    a: usize,
    b: usize,
    arena: &'a Arena,
    params: &[Datum<'a>],
    hooks: &EvalHooks<'_, 'a>,
    storage: &Storage,
) -> Result<core::cmp::Ordering, SqlError> {
    use core::cmp::Ordering;
    for o in ord {
        let ra = window_row(scope, rows[a], offs);
        let va = eval_full(o.expression, arena, params, &ra, hooks)?;
        let rb = window_row(scope, rows[b], offs);
        let vb = eval_full(o.expression, arena, params, &rb, hooks)?;
        let c = order_by_key(
            storage,
            scope.expression_collation(o.expression)?,
            &va,
            &vb,
            o,
        )?;
        if c != Ordering::Equal {
            return Ok(c);
        }
    }
    Ok(Ordering::Equal)
}

/// Materializes the post-WHERE source rows, computes each window function, and
/// projects every row with the window values in scope. Returns the (unsorted)
/// projected rows and their ORDER BY sort keys. Shared by the streaming
/// `window_select` and the derived-table / INSERT-source materializer.
#[allow(
    clippy::type_complexity,
    clippy::too_many_arguments,
    clippy::needless_range_loop
)]
pub(crate) fn project_window_rows<'a>(
    storage: &'a Storage,
    txid: u32,
    statement: &'a Select<'a>,
    from: &'a FromClause<'a>,
    scope: &QueryScope<'a>,
    win_nodes: &[&'a Expr<'a>],
    hooks: &EvalHooks<'_, 'a>,
    correlated: &'a [&'a Expr<'a>],
    base: &SubqueryValues<'a, 'a>,
    arena: &'a Arena,
    params: &[Datum<'a>],
    // Enclosing query's row, when this window query is a correlated subquery's
    // body; its columns resolve after this query's own.
    outer: Option<&dyn ColumnLookup<'a>>,
) -> Result<(&'a [&'a [Datum<'a>]], &'a [&'a [Datum<'a>]]), SqlError> {
    let scan_where = window_scan_where(statement, correlated, arena)?;
    let pax_columns = window_pax_columns(statement, scope, from, win_nodes);
    // Flat-row column offsets per table.
    let mut offs = [0usize; MAX_JOIN_TABLES];
    let mut total = 0usize;
    for t in 0..scope.n {
        offs[t] = total;
        total += scope.defs[t].expect("resolved").n_columns;
    }

    // Pass 1: count source rows.
    let mut count = 0usize;
    scan_source_with_pax_columns(
        storage,
        scope,
        from,
        txid,
        scan_where,
        arena,
        params,
        hooks,
        outer,
        pax_columns,
        &mut |row| {
            if !row_passes_correlated_where(
                correlated,
                statement.where_clause,
                storage,
                txid,
                arena,
                params,
                hooks,
                row,
            )? {
                return Ok(true);
            }
            count += 1;
            Ok(true)
        },
    )?;
    // Pass 2: materialize each row's columns flat in the arena.
    let empty: &[Datum] = &[];
    let rows: &mut [&[Datum]] = arena
        .alloc_slice_with(count, |_| empty)
        .map_err(|_| arena_full())?;
    let mut at = 0usize;
    scan_source_with_pax_columns(
        storage,
        scope,
        from,
        txid,
        scan_where,
        arena,
        params,
        hooks,
        outer,
        pax_columns,
        &mut |row| {
            if !row_passes_correlated_where(
                correlated,
                statement.where_clause,
                storage,
                txid,
                arena,
                params,
                hooks,
                row,
            )? {
                return Ok(true);
            }
            let flat = arena
                .alloc_slice_with(total.max(1), |_| Datum::Null)
                .map_err(|_| arena_full())?;
            for (t, offset) in offs.iter().enumerate().take(scope.n) {
                let def = scope.defs[t].expect("resolved");
                let vals = row.values[t].expect("bound");
                for c in 0..def.n_columns {
                    flat[offset + c] = if vals.is_empty() {
                        Datum::Null
                    } else {
                        vals[c]
                    };
                }
            }
            rows[at] = &flat[..total];
            at += 1;
            Ok(true)
        },
    )?;
    let rows: &[&[Datum]] = &rows[..count];

    // Compute each window function's per-row values.
    let mut win_vals: [&[Datum]; MAX_WINDOWS] = [empty; MAX_WINDOWS];
    for (wi, &node) in win_nodes.iter().enumerate() {
        win_vals[wi] = compute_window(
            storage, txid, node, rows, scope, &offs, arena, params, hooks,
        )?;
    }
    let win_ptrs: &[*const Expr] = arena
        .alloc_slice_with(win_nodes.len(), |i| win_nodes[i] as *const Expr)
        .map_err(|_| arena_full())?;

    // Resolve ORDER BY (ordinals → select items).
    let n_order = statement.order_by.len();
    let mut order_exprs: [Option<&Expr>; MAX_WIN_KEYS] = [None; MAX_WIN_KEYS];
    if n_order > MAX_WIN_KEYS {
        return Err(sql_err!(
            sqlstate::TOO_MANY_ARGUMENTS,
            "ORDER BY list too long"
        ));
    }
    for (k, ob) in statement.order_by.iter().enumerate() {
        order_exprs[k] = Some(resolve_order_target(
            ob.expression,
            statement.items,
            scope,
            arena,
        )?);
    }

    // Project each row (with the window hook) and compute its sort keys.
    let proj_rows: &mut [&[Datum]] = arena
        .alloc_slice_with(count, |_| empty)
        .map_err(|_| arena_full())?;
    let sort_keys: &mut [&[Datum]] = arena
        .alloc_slice_with(count, |_| empty)
        .map_err(|_| arena_full())?;
    for i in 0..count {
        let mut wv = [Datum::Null; MAX_WINDOWS];
        for (w, wval) in win_vals.iter().enumerate().take(win_nodes.len()) {
            wv[w] = wval[i];
        }
        let jr = window_row(scope, rows[i], &offs);
        // Correlated subqueries in the select list / ORDER BY re-evaluate per
        // output row (their outer references resolve to this window row).
        let mut sc: [(*const Expr, Datum, Datum); MAX_SUBQUERIES] =
            [(core::ptr::null(), Datum::Null, Datum::Null); MAX_SUBQUERIES];
        let mut ls = [super::subquery::empty_subquery_list(); MAX_SUBQUERIES];
        let row_subs;
        let subs = if correlated.is_empty() {
            hooks.subs
        } else {
            row_subs = merge_correlated(
                correlated, base, &jr, storage, txid, arena, params, &mut sc, &mut ls,
            )?;
            Some(&row_subs)
        };
        let win_hooks = EvalHooks {
            group: None,
            aggs: None,
            subs,
            windows: Some((win_ptrs, &wv[..win_nodes.len()])),
            catalog: hooks.catalog,
            srf_index: hooks.srf_index,
            project_sets: hooks.project_sets,
            sequences: hooks.sequences,
            merge_action: hooks.merge_action,
        };
        let mut projected = [Datum::Null; MAX_PROJ];
        let np = project_row(
            statement.items,
            scope,
            &jr,
            arena,
            params,
            &win_hooks,
            &mut projected,
            outer,
        )?;
        proj_rows[i] = &*arena
            .alloc_slice_copy(&projected[..np])
            .map_err(|_| arena_full())?;
        let mut keys = [Datum::Null; MAX_WIN_KEYS];
        for (k, oe) in order_exprs.iter().enumerate().take(n_order) {
            keys[k] = eval_full(
                oe.expect("set"),
                arena,
                params,
                &super::Chained { inner: &jr, outer },
                &win_hooks,
            )?;
        }
        sort_keys[i] = &*arena
            .alloc_slice_copy(&keys[..n_order])
            .map_err(|_| arena_full())?;
    }
    Ok((proj_rows, sort_keys))
}

/// Compares two encoded rows' first `n` columns under SQL equality and order,
/// NULLs first — the same semantics `keys_equal` uses to partition rows.
fn compare_encoded_keys(
    storage: &Storage,
    collations: &[Collation],
    a: &[u8],
    b: &[u8],
    at: usize,
    n: usize,
) -> Result<core::cmp::Ordering, SqlError> {
    use core::cmp::Ordering;
    for (k, &collation) in collations.iter().enumerate().take(n) {
        let va = crate::sql::exec::decode_projected_pub(a, at + k);
        let vb = crate::sql::exec::decode_projected_pub(b, at + k);
        let c = match (va.is_null(), vb.is_null()) {
            (true, true) => Ordering::Equal,
            (true, false) => Ordering::Less,
            (false, true) => Ordering::Greater,
            (false, false) => compare_datums_collated(storage, collation, &va, &vb)?,
        };
        if c != Ordering::Equal {
            return Ok(c);
        }
    }
    Ok(Ordering::Equal)
}

/// Compares two encoded rows' ORDER BY keys (decoded, honoring each key's
/// ASC/DESC + NULLS placement), mirroring `cmp_order` for the external runs.
fn compare_encoded_order<'a>(
    storage: &Storage,
    collations: &[Collation],
    a: &[u8],
    b: &[u8],
    at: usize,
    ord: &[OrderBy<'a>],
) -> Result<core::cmp::Ordering, SqlError> {
    use core::cmp::Ordering;
    for (k, o) in ord.iter().enumerate() {
        let va = crate::sql::exec::decode_projected_pub(a, at + k);
        let vb = crate::sql::exec::decode_projected_pub(b, at + k);
        let c = order_by_key(storage, collations[k], &va, &vb, o)?;
        if c != Ordering::Equal {
            return Ok(c);
        }
    }
    Ok(Ordering::Equal)
}

/// The projected column count for a statement's select list: wildcards expand
/// by the scope, expressions occupy one slot.
fn projected_width<'a>(
    items: &[SelectItem<'a>],
    scope: &QueryScope<'a>,
) -> Result<usize, SqlError> {
    let mut width = 0;
    for item in items {
        width += match item {
            SelectItem::Wildcard => scope.star_columns(),
            SelectItem::TableWildcard(qualifier) => scope.qualified_star_columns(qualifier)?,
            SelectItem::RecordStar(base) => record_star_width(base, scope),
            SelectItem::Expr { .. } => 1,
        };
    }
    Ok(width)
}

/// Doubles an arena-backed scratch array, copying the live prefix. Growth is
/// geometric so per-row appends stay amortized linear in the final length.
fn grow_slots<'a, T: Copy>(
    arena: &'a Arena,
    slots: &'a mut [T],
    fill: T,
) -> Result<&'a mut [T], SqlError> {
    let new_len = slots.len().saturating_mul(2).max(2);
    let bigger = arena
        .alloc_slice_with(new_len, |_| fill)
        .map_err(|_| arena_full())?;
    bigger[..slots.len()].copy_from_slice(slots);
    Ok(bigger)
}

/// Spill-mode window execution: sorts the source by (PARTITION BY, ORDER BY)
/// per window call, computes each partition's values against a resident
/// decoded copy, then re-scans the source to project every row with its
/// window values and stream the ordered, deduped, paged result. Every
/// intermediate set lives in the external run stack, so the statement arena
/// holds at most one window partition at a time.
#[allow(clippy::too_many_arguments)]
pub(crate) fn external_window_into<'a>(
    storage: &'a Storage,
    txid: u32,
    statement: &'a Select<'a>,
    from: &'a FromClause<'a>,
    scope: &QueryScope<'a>,
    win_nodes: &[&'a Expr<'a>],
    hooks: &EvalHooks<'_, 'a>,
    correlated: &'a [&'a Expr<'a>],
    base: &SubqueryValues<'a, 'a>,
    arena: &'a Arena,
    params: &[Datum<'a>],
    outer: Option<&dyn ColumnLookup<'a>>,
    limit: u64,
    offset: u64,
    emit: &mut dyn for<'row> FnMut(&[Datum<'row>]) -> Result<bool, SqlError>,
) -> Result<u64, SqlError> {
    let scan_where = window_scan_where(statement, correlated, arena)?;
    let pax_columns = window_pax_columns(statement, scope, from, win_nodes);
    // Flat-row column offsets per table.
    let mut offs = [0usize; MAX_JOIN_TABLES];
    let mut total = 0usize;
    for (t, offset) in offs.iter_mut().enumerate().take(scope.n) {
        *offset = total;
        total += scope.defs[t].expect("resolved").n_columns;
    }
    // Resolve ORDER BY (ordinals → select items).
    let n_order = statement.order_by.len();
    if n_order > MAX_WIN_KEYS {
        return Err(sql_err!(
            sqlstate::TOO_MANY_ARGUMENTS,
            "ORDER BY list too long"
        ));
    }
    let mut order_exprs: [Option<&Expr>; MAX_WIN_KEYS] = [None; MAX_WIN_KEYS];
    for (k, ob) in statement.order_by.iter().enumerate() {
        order_exprs[k] = Some(resolve_order_target(
            ob.expression,
            statement.items,
            scope,
            arena,
        )?);
    }
    let win_ptrs: &[*const Expr] = arena
        .alloc_slice_with(win_nodes.len(), |i| win_nodes[i] as *const Expr)
        .map_err(|_| arena_full())?;

    // Every window function's value for every source row lands in one run,
    // keyed by (row position, window index), so the projection pass re-reads
    // them in lockstep with a fresh source scan.
    let mut win_sorter = storage.external_sorter()?;
    win_sorter.reset();
    let mut win_compare = |left: &[u8], right: &[u8]| -> Result<core::cmp::Ordering, SqlError> {
        let pa = crate::sql::exec::decode_projected_pub(left, 0);
        let pb = crate::sql::exec::decode_projected_pub(right, 0);
        compare_datums(&pa, &pb)
    };
    for (wi, &node) in win_nodes.iter().enumerate() {
        let Expr::Call {
            name: _,
            args: _,
            over: Some(spec),
            ..
        } = node
        else {
            return Err(sql_err!(sqlstate::INTERNAL_ERROR, "not a window function"));
        };
        let n_partition = spec.partition_by.len();
        let n_keys = n_partition + spec.order_by.len();
        let mut partition_collations = [Collation::None; MAX_WIN_KEYS];
        let mut order_collations = [Collation::None; MAX_WIN_KEYS];
        for (index, expression) in spec.partition_by.iter().enumerate() {
            partition_collations[index] = scope.expression_collation(expression)?;
        }
        for (index, order) in spec.order_by.iter().enumerate() {
            order_collations[index] = scope.expression_collation(order.expression)?;
        }
        // Sort the source rows by (PARTITION BY keys, ORDER BY keys) so every
        // partition streams back as one contiguous run segment.
        let mut spec_sorter = storage.external_sorter()?;
        spec_sorter.reset();
        let mut spec_compare =
            |left: &[u8], right: &[u8]| -> Result<core::cmp::Ordering, SqlError> {
                let by_partition = compare_encoded_keys(
                    storage,
                    &partition_collations[..n_partition],
                    left,
                    right,
                    0,
                    n_partition,
                )?;
                if !by_partition.is_eq() {
                    return Ok(by_partition);
                }
                compare_encoded_order(
                    storage,
                    &order_collations[..spec.order_by.len()],
                    left,
                    right,
                    n_partition,
                    spec.order_by,
                )
            };
        let mut position = 0i64;
        scan_source_recycling_with_pax_columns(
            storage,
            scope,
            from,
            txid,
            scan_where,
            arena,
            params,
            hooks,
            outer,
            pax_columns,
            &mut |row| {
                if !row_passes_correlated_where(
                    correlated,
                    statement.where_clause,
                    storage,
                    txid,
                    arena,
                    params,
                    hooks,
                    row,
                )? {
                    return Ok(true);
                }
                // The full flat source row rides along so `compute_window`
                // evaluates every window argument and key against the original
                // row without rescanning.
                let key_vals = arena
                    .alloc_slice_with(n_keys.max(1), |_| Datum::Null)
                    .map_err(|_| arena_full())?;
                for (k, pk) in spec.partition_by.iter().enumerate() {
                    key_vals[k] = eval_full(pk, arena, params, row, hooks)?;
                }
                for (k, ob) in spec.order_by.iter().enumerate() {
                    key_vals[n_partition + k] =
                        eval_full(ob.expression, arena, params, row, hooks)?;
                }
                let row_position = position;
                storage
                    .with_block_store(|blocks| {
                        spec_sorter.push_projected_by(
                            blocks,
                            n_keys + total + 1,
                            |index| {
                                if index < n_keys {
                                    key_vals[index]
                                } else if index < n_keys + total {
                                    let flat = index - n_keys;
                                    for (t, offset) in offs.iter().enumerate().take(scope.n) {
                                        let nc = scope.defs[t].expect("resolved").n_columns;
                                        if flat < offset + nc {
                                            let vals = row.values[t].expect("bound");
                                            return if vals.is_empty() {
                                                Datum::Null
                                            } else {
                                                vals[flat - offset]
                                            };
                                        }
                                    }
                                    unreachable!("flat window column is in scope")
                                } else {
                                    Datum::Int8(row_position)
                                }
                            },
                            &mut spec_compare,
                        )
                    })
                    .expect("spill-attached block store")?;
                position += 1;
                Ok(true)
            },
        )?;
        let spec_run = storage
            .with_block_store(|blocks| spec_sorter.finish(blocks, &mut spec_compare))
            .expect("spill-attached block store")?;
        drop(spec_sorter);
        // Stream the sorted run back partition by partition, computing each
        // partition's window values against a resident decoded copy (one
        // partition is the largest working set the arena ever holds here).
        if let Some(run) = spec_run {
            let mut spec_reader = storage.external_run_reader()?;
            storage
                .with_block_store(|blocks| spec_reader.start(blocks, run))
                .expect("spill-attached block store")?;
            let mark = arena.mark();
            let empty_row: &[Datum] = &[];
            let mut rows: &mut [&[Datum]] = arena
                .alloc_slice_with(16, |_| empty_row)
                .map_err(|_| arena_full())?;
            let mut pos_of: &mut [i64] = arena
                .alloc_slice_with(16, |_| 0)
                .map_err(|_| arena_full())?;
            let mut count = 0usize;
            let mut finish_partition =
                |rows: &[&'a [Datum<'a>]], pos_of: &[i64]| -> Result<(), SqlError> {
                    let out = compute_window(
                        storage, txid, node, rows, scope, &offs, arena, params, hooks,
                    )?;
                    for (r, &pos) in pos_of.iter().enumerate() {
                        storage
                            .with_block_store(|blocks| {
                                win_sorter.push_projected_by(
                                    blocks,
                                    3,
                                    |index| match index {
                                        0 => Datum::Int8(pos),
                                        1 => Datum::Int4(wi as i32),
                                        _ => out[r],
                                    },
                                    &mut win_compare,
                                )
                            })
                            .expect("spill-attached block store")?;
                    }
                    Ok(())
                };
            loop {
                let keep_scanning = {
                    let Some(context) = spec_reader.context() else {
                        break;
                    };
                    let crate::sql::external::ExternalRunContext {
                        row,
                        previous,
                        boundary: _,
                        output: _,
                    } = context;
                    let is_new = match previous {
                        None => true,
                        Some(prior) => !compare_encoded_keys(
                            storage,
                            &partition_collations[..n_partition],
                            prior,
                            row,
                            0,
                            n_partition,
                        )?
                        .is_eq(),
                    };
                    if is_new && count > 0 {
                        finish_partition(&rows[..count], &pos_of[..count])?;
                        // SAFETY: every value allocated since `mark` was
                        // encoded into the window run; none escapes.
                        unsafe { arena.rewind_to(mark) };
                        count = 0;
                        rows = arena
                            .alloc_slice_with(16, |_| empty_row)
                            .map_err(|_| arena_full())?;
                        pos_of = arena
                            .alloc_slice_with(16, |_| 0)
                            .map_err(|_| arena_full())?;
                    }
                    if count == rows.len() {
                        rows = grow_slots(arena, rows, empty_row)?;
                        pos_of = grow_slots(arena, pos_of, 0)?;
                    }
                    // Copy the encoded row into the arena so its decoded
                    // columns survive the reader's next advance.
                    let copied = arena.alloc_slice_copy(row).map_err(|_| arena_full())?;
                    let flat = arena
                        .alloc_slice_with(total.max(1), |_| Datum::Null)
                        .map_err(|_| arena_full())?;
                    for (f, slot) in flat.iter_mut().enumerate() {
                        *slot = crate::sql::exec::decode_projected_col_record(
                            copied,
                            n_keys + f,
                            arena,
                        )?;
                    }
                    rows[count] = &flat[..total];
                    pos_of[count] =
                        match crate::sql::exec::decode_projected_pub(copied, n_keys + total) {
                            Datum::Int8(v) => v,
                            _ => {
                                return Err(sql_err!(
                                    sqlstate::INTERNAL_ERROR,
                                    "window partition position is corrupt"
                                ));
                            }
                        };
                    count += 1;
                    true
                };
                if !keep_scanning {
                    break;
                }
                storage
                    .with_block_store(|blocks| spec_reader.advance(blocks))
                    .expect("spill-attached block store")?;
            }
            if count > 0 {
                finish_partition(&rows[..count], &pos_of[..count])?;
                // SAFETY: the partition's decoded values were consumed into
                // the window run before this rewind.
                unsafe { arena.rewind_to(mark) };
            }
        }
    }
    let win_run = storage
        .with_block_store(|blocks| win_sorter.finish(blocks, &mut win_compare))
        .expect("spill-attached block store")?;
    drop(win_sorter);

    // Re-scan the source, projecting each row with its window values read
    // back from the completed run, and sort the projected rows by ORDER BY.
    let width = projected_width(statement.items, scope)?;
    let mut statement_order_collations = [Collation::None; MAX_WIN_KEYS];
    for (index, order) in statement.order_by.iter().enumerate() {
        let expression = resolve_order_target(order.expression, statement.items, scope, arena)?;
        statement_order_collations[index] = scope.expression_collation(expression)?;
    }
    let mut out_sorter = storage.external_sorter()?;
    out_sorter.reset();
    let mut out_compare = |left: &[u8], right: &[u8]| -> Result<core::cmp::Ordering, SqlError> {
        let by_order = compare_encoded_order(
            storage,
            &statement_order_collations[..statement.order_by.len()],
            left,
            right,
            width,
            statement.order_by,
        )?;
        if statement.distinct && by_order.is_eq() {
            Ok(crate::sql::exec::compare_projected_prefix(
                left, right, width,
            ))
        } else {
            Ok(by_order)
        }
    };
    let mut win_reader = match win_run {
        Some(run) => {
            let mut reader = storage.external_run_reader()?;
            storage
                .with_block_store(|blocks| reader.start(blocks, run))
                .expect("spill-attached block store")?;
            Some(reader)
        }
        None => None,
    };
    let win_count = win_nodes.len();
    let mut position = 0i64;
    scan_source_recycling_with_pax_columns(
        storage,
        scope,
        from,
        txid,
        scan_where,
        arena,
        params,
        hooks,
        outer,
        pax_columns,
        &mut |row| {
            if !row_passes_correlated_where(
                correlated,
                statement.where_clause,
                storage,
                txid,
                arena,
                params,
                hooks,
                row,
            )? {
                return Ok(true);
            }
            // Each window value is detached into the arena so the reader can
            // advance past it before the row is projected.
            let mut wv = [Datum::Null; MAX_WINDOWS];
            for w in 0..win_count {
                let reader = win_reader.as_mut().ok_or_else(|| {
                    sql_err!(sqlstate::INTERNAL_ERROR, "window run is missing rows")
                })?;
                let entry = reader
                    .row()
                    .ok_or_else(|| sql_err!(sqlstate::INTERNAL_ERROR, "window run ended early"))?;
                debug_assert_eq!(
                    crate::sql::exec::decode_projected_pub(entry, 0),
                    Datum::Int8(position)
                );
                let index = match crate::sql::exec::decode_projected_pub(entry, 1) {
                    Datum::Int4(i) => i as usize,
                    _ => {
                        return Err(sql_err!(
                            sqlstate::INTERNAL_ERROR,
                            "window index is corrupt"
                        ));
                    }
                };
                debug_assert_eq!(index, w);
                let value = crate::sql::exec::decode_projected_col_record(entry, 2, arena)?;
                let owned = crate::sql::exec::encode_projected_pub(&[value], arena)?;
                wv[index] = crate::sql::exec::decode_projected_col_record(owned, 0, arena)?;
                storage
                    .with_block_store(|blocks| reader.advance(blocks))
                    .expect("spill-attached block store")?;
            }
            let mut sc: [(*const Expr, Datum, Datum); MAX_SUBQUERIES] =
                [(core::ptr::null(), Datum::Null, Datum::Null); MAX_SUBQUERIES];
            let mut ls = [super::subquery::empty_subquery_list(); MAX_SUBQUERIES];
            let row_subs;
            let subs = if correlated.is_empty() {
                hooks.subs
            } else {
                row_subs = merge_correlated(
                    correlated, base, row, storage, txid, arena, params, &mut sc, &mut ls,
                )?;
                Some(&row_subs)
            };
            let win_hooks = EvalHooks {
                group: None,
                aggs: None,
                subs,
                windows: Some((win_ptrs, &wv[..win_count])),
                catalog: hooks.catalog,
                srf_index: hooks.srf_index,
                project_sets: hooks.project_sets,
                sequences: hooks.sequences,
                merge_action: hooks.merge_action,
            };
            let mut projected = [Datum::Null; MAX_PROJ];
            let np = project_row(
                statement.items,
                scope,
                row,
                arena,
                params,
                &win_hooks,
                &mut projected,
                outer,
            )?;
            debug_assert_eq!(np, width);
            let mut keys = [Datum::Null; MAX_WIN_KEYS];
            for (k, oe) in order_exprs.iter().enumerate().take(n_order) {
                keys[k] = eval_full(
                    oe.expect("set"),
                    arena,
                    params,
                    &super::Chained { inner: row, outer },
                    &win_hooks,
                )?;
            }
            storage
                .with_block_store(|blocks| {
                    out_sorter.push_projected_by(
                        blocks,
                        width + n_order,
                        |index| {
                            if index < width {
                                projected[index]
                            } else {
                                keys[index - width]
                            }
                        },
                        &mut out_compare,
                    )
                })
                .expect("spill-attached block store")?;
            position += 1;
            Ok(true)
        },
    )?;
    let out_run = storage
        .with_block_store(|blocks| out_sorter.finish(blocks, &mut out_compare))
        .expect("spill-attached block store")?;
    drop(out_sorter);

    // Stream the ordered run under DISTINCT / OFFSET / LIMIT.
    let Some(run) = out_run else {
        return Ok(0);
    };
    let mut out_reader = storage.external_run_reader()?;
    storage
        .with_block_store(|blocks| out_reader.start(blocks, run))
        .expect("spill-attached block store")?;
    let mut emitted = 0u64;
    let mut skipped = 0u64;
    loop {
        let keep_scanning = {
            let Some(context) = out_reader.context() else {
                break;
            };
            let crate::sql::external::ExternalRunContext {
                row,
                previous,
                boundary: _,
                output: _,
            } = context;
            let duplicate = statement.distinct
                && previous.is_some_and(|prior| {
                    crate::sql::exec::compare_projected_prefix(row, prior, width).is_eq()
                });
            if duplicate {
                true
            } else if skipped < offset {
                skipped += 1;
                true
            } else if emitted >= limit {
                false
            } else {
                let mut output = [Datum::Null; MAX_PROJ];
                for (column, value) in output.iter_mut().enumerate().take(width) {
                    *value = crate::sql::exec::decode_projected_col_record(row, column, arena)?;
                }
                if !emit(&output[..width])? {
                    false
                } else {
                    emitted += 1;
                    true
                }
            }
        };
        if !keep_scanning {
            break;
        }
        storage
            .with_block_store(|blocks| out_reader.advance(blocks))
            .expect("spill-attached block store")?;
    }
    Ok(emitted)
}

/// Window-function execution to the wire: projects each row with its window
/// values computed over the materialized source, applies DISTINCT and ORDER
/// BY, then pages with LIMIT/OFFSET and emits.
#[allow(clippy::too_many_arguments)]
pub(crate) fn window_select<'a>(
    storage: &'a Storage,
    txid: u32,
    statement: &'a Select<'a>,
    from: &'a FromClause<'a>,
    scope: &QueryScope<'a>,
    win_nodes: &[&'a Expr<'a>],
    hooks: &EvalHooks<'_, 'a>,
    correlated: &'a [&'a Expr<'a>],
    base: &SubqueryValues<'a, 'a>,
    arena: &'a Arena,
    params: &[Datum<'a>],
    limit: u64,
    offset: u64,
    responder: &mut Responder,
) -> Outcome {
    if !statement.group_by.is_empty() || statement.having.is_some() {
        return sql_fail(sql_err!(
            sqlstate::FEATURE_NOT_SUPPORTED,
            "grouped window queries reach this executor only after rewriting"
        ));
    }
    // SELECT DISTINCT: ORDER BY keys must be select-list members.
    if statement.distinct {
        for ob in statement.order_by {
            if matches!(ob.expression, Expr::Int(_)) {
                continue;
            }
            let in_list = statement.items.iter().any(|item| {
                matches!(item, SelectItem::Expr { expression, .. } if **expression == *ob.expression)
            });
            if !in_list {
                return sql_fail(sql_err!(
                    sqlstate::INVALID_COLUMN_REFERENCE,
                    "for SELECT DISTINCT, ORDER BY expressions must appear in select list"
                ));
            }
        }
    }
    if storage.spill_attached() {
        let mut wire_full = false;
        let emitted = match external_window_into(
            storage,
            txid,
            statement,
            from,
            scope,
            win_nodes,
            hooks,
            correlated,
            base,
            arena,
            params,
            None,
            limit,
            offset,
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
        return sql_ok();
    }
    let (proj_rows, sort_keys) = match project_window_rows(
        // The top-level query has no enclosing row to resolve against.
        storage, txid, statement, from, scope, win_nodes, hooks, correlated, base, arena, params,
        None,
    ) {
        Ok(v) => v,
        Err(e) => return sql_fail(e),
    };
    // DISTINCT dedups on the projected row (encoded order-preserving), with
    // each surviving row keeping its sort keys.
    let (proj_rows, sort_keys) = if statement.distinct {
        match dedup_window_rows(proj_rows, sort_keys, arena) {
            Ok(pair) => pair,
            Err(e) => return sql_fail(e),
        }
    } else {
        (proj_rows, sort_keys)
    };
    let count = proj_rows.len();

    // Sort output rows by the ORDER BY keys.
    let order = match arena.alloc_slice_with(count, |i| i) {
        Ok(o) => o,
        Err(_) => return sql_fail(arena_full()),
    };
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

    // Emit under OFFSET/LIMIT.
    let mut emitted = 0u64;
    let mut skipped = 0u64;
    for &i in order.iter() {
        if skipped < offset {
            skipped += 1;
            continue;
        }
        if emitted >= limit {
            break;
        }
        match super::emit_data_row(storage, txid, arena, responder, proj_rows[i]) {
            Ok(Ok(())) => {}
            Ok(Err(error)) => return sql_fail(error),
            Err(wire) => return Err(wire),
        }
        emitted += 1;
    }
    let tag = stack_format!(48, "SELECT {}", emitted);
    responder.command_complete(tag.as_str())?;
    sql_ok()
}

/// Dedups window-projected rows on the projected values (order-preserving
/// encoding), keeping each survivor's sort keys. Used by SELECT DISTINCT
/// with window functions.
#[allow(clippy::type_complexity)]
pub(crate) fn dedup_window_rows<'a>(
    proj_rows: &'a [&'a [Datum<'a>]],
    sort_keys: &'a [&'a [Datum<'a>]],
    arena: &'a Arena,
) -> Result<(&'a [&'a [Datum<'a>]], &'a [&'a [Datum<'a>]]), SqlError> {
    let n = proj_rows.len();
    let index = arena.alloc_slice_with(n, |i| i).map_err(|_| arena_full())?;
    let empty: &[u8] = &[];
    let encoded = arena
        .alloc_slice_with(n, |_| empty)
        .map_err(|_| arena_full())?;
    for i in 0..n {
        if proj_rows[i].iter().any(|d| matches!(d, Datum::Bpchar(_))) {
            let key = arena
                .alloc_slice_with(proj_rows[i].len(), |j| {
                    crate::sql::eval::text_view(proj_rows[i][j])
                })
                .map_err(|_| arena_full())?;
            encoded[i] = crate::sql::exec::encode_projected_pub(key, arena)?;
        } else {
            encoded[i] = crate::sql::exec::encode_projected_pub(proj_rows[i], arena)?;
        }
    }
    index.sort_unstable_by(|&a, &b| encoded[a].cmp(encoded[b]));
    let mut unique = 0usize;
    for k in 0..n {
        let same = k > 0 && encoded[index[k]] == encoded[index[k - 1]];
        if !same {
            index[unique] = index[k];
            unique += 1;
        }
    }
    let empty_row: &[Datum] = &[];
    let out_rows = arena
        .alloc_slice_with(unique, |_| empty_row)
        .map_err(|_| arena_full())?;
    let out_keys = arena
        .alloc_slice_with(unique, |_| empty_row)
        .map_err(|_| arena_full())?;
    for k in 0..unique {
        out_rows[k] = proj_rows[index[k]];
        out_keys[k] = sort_keys[index[k]];
    }
    Ok((&*out_rows, &*out_keys))
}

/// Compares two precomputed sort-key tuples honoring ASC/DESC + NULLS order.
pub(crate) fn cmp_key_rows(a: &[Datum], b: &[Datum], ord: &[OrderBy]) -> core::cmp::Ordering {
    use core::cmp::Ordering;
    for (k, o) in ord.iter().enumerate() {
        let (va, vb) = (&a[k], &b[k]);
        let base = match (va.is_null(), vb.is_null()) {
            (true, true) => Ordering::Equal,
            (true, false) => {
                if o.nulls_first {
                    Ordering::Less
                } else {
                    Ordering::Greater
                }
            }
            (false, true) => {
                if o.nulls_first {
                    Ordering::Greater
                } else {
                    Ordering::Less
                }
            }
            (false, false) => compare_datums(va, vb).unwrap_or(Ordering::Equal),
        };
        let c = if o.descending && !va.is_null() && !vb.is_null() {
            base.reverse()
        } else {
            base
        };
        if c != Ordering::Equal {
            return c;
        }
    }
    Ordering::Equal
}
