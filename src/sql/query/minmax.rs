//! `preprocess_minmax_aggregates` over a query of one plain local table: a
//! select list whose aggregates are all `min` or `max` of a column reads
//! each as an InitPlan, the first row of an index scan ordered by the
//! column with its nulls excluded and the query's conditions applied, when
//! those scans cost less than aggregating the table (`add_path`).
//! https://github.com/postgres/postgres/blob/REL_18_6/src/backend/optimizer/plan/planagg.c

use crate::mem::arena::{Arena, ArenaList};
use crate::sql::ast::{BinaryOp, Expr, OrderBy, RelationInheritance, Select, SelectItem};
use crate::sql::eval::{SqlError, arena_full};
use crate::storage::Storage;

use super::index_paths::{
    AggInput, AggStrategy, Planned, QueryShape, SortKind, aggregate_costs, cost_agg, query_shape,
    scan_path,
};
use super::scope::{QueryScope, ResolvedColumn};
use super::{
    AggregateRoutine, aggregate_routine, streaming_pax_columns_ordered, walk_expression_tree,
};

/// `STD_FUZZ_FACTOR`, by which `add_path` lets a cheaper path win.
const FUZZ_FACTOR: f64 = 1.01;

/// The FROM-less query reading each `min` and `max` of `statement` from its
/// InitPlan, or `None` when PostgreSQL's planner keeps the aggregate.
pub(crate) fn local_minmax_subquery<'a>(
    storage: &Storage,
    txid: u32,
    statement: &'a Select<'a>,
    arena: &'a Arena,
) -> Result<Option<&'a Select<'a>>, SqlError> {
    let Some(from) = statement.from.as_ref() else {
        return Ok(None);
    };
    let base = &from.base;
    if !from.joins.is_empty()
        || statement.set_body.is_some()
        || !statement.with.is_empty()
        || base.subquery.is_some()
        || base.is_function_source()
        || base.cte.is_some()
        || base.sample.is_some()
        || base.col_alias.is_some()
        || base.view_access.is_some()
        || base.lateral
        || !statement.group_by.is_empty()
        || !statement.grouping_sets.is_empty()
        || statement.having.is_some()
        || !statement.windows.is_empty()
        || statement.distinct
        || !statement.distinct_on.is_empty()
        || !statement.locking.is_empty()
        || !statement.order_by.is_empty()
        || statement.limit.is_some()
        || statement.offset.is_some()
        || statement.where_clause.is_some_and(Expr::contains_subquery)
    {
        return Ok(None);
    }
    let scope = QueryScope::resolve_schema(storage, from, txid, arena, None)?;
    let slot = scope.slots[0];
    if scope.n != 1 || scope.derived[0].is_some() || slot == usize::MAX {
        return Ok(None);
    }
    let Some(table) = scope.defs.stored(0) else {
        return Ok(None);
    };
    if table.kind != crate::storage::TableKind::Local
        || table.partition.is_partitioned()
        || (base.inheritance != RelationInheritance::Only
            && storage.relation_has_descendants(slot, txid))
    {
        return Ok(None);
    }
    let columns = super::CatalogScopeCols {
        scope: &scope,
        outer: None,
        storage,
        txid,
        arena,
    };
    // Every aggregate a `min` or `max` of a column (`can_minmax_aggs`), each
    // distinct one once.
    let mut aggregates = ArenaList::<&'a Expr<'a>>::new(arena);
    let mut eligible = true;
    for item in statement.items {
        let SelectItem::Expr { expression, .. } = item else {
            return Ok(None);
        };
        if expression.contains_subquery() || super::expr_has_window(expression) {
            return Ok(None);
        }
        let mut calls = ArenaList::<&'a Expr<'a>>::new(arena);
        let mut full = false;
        walk_expression_tree(expression, &mut |node| {
            if node.is_aggregate_use() && calls.push(node).is_err() {
                full = true;
            }
        })?;
        if full {
            return Err(arena_full());
        }
        for &call in calls.as_slice() {
            let Expr::Call {
                name,
                args: [argument],
                distinct: _,
                order_by: [],
                filter: None,
                over: None,
                ..
            } = call
            else {
                eligible = false;
                continue;
            };
            let bare = name.rsplit('.').next().unwrap_or(name);
            let column = match argument {
                Expr::Column { qualifier, name } => scope.find_column(*qualifier, name).ok(),
                _ => None,
            };
            if !matches!(bare, "min" | "max")
                || !matches!(column, Some(ResolvedColumn::Table(0, _)))
                || !matches!(
                    aggregate_routine(call, storage, txid, &columns)?,
                    Some(AggregateRoutine::Builtin(_))
                )
            {
                eligible = false;
                continue;
            }
            if !aggregates.as_slice().contains(&call) {
                aggregates.push(call).map_err(|_| arena_full())?;
            }
        }
    }
    if !eligible || aggregates.as_slice().is_empty() {
        return Ok(None);
    }
    let only = base.inheritance == RelationInheritance::Only;
    // Each aggregate's subquery and the cost of its first row
    // (`build_minmax_path`).
    let subqueries = arena
        .alloc_slice_with(aggregates.as_slice().len(), |_| &Expr::Null)
        .map_err(|_| arena_full())?;
    let mut initplans = 0.0;
    for (at, &aggregate) in aggregates.as_slice().iter().enumerate() {
        let Expr::Call {
            name,
            args: [argument],
            ..
        } = aggregate
        else {
            unreachable!("checked above")
        };
        let descending = name.ends_with("max");
        // PostgreSQL drops the `IS NOT NULL` of a column its validated NOT
        // NULL constraint keeps from holding a NULL
        // (`restriction_is_always_true`).
        let never_null = match argument {
            Expr::Column { name, .. } => table.column_index(name).is_some_and(|column| {
                let column = &table.columns()[column];
                column.not_null != crate::storage::NotNullOrigin::Nullable
                    && column.not_null_validated
            }),
            _ => false,
        };
        let where_clause = if never_null {
            statement.where_clause
        } else {
            let not_null = arena
                .alloc(Expr::IsNull {
                    operand: argument,
                    negated: true,
                })
                .map_err(|_| arena_full())?;
            Some(match statement.where_clause {
                Some(condition) => &*arena
                    .alloc(Expr::Binary {
                        operator: BinaryOp::And,
                        left: not_null,
                        right: condition,
                        source_position: crate::sql::ast::SourcePosition::UNKNOWN,
                    })
                    .map_err(|_| arena_full())?,
                None => not_null,
            })
        };
        let items = arena
            .alloc_slice_copy(&[SelectItem::Expr {
                expression: argument,
                alias: None,
            }])
            .map_err(|_| arena_full())?;
        let order_by = arena
            .alloc_slice_copy(&[OrderBy {
                expression: argument,
                descending,
                nulls_first: descending,
            }])
            .map_err(|_| arena_full())?;
        let one = arena.alloc(Expr::Int(1)).map_err(|_| arena_full())?;
        let subquery = arena
            .alloc(Select {
                items,
                where_clause,
                order_by,
                limit: Some(one),
                offset: None,
                minmax_subquery: Some(core::ptr::from_ref(aggregate) as usize),
                ..*statement
            })
            .map_err(|_| arena_full())?;
        let shape = query_shape(
            storage,
            txid,
            &scope,
            items,
            order_by,
            Some(one),
            None,
            &[],
            arena,
        )?;
        let shape = QueryShape {
            only,
            output: streaming_pax_columns_ordered(&scope, from, items, None, order_by)
                .selected_columns(0),
            ..shape
        };
        let planned = scan_path(
            storage,
            &scope,
            txid,
            where_clause,
            streaming_pax_columns_ordered(&scope, from, items, where_clause, order_by)
                .selected_columns(0),
            &[],
            arena,
            shape,
        )?;
        // Only a path presorted by the argument serves
        // (`get_cheapest_fractional_path_for_pathkeys`).
        let Planned::Chosen(path) = planned else {
            return Ok(None);
        };
        let Some((_, first_row, _)) = path.limit_cost.filter(|_| path.sort == SortKind::None)
        else {
            return Ok(None);
        };
        initplans += first_row;
        subqueries[at] = &*arena
            .alloc(Expr::Subquery(subquery))
            .map_err(|_| arena_full())?;
    }
    // `create_minmaxagg_path` against the plain aggregate over the cheapest
    // path, which `add_path` keeps only when it is fuzzily cheaper.
    let minmax_total = initplans + crate::sql::guc::planner_value("cpu_tuple_cost");
    // `DISTINCT` only adds to the aggregate's cost, so the subqueries that
    // beat it without beat it with.
    let expressions = arena
        .alloc_slice_with(aggregates.as_slice().len(), |_| &Expr::Null)
        .map_err(|_| arena_full())?;
    for (slot, &aggregate) in expressions.iter_mut().zip(aggregates.as_slice()) {
        let mut plain = *aggregate;
        if let Expr::Call { distinct, .. } = &mut plain {
            *distinct = false;
        }
        *slot = &*arena.alloc(plain).map_err(|_| arena_full())?;
    }
    let Some(aggregate) = aggregate_costs(expressions, storage, txid, &columns, arena)?
        .filter(|costs| !costs.ordered)
    else {
        return Ok(None);
    };
    let planned = scan_path(
        storage,
        &scope,
        txid,
        statement.where_clause,
        streaming_pax_columns_ordered(&scope, from, statement.items, statement.where_clause, &[])
            .selected_columns(0),
        &[],
        arena,
        QueryShape {
            only,
            output: streaming_pax_columns_ordered(&scope, from, statement.items, None, &[])
                .selected_columns(0),
            ..QueryShape::default()
        },
    )?;
    let Planned::Chosen(path) = planned else {
        return Ok(None);
    };
    let (_, aggregate_total, _) = cost_agg(
        AggStrategy::Plain,
        &aggregate,
        0,
        1.0,
        AggInput {
            startup: path.startup_cost,
            total: path.total_cost,
            rows: path.rows,
            width: f64::from(path.width),
        },
    );
    if aggregate_total * FUZZ_FACTOR < minmax_total {
        return Ok(None);
    }
    // The select list reads each subquery's value where it read the
    // aggregate, under the names it had.
    let rewritten = arena
        .alloc_slice_copy(statement.items)
        .map_err(|_| arena_full())?;
    for item in rewritten.iter_mut() {
        let SelectItem::Expr { expression, alias } = item else {
            unreachable!("checked above")
        };
        *alias = Some(alias.unwrap_or_else(|| crate::sql::exec::derived_name(expression)));
        *expression = crate::sql::ast::rewrite_expression(
            expression,
            arena,
            &mut |node: &'a Expr<'a>| -> Result<Option<&'a Expr<'a>>, crate::mem::arena::ArenaFull> {
                Ok(aggregates
                    .as_slice()
                    .iter()
                    .position(|aggregate| *aggregate == node)
                    .map(|at| subqueries[at]))
            },
        )
        .map_err(|_| arena_full())?;
    }
    let outer = arena
        .alloc(Select {
            items: rewritten,
            from: None,
            where_clause: None,
            ..*statement
        })
        .map_err(|_| arena_full())?;
    Ok(Some(outer))
}
