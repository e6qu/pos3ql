//! Whether a cursor declared without `SCROLL` or `NO SCROLL` scrolls.
//!
//! PostgreSQL makes such a cursor scrollable when it locks no rows and its
//! plan can run backward (`ExecSupportsBackwardScan`): table, function,
//! `VALUES` and CTE scans, sorts, and limits, subquery scans and appends over
//! them. Joins, aggregation, `DISTINCT`, window functions, set-returning
//! functions in the output list, sampling, `UNION`/`INTERSECT`/`EXCEPT` and a
//! `FROM`-less query do not. pos3ql decides from the query shape, after views
//! and inlined CTEs are expanded, the plan PostgreSQL chooses for it:
//!
//! - `ORDER BY` over a `FROM` clause adds a sort on top, which scrolls,
//!   unless `DISTINCT ON` or a window function with its own ordering or
//!   partitioning must run above that sort, or the first key is a column an
//!   equality or `NATURAL` join matches on, which PostgreSQL merge-joins
//!   already sorted;
//! - an `IN` or correlated `EXISTS`/`NOT EXISTS` subquery among the top-level
//!   `AND` conditions of `WHERE` is a semi- or anti-join, which does not; an
//!   `EXISTS` subquery counts as correlated when it has a `WHERE` clause;
//! - a `FROM`-less one-row subquery joined without a condition is folded
//!   away;
//! - `UNION ALL` scrolls when every branch does, and multirow `VALUES` does.

use super::*;
use crate::sql::ast::{JoinKind, SetOp, SetOperationSyntax, SetTree, UnaryOp};

/// Whether the plan of a cursor query over `select` can scan backward.
pub(crate) fn select_scans_backward(select: &Select<'_>, storage: &Storage, txid: u32) -> bool {
    if !select.locking.is_empty() {
        return false;
    }
    if let Some(body) = select.set_body {
        return set_scans_backward(body, !select.order_by.is_empty(), storage, txid);
    }
    let Some(from) = select.from.as_ref() else {
        return false;
    };
    let windows = select
        .items
        .iter()
        .filter_map(item_expression)
        .chain(select.order_by.iter().map(|key| key.expression))
        .any(expr_has_window);
    if !select.order_by.is_empty() {
        if merge_join_order(from, select.order_by[0].expression) {
            return false;
        }
        let ordered_windows =
            windows && (!select.windows.is_empty() || window_calls_ordered(select.items));
        return select.distinct_on.is_empty() && !ordered_windows;
    }
    if select.distinct
        || !select.distinct_on.is_empty()
        || !select.group_by.is_empty()
        || !select.grouping_sets.is_empty()
        || select.having.is_some()
        || windows
        || select
            .items
            .iter()
            .filter_map(item_expression)
            .any(has_aggregate_use)
        || has_project_set(select.items, storage, txid)
    {
        return false;
    }
    from_scans_backward(from, storage, txid)
        && select
            .where_clause
            .is_none_or(|condition| !has_semijoin(condition))
}

/// Whether a set-operation query's plan can scan backward.
pub(crate) fn set_query_scans_backward(query: &SetQuery<'_>, storage: &Storage, txid: u32) -> bool {
    query.locking.is_empty()
        && set_scans_backward(query.body, !query.order_by.is_empty(), storage, txid)
}

fn set_scans_backward(tree: &SetTree<'_>, ordered: bool, storage: &Storage, txid: u32) -> bool {
    match tree {
        SetTree::Select(select) => ordered || select_scans_backward(select, storage, txid),
        SetTree::Op {
            syntax: SetOperationSyntax::Values,
            ..
        } => true,
        SetTree::Op {
            operator: SetOp::Union,
            all: true,
            left,
            right,
            ..
        } => {
            ordered
                || (set_scans_backward(left, false, storage, txid)
                    && set_scans_backward(right, false, storage, txid))
        }
        SetTree::Op { .. } => false,
    }
}

fn from_scans_backward(from: &FromClause<'_>, storage: &Storage, txid: u32) -> bool {
    let base = &from.base;
    if base.sample.is_some()
        || base.join_members.is_some()
        || !from.joins.iter().all(|join| {
            matches!(
                join.kind,
                JoinKind::Inner | JoinKind::Cross | JoinKind::Comma
            ) && join.on.is_none()
                && join.using.is_none()
                && !join.natural
                && join.table.subquery.is_some_and(is_one_constant_row)
        })
    {
        return false;
    }
    match base.subquery {
        Some(subquery) => select_scans_backward(subquery, storage, txid),
        // Tables, function scans and materialized CTE scans run backward.
        None => true,
    }
}

/// A `FROM`-less subquery producing one row, which PostgreSQL folds into the
/// query it is joined to.
fn is_one_constant_row(select: &Select<'_>) -> bool {
    select.from.is_none()
        && select.set_body.is_none()
        && select.where_clause.is_none()
        && select.having.is_none()
        && select.group_by.is_empty()
        && select.limit.is_none()
        && select.offset.is_none()
        && !select
            .items
            .iter()
            .filter_map(item_expression)
            .any(|item| item.is_aggregate_use() || expr_has_window(item))
}

/// An `ORDER BY` led by a column an equality join matches on: PostgreSQL
/// merge-joins sorted inputs, so no sort runs above the join.
fn merge_join_order(from: &FromClause<'_>, first: &Expr<'_>) -> bool {
    let Expr::Column { name, .. } = first else {
        return false;
    };
    let names =
        |expression: &Expr<'_>| matches!(expression, Expr::Column { name: key, .. } if key == name);
    fn equality_on(condition: &Expr<'_>, names: &dyn Fn(&Expr<'_>) -> bool) -> bool {
        match condition {
            Expr::Binary {
                operator: crate::sql::ast::BinaryOp::And,
                left,
                right,
                ..
            } => equality_on(left, names) || equality_on(right, names),
            Expr::Binary {
                operator: crate::sql::ast::BinaryOp::Eq,
                left,
                right,
                ..
            } => names(left) || names(right),
            _ => false,
        }
    }
    from.joins.iter().any(|join| {
        join.natural
            || join
                .using
                .is_some_and(|using| using.columns.iter().any(|column| column == name))
            || join.on.is_some_and(|on| equality_on(on, &names))
    })
}

fn item_expression<'a>(item: &SelectItem<'a>) -> Option<&'a Expr<'a>> {
    match item {
        SelectItem::Expr { expression, .. } | SelectItem::RecordStar(expression) => {
            Some(expression)
        }
        SelectItem::Wildcard(_) | SelectItem::TableWildcard(_) => None,
    }
}

fn has_aggregate_use(expression: &Expr<'_>) -> bool {
    if expression.is_aggregate_use() {
        return true;
    }
    let mut found = false;
    // The walk itself cannot fail; a subquery's aggregates group its own rows.
    let _ = walk_children(expression, &mut |child| {
        if !found && !matches!(child, Expr::Subquery(_) | Expr::Exists(_)) {
            found = has_aggregate_use(child);
        }
        Ok(())
    });
    found
}

/// Whether an inline window specification orders or partitions its rows,
/// so its own sort runs below the window function.
fn window_calls_ordered(items: &[SelectItem<'_>]) -> bool {
    fn ordered(expression: &Expr<'_>) -> bool {
        if let Expr::Call {
            over: Some(window), ..
        } = expression
            && (!window.partition_by.is_empty() || !window.order_by.is_empty())
        {
            return true;
        }
        let mut found = false;
        // The walk itself cannot fail.
        let _ = walk_children(expression, &mut |child| {
            if !found {
                found = ordered(child);
            }
            Ok(())
        });
        found
    }
    items.iter().filter_map(item_expression).any(ordered)
}

/// An `IN` or correlated `EXISTS` subquery among the top-level `AND`
/// conditions, which PostgreSQL turns into a semi- or anti-join.
fn has_semijoin(condition: &Expr<'_>) -> bool {
    match condition {
        Expr::Binary {
            operator: crate::sql::ast::BinaryOp::And,
            left,
            right,
            ..
        } => has_semijoin(left) || has_semijoin(right),
        Expr::InSubquery { negated: false, .. } | Expr::QuantifiedSubquery { all: false, .. } => {
            true
        }
        Expr::Exists(select) => select.where_clause.is_some(),
        Expr::Unary {
            operator: UnaryOp::Not,
            operand: Expr::Exists(select),
            ..
        } => select.where_clause.is_some(),
        _ => false,
    }
}
