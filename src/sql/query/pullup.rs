//! Pulls simple subqueries in FROM up into the query that reads them, as
//! PostgreSQL's planner does (`pull_up_subqueries`): the subquery's FROM
//! items join the outer query, its WHERE joins the outer restrictions, and
//! each reference to one of its output columns becomes that column's
//! expression. Evaluation order then matches PostgreSQL: an outer filter runs
//! before the subquery's output expressions, and an output column nothing
//! reads is never computed (`SELECT x FROM (SELECT 10/x AS q, x FROM t) s
//! WHERE x <> 0` cannot divide by zero).
//!
//! A subquery is pulled up only when the rewrite is exact: no grouping,
//! aggregates, window functions, `DISTINCT`, ordering, bounds, locking,
//! set-returning or volatile output, and no name whose binding the merge
//! could change. Every other subquery keeps its own materialized scope and
//! receives the outer `WHERE` conjuncts that read only its columns. Grouped
//! queries move `HAVING` conjuncts that need no aggregate into `WHERE`, as
//! PostgreSQL's `subquery_planner` does.

use crate::mem::arena::Arena;
use crate::sql::ast::{
    CastBinding, Expr, FromClause, Join, JoinKind, OrderBy, Select, SelectItem, SetTree,
    SourcePosition, TableRef, rewrite_expression,
};
use crate::sql::eval::{ColumnLookup, SqlError, sqlstate};
use crate::sql_err;
use crate::storage::Storage;

use super::{CatalogScopeCols, QueryScope, ResolvedColumn, arena_full};

/// Why a rewrite stopped: the subquery stays in place, or a real error.
enum Stop {
    Keep,
    /// The statement arena is exhausted.
    Full,
}

impl Stop {
    fn full() -> Self {
        Self::Full
    }
}

/// An analysis error leaves the subquery in place; the ordinary path then
/// reports it where PostgreSQL would.
impl From<SqlError> for Stop {
    fn from(_: SqlError) -> Self {
        Self::Keep
    }
}

impl From<crate::mem::arena::ArenaFull> for Stop {
    fn from(_: crate::mem::arena::ArenaFull) -> Self {
        Self::Full
    }
}

type Step<T> = Result<T, Stop>;

/// Returns `select` with every pullable FROM subquery merged into it, or
/// `select` itself when none is.
pub(super) fn pull_up_subqueries<'a>(
    select: &'a Select<'a>,
    storage: &'a Storage,
    txid: u32,
    arena: &'a Arena,
    outer: Option<&dyn ColumnLookup<'a>>,
) -> Result<&'a Select<'a>, SqlError> {
    let mut current = select;
    let mut next = 0usize;
    // A grouped query names a subquery's columns in its ungrouped-column
    // errors, which merging the subquery would rename; there a subquery that
    // only renames columns stays in place, as merging it changes no result.
    let grouped = grouped_query(select, storage, txid, arena);
    while let Some(from) = current.from.as_ref()
        && candidate_query(current)
    {
        let count = 1 + from.joins.len();
        let Some(position) = (next..count).find(|&index| {
            item(from, index)
                .subquery
                .is_some_and(|body| !grouped || computes_outputs(body))
        }) else {
            break;
        };
        let mark = arena.mark();
        match pull_up_one(current, position, storage, txid, arena, outer) {
            Ok(merged) => current = merged,
            Err(Stop::Keep) => {
                // SAFETY: nothing allocated since `mark` is referenced: the
                // attempt's scopes and partial rewrites are discarded.
                unsafe { arena.rewind_to(mark) };
                next = position + 1;
            }
            Err(Stop::Full) => return Err(arena_full()),
        }
    }
    for step in [push_down_restrictions, move_having_to_where] {
        let mark = arena.mark();
        match step(current, storage, txid, arena, outer) {
            Ok(rewritten) => current = rewritten,
            Err(Stop::Keep) => {
                // SAFETY: as above, the failed attempt's allocations are unused.
                unsafe { arena.rewind_to(mark) };
            }
            Err(Stop::Full) => return Err(arena_full()),
        }
    }
    Ok(current)
}

/// Moves each `HAVING` conjunct without aggregates, volatile functions or
/// subqueries into `WHERE`, as PostgreSQL's `subquery_planner` does, so it
/// filters rows before any aggregate reads them. Without `GROUP BY` a copy
/// stays in `HAVING`, which still decides whether the one aggregate row is
/// returned. Grouping sets keep their `HAVING`.
fn move_having_to_where<'a>(
    select: &'a Select<'a>,
    storage: &'a Storage,
    txid: u32,
    arena: &'a Arena,
    outer: Option<&dyn ColumnLookup<'a>>,
) -> Step<&'a Select<'a>> {
    let Some(having) = select.having else {
        return Ok(select);
    };
    if !select.grouping_sets.is_empty() || select.set_body.is_some() {
        return Ok(select);
    }
    let scope = select
        .from
        .as_ref()
        .map(|from| QueryScope::resolve_schema(storage, from, txid, arena, outer))
        .transpose()?;
    let scoped = scope.as_ref().map(|scope| CatalogScopeCols {
        scope,
        outer: None,
        storage,
        txid,
        arena,
    });
    let fromless = super::super::exec::CatalogCols {
        definition: None,
        alias: None,
        output_aliases: &[],
        storage,
        txid,
        arena,
    };
    let columns: &dyn super::super::exec::ColTypeResolver = match scoped.as_ref() {
        Some(columns) => columns,
        None => &fromless,
    };
    let mut conjuncts = crate::mem::arena::ArenaList::new(arena);
    flatten_and(Some(having), &mut conjuncts).map_err(|_| Stop::full())?;
    let grouped = !select.group_by.is_empty();
    let mut kept = None;
    let mut moved = None;
    for conjunct in conjuncts.as_slice() {
        let stays = !grouped_expression(conjunct, select.group_by)
            || conjunct.contains_subquery()
            || super::expr_has_aggregate(conjunct, storage, txid, columns)?
            || super::expr_has_window(conjunct)
            || super::predicate_safety::expression_properties(
                conjunct,
                columns,
                storage,
                txid,
                super::predicate_safety::AnalysisStage::RuntimeProjection,
            )?
            .volatile;
        if stays || !grouped {
            kept = super::and_where(kept, Some(conjunct), arena)?;
        }
        if !stays {
            moved = super::and_where(moved, Some(conjunct), arena)?;
        }
    }
    if moved.is_none() {
        return Ok(select);
    }
    Ok(&*arena
        .alloc(Select {
            where_clause: super::and_where(select.where_clause, moved, arena)?,
            having: kept,
            ..*select
        })
        .map_err(|_| Stop::full())?)
}

/// Moves each outer `WHERE` conjunct that reads only one subquery that
/// stayed in place into that subquery, as PostgreSQL's
/// `set_subquery_pathlist` does, so it filters rows before the subquery's
/// output expressions run. The safety rules (bounds, volatile, window and
/// `DISTINCT ON` outputs, set-operation arms) are the view-filter ones.
fn push_down_restrictions<'a>(
    select: &'a Select<'a>,
    storage: &'a Storage,
    txid: u32,
    arena: &'a Arena,
    outer: Option<&dyn ColumnLookup<'a>>,
) -> Step<&'a Select<'a>> {
    let (Some(from), Some(condition)) = (select.from.as_ref(), select.where_clause) else {
        return Ok(select);
    };
    let count = 1 + from.joins.len();
    if !(0..count).any(|index| item(from, index).subquery.is_some()) {
        return Ok(select);
    }
    let scope = QueryScope::resolve_schema(storage, from, txid, arena, outer)?;
    let mut remaining = Some(condition);
    let mut changed = false;
    let mut rewritten_from = *from;
    let mut joins: Option<&'a mut [Join<'a>]> = None;
    for position in 0..count {
        let table = item(from, position);
        let Some(body) = table.subquery else {
            continue;
        };
        if table.lateral
            || (position > 0
                && matches!(
                    from.joins[position - 1].kind,
                    JoinKind::Left | JoinKind::Full
                ))
            || from.joins[position..]
                .iter()
                .any(|join| matches!(join.kind, JoinKind::Right | JoinKind::Full))
        {
            continue;
        }
        let Some(definition) = scope.defs.get(position) else {
            continue;
        };
        let alias = scope.names[position];
        let mut eligible = None;
        let mut kept = None;
        let mut conjuncts = crate::mem::arena::ArenaList::new(arena);
        flatten_and(remaining, &mut conjuncts).map_err(|_| Stop::full())?;
        for conjunct in conjuncts.as_slice() {
            let mut columns = false;
            if reads_only(conjunct, &scope, position, alias, &mut columns) && columns {
                eligible = super::and_where(eligible, Some(conjunct), arena)?;
            } else {
                kept = super::and_where(kept, Some(conjunct), arena)?;
            }
        }
        if eligible.is_none() {
            continue;
        }
        let barrier = table
            .view_access
            .map_or(crate::storage::ViewSecurityBarrier::Default, |slot| {
                storage.view(usize::from(slot)).security_barrier_for(txid)
            });
        let (filtered, residual) = super::filter_view_rows(
            body,
            eligible,
            definition,
            Some(alias),
            barrier,
            storage,
            txid,
            arena,
        )?;
        if core::ptr::eq(filtered, body) {
            continue;
        }
        changed = true;
        remaining = super::and_where(residual, kept, arena)?;
        let mut moved = *table;
        moved.subquery = Some(filtered);
        if position == 0 {
            rewritten_from.base = moved;
        } else {
            let slice = match joins.take() {
                Some(slice) => slice,
                None => arena
                    .alloc_slice_copy(from.joins)
                    .map_err(|_| Stop::full())?,
            };
            slice[position - 1].table = moved;
            joins = Some(slice);
        }
    }
    if !changed {
        return Ok(select);
    }
    if let Some(joins) = joins {
        rewritten_from.joins = joins;
    }
    Ok(&*arena
        .alloc(Select {
            from: Some(rewritten_from),
            where_clause: remaining,
            ..*select
        })
        .map_err(|_| Stop::full())?)
}

/// Whether every column `expression` reads is inside one of the grouping
/// keys, so it is valid in `HAVING` and means the same per input row.
/// Anything else stays in `HAVING`, where the grouping rule reports it.
fn grouped_expression(expression: &Expr<'_>, keys: &[&Expr<'_>]) -> bool {
    if keys.iter().any(|key| **key == *expression) {
        return true;
    }
    match expression {
        Expr::Column { .. }
        | Expr::WholeRow(_)
        | Expr::SchemaColumn { .. }
        | Expr::RoutineParam { .. }
        | Expr::RecursiveState { .. } => false,
        _ => {
            let mut grouped = true;
            let _ = super::walk_children(expression, &mut |child| {
                grouped &= grouped_expression(child, keys);
                Ok(())
            });
            grouped
        }
    }
}

fn flatten_and<'a>(
    condition: Option<&'a Expr<'a>>,
    out: &mut crate::mem::arena::ArenaList<'a, &'a Expr<'a>>,
) -> Result<(), crate::mem::arena::ArenaFull> {
    match condition {
        None => Ok(()),
        Some(Expr::Binary {
            operator: crate::sql::ast::BinaryOp::And,
            left,
            right,
            ..
        }) => {
            flatten_and(Some(left), out)?;
            flatten_and(Some(right), out)
        }
        Some(condition) => out.push(condition),
    }
}

/// Whether `expression` reads columns of FROM item `position` and nothing
/// else that could bind elsewhere.
fn reads_only(
    expression: &Expr<'_>,
    scope: &QueryScope<'_>,
    position: usize,
    alias: &str,
    columns: &mut bool,
) -> bool {
    match expression {
        Expr::Column { qualifier, name } => {
            if qualifier.is_some_and(|qualifier| qualifier != alias) {
                return false;
            }
            *columns = true;
            matches!(
                scope.find_column(*qualifier, name),
                Ok(ResolvedColumn::Table(table, _)) if table == position
            )
        }
        Expr::WholeRow(_)
        | Expr::SchemaColumn { .. }
        | Expr::RoutineParam { .. }
        | Expr::RecursiveState { .. }
        | Expr::Subquery(_)
        | Expr::RowSubquery { .. }
        | Expr::InSubquery { .. }
        | Expr::QuantifiedSubquery { .. }
        | Expr::Exists(_)
        | Expr::ArraySubquery(_) => false,
        _ => {
            let mut only = true;
            let _ = super::walk_children(expression, &mut |child| {
                only &= reads_only(child, scope, position, alias, columns);
                Ok(())
            });
            only
        }
    }
}

fn item<'f, 'a>(from: &'f FromClause<'a>, index: usize) -> &'f TableRef<'a> {
    if index == 0 {
        &from.base
    } else {
        &from.joins[index - 1].table
    }
}

/// Outer-query shapes the merge keeps exact.
fn grouped_query(select: &Select<'_>, storage: &Storage, txid: u32, arena: &Arena) -> bool {
    if !select.group_by.is_empty() || !select.grouping_sets.is_empty() || select.having.is_some() {
        return true;
    }
    let columns = crate::sql::exec::CatalogCols {
        definition: None,
        alias: None,
        output_aliases: &[],
        storage,
        txid,
        arena,
    };
    select
        .items
        .iter()
        .filter_map(|item| match item {
            SelectItem::Expr { expression, .. } | SelectItem::RecordStar(expression) => {
                Some(*expression)
            }
            _ => None,
        })
        .chain(select.order_by.iter().map(|order| order.expression))
        // An expression whose calls do not resolve here is treated as grouped.
        .any(|expression| {
            super::expr_has_aggregate(expression, storage, txid, &columns).unwrap_or(true)
        })
}

fn candidate_query(select: &Select<'_>) -> bool {
    select.set_body.is_none()
        && select.with.is_empty()
        && select.from.as_ref().is_some_and(|from| {
            // A parenthesized join's members are hidden behind its alias,
            // which merging would change.
            core::iter::once(&from.base)
                .chain(from.joins.iter().map(|join| &join.table))
                .all(|table| table.join_members.is_none())
        })
}

fn merges_columns(from: &FromClause<'_>) -> bool {
    from.joins
        .iter()
        .any(|join| join.using.is_some() || join.natural)
}

/// Spells `USING` and `NATURAL` joins out as `ON` joins: each merged column
/// becomes its join output expression (the coalesce of the joined columns,
/// which equals PostgreSQL's merged value for every join type), and `*`
/// lists the merged columns first, as the join's output does.
fn spell_out_merged_columns<'a>(
    select: &'a Select<'a>,
    storage: &'a Storage,
    txid: u32,
    arena: &'a Arena,
) -> Step<&'a Select<'a>> {
    let from = select.from.as_ref().expect("candidate has FROM");
    if from.joins.iter().any(|join| join.table.lateral) || from.base.lateral {
        return Err(Stop::Keep);
    }
    let scope = QueryScope::resolve_schema_with_join_conditions(storage, from, txid, arena)?;
    let rewrite = Rewrite {
        storage,
        txid,
        arena,
        top: Top::Merged { scope: &scope },
    };
    let mut joins = crate::mem::arena::ArenaList::new(arena);
    for (index, join) in from.joins.iter().enumerate() {
        let mut join = *join;
        if join.using.is_some() || join.natural {
            join.on = scope.join_on.get(index).copied().flatten();
            join.using = None;
            join.natural = false;
            match (join.kind, join.on) {
                (JoinKind::Inner, None) => join.kind = JoinKind::Cross,
                (JoinKind::Left | JoinKind::Right | JoinKind::Full, None) => {
                    join.on = Some(&Expr::Bool(true));
                }
                _ => {}
            }
        } else if let Some(condition) = join.on {
            join.on = Some(rewrite.expression(condition, None)?);
        }
        joins.push(join).map_err(|_| Stop::full())?;
    }
    let mut items = crate::mem::arena::ArenaList::new(arena);
    for entry in select.items {
        match entry {
            SelectItem::Wildcard(_) => {
                for column in 0..scope.star_columns() {
                    let entry = scope.star_entry(column);
                    items
                        .push(SelectItem::Expr {
                            expression: scope.star_expression(entry, arena)?,
                            alias: Some(scope.output_name(entry)),
                        })
                        .map_err(|_| Stop::full())?;
                }
            }
            SelectItem::TableWildcard(name) if scope.using_alias(name).is_some() => {
                for column in 0..scope.qualified_star_columns(name)? {
                    let entry = scope.qualified_star_entry(name, column)?;
                    items
                        .push(SelectItem::Expr {
                            expression: scope.star_expression(entry, arena)?,
                            alias: Some(scope.output_name(entry)),
                        })
                        .map_err(|_| Stop::full())?;
                }
            }
            SelectItem::TableWildcard(_) => items.push(*entry).map_err(|_| Stop::full())?,
            SelectItem::RecordStar(_) => return Err(Stop::Keep),
            SelectItem::Expr { expression, alias } => {
                let mapped = rewrite.expression(expression, None)?;
                let alias = if core::ptr::eq(mapped, *expression) {
                    *alias
                } else {
                    alias.or_else(|| {
                        let derived = super::super::exec::derived_name(expression);
                        (derived != "?column?").then_some(derived)
                    })
                };
                items
                    .push(SelectItem::Expr {
                        expression: mapped,
                        alias,
                    })
                    .map_err(|_| Stop::full())?;
            }
        }
    }
    let output_names = output_names(select.items, &scope, arena)?;
    Ok(&*arena
        .alloc(Select {
            items: arena
                .alloc_slice_copy(items.as_slice())
                .map_err(|_| Stop::full())?,
            distinct_on: rewrite.output_expressions(select.distinct_on, output_names)?,
            from: Some(FromClause {
                base: from.base,
                joins: arena
                    .alloc_slice_copy(joins.as_slice())
                    .map_err(|_| Stop::full())?,
            }),
            where_clause: select
                .where_clause
                .map(|condition| rewrite.expression(condition, None))
                .transpose()?,
            group_by: rewrite.expressions(select.group_by, None)?,
            having: select
                .having
                .map(|condition| rewrite.expression(condition, None))
                .transpose()?,
            order_by: rewrite.output_keys(select.order_by, output_names)?,
            ..*select
        })
        .map_err(|_| Stop::full())?)
}

/// A subquery PostgreSQL's `is_simple_subquery` accepts, restricted to the
/// shapes merged here.
fn simple_subquery<'a>(
    table: &TableRef<'a>,
    storage: &'a Storage,
    txid: u32,
) -> Option<&'a Select<'a>> {
    let body = table.subquery?;
    if body.set_body.is_some()
        || !body.with.is_empty()
        || !body.locking.is_empty()
        || body.distinct
        || !body.distinct_on.is_empty()
        || !body.group_by.is_empty()
        || !body.grouping_sets.is_empty()
        || body.having.is_some()
        || !body.order_by.is_empty()
        || body.limit.is_some()
        || body.offset.is_some()
        || table.sample.is_some()
        || table.cte.is_some()
        || table.inheritance != crate::sql::ast::RelationInheritance::Descendants
    {
        return None;
    }
    let from = body.from.as_ref()?;
    if core::iter::once(&from.base)
        .chain(from.joins.iter().map(|join| &join.table))
        .any(|table| table.join_members.is_some())
    {
        return None;
    }
    // A view's output column carries its collation as implicit; merging an
    // explicit `COLLATE` into the outer query would make it explicit there.
    if body.items.iter().any(|item| match item {
        SelectItem::Expr { expression, .. } => has_collate(expression),
        _ => false,
    }) {
        return None;
    }
    if from.joins.iter().any(|join| {
        join.using.is_some()
            || join.natural
            || !matches!(
                join.kind,
                JoinKind::Inner | JoinKind::Cross | JoinKind::Left
            )
    }) {
        return None;
    }
    if let Some(slot) = table.view_access {
        let view = storage.view(usize::from(slot));
        if view.security_barrier_for(txid) == crate::storage::ViewSecurityBarrier::Enabled {
            return None;
        }
        // The caller's column privileges on the view are checked against
        // the columns it reads from the materialized view source.
        let role = storage.current_role_slot(txid)?;
        let object = crate::storage::AccessObject {
            class: crate::storage::AccessClass::View,
            slot,
        };
        if !storage.has_object_privilege(object, role, crate::storage::PrivilegeSet::SELECT, txid) {
            return None;
        }
    }
    Some(body)
}

#[allow(clippy::too_many_lines)]
fn pull_up_one<'a>(
    outer_select: &'a Select<'a>,
    position: usize,
    storage: &'a Storage,
    txid: u32,
    arena: &'a Arena,
    outer: Option<&dyn ColumnLookup<'a>>,
) -> Step<&'a Select<'a>> {
    let from = outer_select.from.as_ref().expect("candidate has FROM");
    let table = item(from, position);
    let body = simple_subquery(table, storage, txid).ok_or(Stop::Keep)?;
    if merges_columns(from) {
        let spelled = spell_out_merged_columns(outer_select, storage, txid, arena)?;
        return pull_up_one(spelled, position, storage, txid, arena, outer);
    }
    // Inner subqueries first, as PostgreSQL pulls up bottom-up.
    let body = pull_up_subqueries(body, storage, txid, arena, None)?;
    let body_from = body.from.as_ref();
    let nullable = match body_from {
        Some(body_from) => nullable_position(from, position, body_from)?,
        None => fromless_position(from, position)?,
    };
    // Its WHERE could only become a join condition, evaluated per candidate
    // pair, while the subquery filters each of its rows once: a filtering
    // subquery under an outer join stays in place.
    if nullable && body.where_clause.is_some() {
        return Err(Stop::Keep);
    }
    let outer_scope =
        QueryScope::resolve_schema(storage, from, txid, arena, outer).map_err(|_| Stop::Keep)?;
    let inner_scope = body_from
        .map(|body_from| QueryScope::resolve_schema(storage, body_from, txid, arena, None))
        .transpose()
        .map_err(|_| Stop::Keep)?;
    let inner = inner_scope.as_ref();
    let alias = outer_scope.names[position];
    // Every exposed name stays unique after the merge.
    for index in 0..inner.map_or(0, |scope| scope.n) {
        let inner_scope = inner.expect("counted");
        let name = inner_scope.names[index];
        if name.is_empty()
            || inner_scope.names[..index].contains(&name)
            || (0..outer_scope.n).any(|other| other != position && outer_scope.names[other] == name)
        {
            return Err(Stop::Keep);
        }
    }
    for index in 0..outer_scope.n {
        if outer_scope.names[..index].contains(&outer_scope.names[index]) {
            return Err(Stop::Keep);
        }
    }
    let scoped_columns = inner.map(|scope| CatalogScopeCols {
        scope,
        outer: None,
        storage,
        txid,
        arena,
    });
    let fromless = super::super::exec::CatalogCols {
        definition: None,
        alias: None,
        output_aliases: &[],
        storage,
        txid,
        arena,
    };
    let inner_columns: &dyn super::super::exec::ColTypeResolver = match scoped_columns.as_ref() {
        Some(columns) => columns,
        None => &fromless,
    };
    if super::has_project_set(body.items, storage, txid) {
        return Err(Stop::Keep);
    }
    let definition = outer_scope.defs.get(position).ok_or(Stop::Keep)?;
    let width = definition.n_columns;
    let projected = arena
        .alloc_slice_with(width, |_| &Expr::Null as &'a Expr<'a>)
        .map_err(|_| Stop::full())?;
    let names = arena
        .alloc_slice_with(width, |_| "")
        .map_err(|_| Stop::full())?;
    for (index, name) in names.iter_mut().enumerate() {
        *name = outer_scope.defs.column_name(position, index);
    }
    let qualify = Rewrite {
        storage,
        txid,
        arena,
        top: Top::Inner {
            inner,
            outer: &outer_scope,
            position,
            lateral: table.lateral,
        },
    };
    let mut filled = 0usize;
    for entry in body.items {
        match entry {
            SelectItem::Expr { expression, .. } => {
                if super::expr_has_window(expression)
                    || super::expr_has_aggregate(expression, storage, txid, inner_columns)?
                    || super::predicate_safety::expression_properties(
                        expression,
                        inner_columns,
                        storage,
                        txid,
                        super::predicate_safety::AnalysisStage::RuntimeProjection,
                    )?
                    .volatile
                {
                    return Err(Stop::Keep);
                }
                let mapped = qualify.expression(expression, None)?;
                // PostgreSQL resolves an untyped literal output to text.
                let mapped = if matches!(mapped, Expr::Str(_) | Expr::Null) {
                    &*arena
                        .alloc(Expr::Cast {
                            source_position: SourcePosition::UNKNOWN,
                            operand: mapped,
                            type_name: "text",
                            type_mod: -1,
                            binding: CastBinding::Builtin,
                        })
                        .map_err(|_| Stop::full())?
                } else {
                    mapped
                };
                *projected.get_mut(filled).ok_or(Stop::Keep)? = mapped;
                filled += 1;
            }
            SelectItem::Wildcard(_) => {
                let inner_scope = inner.ok_or(Stop::Keep)?;
                for column in 0..inner_scope.star_columns() {
                    *projected.get_mut(filled).ok_or(Stop::Keep)? =
                        inner_column(inner_scope, inner_scope.star_entry(column), arena)?;
                    filled += 1;
                }
            }
            SelectItem::TableWildcard(name) => {
                let inner_scope = inner.ok_or(Stop::Keep)?;
                let count = inner_scope
                    .qualified_star_columns(name)
                    .map_err(|_| Stop::Keep)?;
                for column in 0..count {
                    let entry = inner_scope
                        .qualified_star_entry(name, column)
                        .map_err(|_| Stop::Keep)?;
                    *projected.get_mut(filled).ok_or(Stop::Keep)? =
                        inner_column(inner_scope, entry, arena)?;
                    filled += 1;
                }
            }
            SelectItem::RecordStar(_) => return Err(Stop::Keep),
        }
    }
    if filled != width {
        return Err(Stop::Keep);
    }
    let inner_where = body
        .where_clause
        .map(|condition| qualify.expression(condition, None))
        .transpose()?;
    let inner_from = body_from
        .map(|body_from| qualify.qualified_from(body_from))
        .transpose()?;
    let rewrite = Rewrite {
        storage,
        txid,
        arena,
        top: Top::Outer {
            scope: &outer_scope,
            inner,
            position,
            alias,
            names: &*names,
            projected: &*projected,
            nullable,
        },
    };
    let output_names = output_names(outer_select.items, &outer_scope, arena)?;
    let items = rewrite.items(outer_select.items, &outer_scope)?;
    let where_clause = outer_select
        .where_clause
        .map(|condition| rewrite.expression(condition, None))
        .transpose()?;
    let group_by = rewrite.expressions(outer_select.group_by, None)?;
    let having = outer_select
        .having
        .map(|condition| rewrite.expression(condition, None))
        .transpose()?;
    let order_by = rewrite.output_keys(outer_select.order_by, output_names)?;
    let distinct_on = rewrite.output_expressions(outer_select.distinct_on, output_names)?;

    // The merged FROM clause: the subquery's items replace it in place.
    // A FROM-less subquery adds no item: the inner join that brought it in,
    // or the one after it as the first item, becomes a restriction.
    let mut lifted = None;
    let mut joins = crate::mem::arena::ArenaList::new(arena);
    let mut base = from.base;
    let mut skipped_base = false;
    for index in 0..1 + from.joins.len() {
        if index == position
            && let None = inner_from
        {
            if index == 0 {
                skipped_base = true;
            } else if let Some(condition) = from.joins[index - 1].on {
                lifted =
                    super::and_where(lifted, Some(rewrite.expression(condition, None)?), arena)?;
            }
            continue;
        }
        if skipped_base && index == 1 {
            let join = &from.joins[0];
            base = rewrite.table(&join.table, true)?;
            if let Some(condition) = join.on {
                lifted =
                    super::and_where(lifted, Some(rewrite.expression(condition, None)?), arena)?;
            }
            continue;
        }
        if index == position
            && let Some(inner_from) = inner_from
        {
            if index == 0 {
                base = inner_from.base;
                for join in inner_from.joins {
                    joins.push(*join).map_err(|_| Stop::full())?;
                }
            } else {
                let join = &from.joins[index - 1];
                let condition = join
                    .on
                    .map(|condition| rewrite.expression(condition, None))
                    .transpose()?;
                let count = inner_from.joins.len();
                let first_on = if count == 0 { condition } else { None };
                joins
                    .push(Join {
                        table: inner_from.base,
                        kind: match join.kind {
                            JoinKind::Left | JoinKind::Right if count == 0 => join.kind,
                            // A comma item stays one: later `ON` conditions
                            // still cannot name it.
                            JoinKind::Comma if first_on.is_none() => JoinKind::Comma,
                            _ if first_on.is_some() => JoinKind::Inner,
                            _ => JoinKind::Cross,
                        },
                        on: first_on,
                        using: None,
                        natural: false,
                    })
                    .map_err(|_| Stop::full())?;
                for (inner_index, inner_join) in inner_from.joins.iter().enumerate() {
                    let mut inner_join = *inner_join;
                    if inner_index + 1 == count {
                        inner_join.on = super::and_where(inner_join.on, condition, arena)?;
                        if inner_join.on.is_some() {
                            inner_join.kind = JoinKind::Inner;
                        }
                    }
                    joins.push(inner_join).map_err(|_| Stop::full())?;
                }
            }
            continue;
        }
        let rewritten_table = rewrite.table(item(from, index), index > position)?;
        if index == 0 {
            base = rewritten_table;
        } else {
            let join = &from.joins[index - 1];
            joins
                .push(Join {
                    table: rewritten_table,
                    on: join
                        .on
                        .map(|condition| rewrite.expression(condition, None))
                        .transpose()?,
                    ..*join
                })
                .map_err(|_| Stop::full())?;
        }
    }
    let from_clause = if skipped_base && from.joins.is_empty() {
        None
    } else {
        Some(FromClause {
            base,
            joins: arena
                .alloc_slice_copy(joins.as_slice())
                .map_err(|_| Stop::full())?,
        })
    };
    let locking = if outer_select
        .locking
        .iter()
        .any(|clause| clause.of.contains(&alias))
    {
        // Locking the subquery locks the relations it reads.
        let mut clauses = crate::mem::arena::ArenaList::new(arena);
        for clause in outer_select.locking {
            let mut clause = *clause;
            if clause.of.contains(&alias) {
                let mut of = crate::mem::arena::ArenaList::new(arena);
                for name in clause.of {
                    if *name == alias {
                        let inner_scope = inner.ok_or(Stop::Keep)?;
                        for index in 0..inner_scope.n {
                            of.push(inner_scope.names[index])
                                .map_err(|_| Stop::full())?;
                        }
                    } else {
                        of.push(*name).map_err(|_| Stop::full())?;
                    }
                }
                clause.of = arena
                    .alloc_slice_copy(of.as_slice())
                    .map_err(|_| Stop::full())?;
            }
            clauses.push(clause).map_err(|_| Stop::full())?;
        }
        &*arena
            .alloc_slice_copy(clauses.as_slice())
            .map_err(|_| Stop::full())?
    } else {
        outer_select.locking
    };
    let merged = Select {
        items,
        locking,
        distinct_on,
        from: from_clause,
        where_clause: super::and_where(
            super::and_where(inner_where, lifted, arena)?,
            where_clause,
            arena,
        )?,
        group_by,
        having,
        order_by,
        ..*outer_select
    };
    Ok(&*arena.alloc(merged).map_err(|_| Stop::full())?)
}

/// Whether an outer join can null the subquery's rows at `position`, or
/// `Keep` when its FROM items cannot replace it in the left-deep join chain.
fn nullable_position(from: &FromClause<'_>, position: usize, body: &FromClause<'_>) -> Step<bool> {
    let entering = (position > 0).then(|| from.joins[position - 1].kind);
    let later = &from.joins[position..];
    if entering == Some(JoinKind::Full) || later.iter().any(|join| join.kind == JoinKind::Full) {
        return Err(Stop::Keep);
    }
    let inner_body = body.joins.iter().all(|join| {
        matches!(
            join.kind,
            JoinKind::Inner | JoinKind::Cross | JoinKind::Comma
        )
    });
    match entering {
        Some(JoinKind::Left | JoinKind::Right) if !body.joins.is_empty() => Err(Stop::Keep),
        Some(_) if !inner_body => Err(Stop::Keep),
        _ => Ok(entering == Some(JoinKind::Left)
            || later.iter().any(|join| join.kind == JoinKind::Right)),
    }
}

/// A FROM-less subquery contributes one row, filtered by its `WHERE`: it can
/// disappear only where no outer join depends on that row.
fn fromless_position(from: &FromClause<'_>, position: usize) -> Step<bool> {
    let entering = (position > 0).then(|| from.joins[position - 1].kind);
    let later = &from.joins[position..];
    let inner =
        |kind: JoinKind| matches!(kind, JoinKind::Inner | JoinKind::Cross | JoinKind::Comma);
    if !entering.is_none_or(inner)
        || later
            .iter()
            .any(|join| matches!(join.kind, JoinKind::Right | JoinKind::Full))
        || (position == 0 && later.first().is_some_and(|join| !inner(join.kind)))
    {
        return Err(Stop::Keep);
    }
    Ok(false)
}

/// Whether `expression` reads a column and is null whenever its columns
/// are: PostgreSQL substitutes such an output of a pulled-up subquery under
/// an outer join directly instead of through a placeholder.
fn nulled_with_columns(expression: &Expr<'_>) -> bool {
    fn strict(expression: &Expr<'_>, columns: &mut bool) -> bool {
        match expression {
            Expr::Column { .. } => {
                *columns = true;
                true
            }
            Expr::Int(_)
            | Expr::Float(_)
            | Expr::NumericLit(_)
            | Expr::Str(_)
            | Expr::BitLit(_)
            | Expr::Bool(_)
            | Expr::Param(_) => true,
            Expr::Binary {
                operator,
                left,
                right,
                ..
            } => {
                matches!(
                    operator,
                    crate::sql::ast::BinaryOp::Add
                        | crate::sql::ast::BinaryOp::Sub
                        | crate::sql::ast::BinaryOp::Mul
                        | crate::sql::ast::BinaryOp::Div
                        | crate::sql::ast::BinaryOp::Mod
                        | crate::sql::ast::BinaryOp::Eq
                        | crate::sql::ast::BinaryOp::NotEq
                        | crate::sql::ast::BinaryOp::Lt
                        | crate::sql::ast::BinaryOp::LtEq
                        | crate::sql::ast::BinaryOp::Gt
                        | crate::sql::ast::BinaryOp::GtEq
                        | crate::sql::ast::BinaryOp::Pow
                ) && strict(left, columns)
                    && strict(right, columns)
            }
            Expr::Unary {
                operator, operand, ..
            } => {
                matches!(
                    operator,
                    crate::sql::ast::UnaryOp::Neg
                        | crate::sql::ast::UnaryOp::Not
                        | crate::sql::ast::UnaryOp::AbsoluteValue
                        | crate::sql::ast::UnaryOp::SquareRoot
                        | crate::sql::ast::UnaryOp::CubeRoot
                ) && strict(operand, columns)
            }
            Expr::Cast { operand, .. } | Expr::Collate { operand, .. } => strict(operand, columns),
            _ => false,
        }
    }
    let mut columns = false;
    strict(expression, &mut columns) && columns
}

fn inner_column<'a>(
    scope: &QueryScope<'a>,
    entry: ResolvedColumn,
    arena: &'a Arena,
) -> Step<&'a Expr<'a>> {
    match entry {
        ResolvedColumn::Table(table, column) => Ok(&*arena
            .alloc(Expr::Column {
                qualifier: Some(scope.names[table]),
                name: scope.defs.column_name(table, column),
            })
            .map_err(|_| Stop::full())?),
        ResolvedColumn::Merged(_) => Err(Stop::Keep),
    }
}

/// The names of the outer query's output columns, which `ORDER BY` and
/// `DISTINCT ON` may name instead of an input column.
fn output_names<'a>(
    items: &'a [SelectItem<'a>],
    scope: &QueryScope<'a>,
    arena: &'a Arena,
) -> Step<&'a [&'a str]> {
    let mut names = crate::mem::arena::ArenaList::new(arena);
    for entry in items {
        match entry {
            SelectItem::Expr { expression, alias } => names
                .push(alias.unwrap_or_else(|| super::super::exec::derived_name(expression)))
                .map_err(|_| Stop::full())?,
            SelectItem::Wildcard(_) => {
                for column in 0..scope.star_columns() {
                    names
                        .push(scope.output_name(scope.star_entry(column)))
                        .map_err(|_| Stop::full())?;
                }
            }
            SelectItem::TableWildcard(name) => {
                let count = scope.qualified_star_columns(name).map_err(|_| Stop::Keep)?;
                for column in 0..count {
                    let entry = scope
                        .qualified_star_entry(name, column)
                        .map_err(|_| Stop::Keep)?;
                    names
                        .push(scope.output_name(entry))
                        .map_err(|_| Stop::full())?;
                }
            }
            SelectItem::RecordStar(_) => return Err(Stop::Keep),
        }
    }
    Ok(arena
        .alloc_slice_copy(names.as_slice())
        .map_err(|_| Stop::full())?)
}

/// Where a reference that no nested query binds is resolved.
#[derive(Clone, Copy)]
enum Top<'r, 'a> {
    /// The outer query: references to the pulled-up item become its output
    /// expressions; any other reference must keep its binding.
    Outer {
        scope: &'r QueryScope<'a>,
        inner: Option<&'r QueryScope<'a>>,
        position: usize,
        alias: &'a str,
        names: &'a [&'a str],
        projected: &'a [&'a Expr<'a>],
        /// An outer join can null the item's rows: an output expression is
        /// substituted only when it is then null too, as PostgreSQL needs
        /// no placeholder for it.
        nullable: bool,
    },
    /// A query with `USING` or `NATURAL` joins: a merged column becomes its
    /// join output expression.
    Merged { scope: &'r QueryScope<'a> },
    /// The subquery body: its own references are qualified so they cannot
    /// bind to an outer item after the merge.
    Inner {
        inner: Option<&'r QueryScope<'a>>,
        outer: &'r QueryScope<'a>,
        position: usize,
        lateral: bool,
    },
}

/// One nested query's scope, innermost first.
struct Level<'l, 'a> {
    scope: Option<&'l QueryScope<'a>>,
    parent: Option<&'l Level<'l, 'a>>,
}

struct Rewrite<'r, 'a> {
    storage: &'a Storage,
    txid: u32,
    arena: &'a Arena,
    top: Top<'r, 'a>,
}

/// Whether `name` would bind in `scope` as a column (or fail as ambiguous)
/// rather than be absent.
fn binds_column(scope: Option<&QueryScope<'_>>, name: &str) -> bool {
    let Some(scope) = scope else {
        return false;
    };
    match scope.find_column(None, name) {
        Ok(_) => true,
        Err(error) => error.sqlstate != sqlstate::UNDEFINED_COLUMN,
    }
}

fn names_relation(scope: Option<&QueryScope<'_>>, name: &str) -> bool {
    scope.is_some_and(|scope| {
        scope.names[..scope.n].contains(&name) || scope.entry_index(name).is_ok()
    })
}

impl<'a> Rewrite<'_, 'a> {
    fn claimed(level: Option<&Level<'_, 'a>>, qualifier: Option<&str>, name: &str) -> bool {
        let mut current = level;
        while let Some(level) = current {
            if let Some(scope) = level.scope {
                let bound = match qualifier {
                    Some(qualifier) => {
                        scope.entry_index(qualifier).is_ok()
                            || scope.using_alias(qualifier).is_some()
                            || scope.names[..scope.n].contains(&qualifier)
                    }
                    None => binds_column(Some(scope), name) || names_relation(Some(scope), name),
                };
                if bound {
                    return true;
                }
            }
            current = level.parent;
        }
        false
    }

    fn column(&self, qualifier: Option<&'a str>, name: &'a str) -> Step<Option<&'a Expr<'a>>> {
        match self.top {
            Top::Merged { scope } => match qualifier {
                Some(qualifier) if scope.using_alias(qualifier).is_some() => {
                    let entry = scope.find_column(Some(qualifier), name)?;
                    Ok(Some(scope.star_expression(entry, self.arena)?))
                }
                Some(_) => Ok(None),
                None => match scope.find_column(None, name) {
                    Ok(entry @ ResolvedColumn::Merged(_)) => {
                        Ok(Some(scope.star_expression(entry, self.arena)?))
                    }
                    _ => Ok(None),
                },
            },
            Top::Outer {
                scope,
                inner,
                position,
                alias,
                names,
                ..
            } => {
                if let Some(qualifier) = qualifier {
                    if qualifier == alias {
                        let index = names
                            .iter()
                            .position(|known| *known == name)
                            .ok_or(Stop::Keep)?;
                        return Ok(Some(self.projected(index)?));
                    }
                    if names_relation(Some(scope), qualifier) {
                        return Ok(None);
                    }
                    if names_relation(inner, qualifier) {
                        return Err(Stop::Keep);
                    }
                    return Ok(None);
                }
                if names_relation(Some(scope), name) || names_relation(inner, name) {
                    return Err(Stop::Keep);
                }
                match scope.find_column(None, name) {
                    Ok(ResolvedColumn::Table(table, column)) if table == position => {
                        Ok(Some(self.projected(column)?))
                    }
                    Ok(_) => {
                        if binds_column(inner, name) {
                            Err(Stop::Keep)
                        } else {
                            Ok(None)
                        }
                    }
                    Err(error) if error.sqlstate == sqlstate::UNDEFINED_COLUMN => {
                        if binds_column(inner, name) {
                            Err(Stop::Keep)
                        } else {
                            Ok(None)
                        }
                    }
                    Err(_) => Err(Stop::Keep),
                }
            }
            Top::Inner {
                inner,
                outer,
                position,
                lateral,
            } => {
                if let Some(qualifier) = qualifier {
                    if let Some(inner) = inner
                        && names_relation(Some(inner), qualifier)
                    {
                        inner
                            .find_column(Some(qualifier), name)
                            .map_err(|_| Stop::Keep)?;
                        return Ok(None);
                    }
                    if let Ok(table) = outer.entry_index(qualifier) {
                        return if lateral && table < position {
                            Ok(None)
                        } else {
                            Err(Stop::Keep)
                        };
                    }
                    if names_relation(Some(outer), qualifier) {
                        return Err(Stop::Keep);
                    }
                    return Ok(None);
                }
                if names_relation(inner, name) || names_relation(Some(outer), name) {
                    return Err(Stop::Keep);
                }
                let resolved = match inner {
                    Some(inner) => inner.find_column(None, name).map(|entry| (inner, entry)),
                    None => Err(sql_err!(
                        sqlstate::UNDEFINED_COLUMN,
                        "column does not exist"
                    )),
                };
                match resolved {
                    Ok((inner, entry)) => Ok(Some(inner_column(inner, entry, self.arena)?)),
                    Err(error) if error.sqlstate == sqlstate::UNDEFINED_COLUMN => {
                        match outer.find_column(None, name) {
                            Ok(ResolvedColumn::Table(table, column))
                                if lateral && table < position =>
                            {
                                Ok(Some(
                                    &*self
                                        .arena
                                        .alloc(Expr::Column {
                                            qualifier: Some(outer.names[table]),
                                            name: outer.defs.column_name(table, column),
                                        })
                                        .map_err(|_| Stop::full())?,
                                ))
                            }
                            Ok(_) => Err(Stop::Keep),
                            Err(error) if error.sqlstate == sqlstate::UNDEFINED_COLUMN => Ok(None),
                            Err(_) => Err(Stop::Keep),
                        }
                    }
                    Err(_) => Err(Stop::Keep),
                }
            }
        }
    }

    /// A copy of the pulled-up item's output expression `index`.
    fn projected(&self, index: usize) -> Step<&'a Expr<'a>> {
        let Top::Outer {
            projected,
            nullable,
            ..
        } = self.top
        else {
            unreachable!("outer substitution");
        };
        let expression = projected[index];
        if nullable && !nulled_with_columns(expression) {
            return Err(Stop::Keep);
        }
        self.copy(expression)
    }

    fn copy(&self, expression: &'a Expr<'a>) -> Step<&'a Expr<'a>> {
        Ok(crate::sql::ast::duplicate_expression(
            expression,
            self.arena,
            &[],
        )?)
    }

    /// Any binding of `name` the merge could change.
    fn routine_parameter(&self, name: &str) -> bool {
        match self.top {
            Top::Outer { inner, .. } => binds_column(inner, name),
            Top::Inner { outer, .. } => binds_column(Some(outer), name),
            Top::Merged { .. } => false,
        }
    }

    fn whole_row(&self, name: &str) -> bool {
        match self.top {
            Top::Outer { alias, inner, .. } => name == alias || names_relation(inner, name),
            Top::Inner { .. } => true,
            Top::Merged { scope } => scope.using_alias(name).is_some(),
        }
    }

    fn expression(
        &self,
        expression: &'a Expr<'a>,
        level: Option<&Level<'_, 'a>>,
    ) -> Step<&'a Expr<'a>> {
        rewrite_expression(expression, self.arena, &mut |node| match node {
            Expr::Column { qualifier, name } => {
                if Self::claimed(level, *qualifier, name) {
                    Ok(None)
                } else {
                    self.column(*qualifier, name)
                }
            }
            Expr::WholeRow(name) => {
                if Self::claimed(level, Some(name), name) || !self.whole_row(name) {
                    Ok(None)
                } else {
                    Err(Stop::Keep)
                }
            }
            Expr::SchemaColumn { table, .. } => {
                if Self::claimed(level, Some(table), table) || !self.whole_row(table) {
                    Ok(None)
                } else {
                    Err(Stop::Keep)
                }
            }
            Expr::RoutineParam {
                qualifier: None,
                name,
                ..
            } if self.routine_parameter(name) => Err(Stop::Keep),
            Expr::RecursiveState { .. } => Err(Stop::Keep),
            Expr::Subquery(select) => Ok(Some(self.sublink(node, select, level)?)),
            Expr::Exists(select) => Ok(Some(self.sublink(node, select, level)?)),
            Expr::ArraySubquery(select) => Ok(Some(self.sublink(node, select, level)?)),
            Expr::RowSubquery { select, .. } => Ok(Some(self.sublink(node, select, level)?)),
            Expr::InSubquery { select, .. } | Expr::QuantifiedSubquery { select, .. } => {
                Ok(Some(self.sublink(node, select, level)?))
            }
            _ => Ok(None),
        })
    }

    fn sublink(
        &self,
        node: &'a Expr<'a>,
        select: &'a Select<'a>,
        level: Option<&Level<'_, 'a>>,
    ) -> Step<&'a Expr<'a>> {
        let mapped = self.select(select, level)?;
        let operand = match node {
            Expr::InSubquery { operand, .. } | Expr::QuantifiedSubquery { operand, .. } => {
                Some(self.expression(operand, level)?)
            }
            _ => None,
        };
        let operand_changed = match (node, operand) {
            (
                Expr::InSubquery { operand, .. } | Expr::QuantifiedSubquery { operand, .. },
                Some(mapped),
            ) => !core::ptr::eq(*operand, mapped),
            _ => false,
        };
        if core::ptr::eq(mapped, select) && !operand_changed {
            return Ok(node);
        }
        let rebuilt = match *node {
            Expr::Subquery(_) => Expr::Subquery(mapped),
            Expr::Exists(_) => Expr::Exists(mapped),
            Expr::ArraySubquery(_) => Expr::ArraySubquery(mapped),
            Expr::RowSubquery { arity, .. } => Expr::RowSubquery {
                select: mapped,
                arity,
            },
            Expr::InSubquery { negated, .. } => Expr::InSubquery {
                operand: operand.expect("IN operand"),
                select: mapped,
                negated,
            },
            Expr::QuantifiedSubquery { operator, all, .. } => Expr::QuantifiedSubquery {
                operand: operand.expect("quantified operand"),
                operator,
                select: mapped,
                all,
            },
            _ => unreachable!("sublink node"),
        };
        Ok(&*self.arena.alloc(rebuilt).map_err(|_| Stop::full())?)
    }

    fn expressions(
        &self,
        expressions: &'a [&'a Expr<'a>],
        level: Option<&Level<'_, 'a>>,
    ) -> Step<&'a [&'a Expr<'a>]> {
        let mut copied: Option<&'a mut [&'a Expr<'a>]> = None;
        for (index, expression) in expressions.iter().enumerate() {
            let mapped = self.expression(expression, level)?;
            if !core::ptr::eq(mapped, *expression) {
                let slice = match copied.take() {
                    Some(slice) => slice,
                    None => self
                        .arena
                        .alloc_slice_copy(expressions)
                        .map_err(|_| Stop::full())?,
                };
                slice[index] = mapped;
                copied = Some(slice);
            }
        }
        Ok(copied.map_or(expressions, |slice| &*slice))
    }

    fn ordering(
        &self,
        keys: &'a [OrderBy<'a>],
        level: Option<&Level<'_, 'a>>,
    ) -> Step<&'a [OrderBy<'a>]> {
        let mut copied: Option<&'a mut [OrderBy<'a>]> = None;
        for (index, key) in keys.iter().enumerate() {
            let mapped = self.expression(key.expression, level)?;
            if !core::ptr::eq(mapped, key.expression) {
                let slice = match copied.take() {
                    Some(slice) => slice,
                    None => self
                        .arena
                        .alloc_slice_copy(keys)
                        .map_err(|_| Stop::full())?,
                };
                slice[index].expression = mapped;
                copied = Some(slice);
            }
        }
        Ok(copied.map_or(keys, |slice| &*slice))
    }

    /// `ORDER BY` keys: a bare name of an output column names that column.
    fn output_keys(&self, keys: &'a [OrderBy<'a>], output: &[&str]) -> Step<&'a [OrderBy<'a>]> {
        let mut copied: Option<&'a mut [OrderBy<'a>]> = None;
        for (index, key) in keys.iter().enumerate() {
            if matches!(key.expression, Expr::Column { qualifier: None, name } if output.contains(name))
            {
                continue;
            }
            let mapped = self.expression(key.expression, None)?;
            if !core::ptr::eq(mapped, key.expression) {
                let slice = match copied.take() {
                    Some(slice) => slice,
                    None => self
                        .arena
                        .alloc_slice_copy(keys)
                        .map_err(|_| Stop::full())?,
                };
                slice[index].expression = mapped;
                copied = Some(slice);
            }
        }
        Ok(copied.map_or(keys, |slice| &*slice))
    }

    fn output_expressions(
        &self,
        expressions: &'a [&'a Expr<'a>],
        output: &[&str],
    ) -> Step<&'a [&'a Expr<'a>]> {
        let mut copied: Option<&'a mut [&'a Expr<'a>]> = None;
        for (index, expression) in expressions.iter().enumerate() {
            if matches!(expression, Expr::Column { qualifier: None, name } if output.contains(name))
            {
                continue;
            }
            let mapped = self.expression(expression, None)?;
            if !core::ptr::eq(mapped, *expression) {
                let slice = match copied.take() {
                    Some(slice) => slice,
                    None => self
                        .arena
                        .alloc_slice_copy(expressions)
                        .map_err(|_| Stop::full())?,
                };
                slice[index] = mapped;
                copied = Some(slice);
            }
        }
        Ok(copied.map_or(expressions, |slice| &*slice))
    }

    /// The outer select list: stars over the pulled-up item expand to its
    /// output expressions, and each rewritten item keeps its output name.
    fn items(
        &self,
        items: &'a [SelectItem<'a>],
        scope: &QueryScope<'a>,
    ) -> Step<&'a [SelectItem<'a>]> {
        let Top::Outer {
            position,
            names,
            alias,
            ..
        } = self.top
        else {
            unreachable!("outer select list");
        };
        let mut rewritten = crate::mem::arena::ArenaList::new(self.arena);
        let mut changed = false;
        let push_pulled = |rewritten: &mut crate::mem::arena::ArenaList<'a, SelectItem<'a>>| {
            for (index, name) in names.iter().enumerate() {
                rewritten
                    .push(SelectItem::Expr {
                        expression: self.projected(index)?,
                        alias: Some(name),
                    })
                    .map_err(|_| Stop::full())?;
            }
            Ok::<(), Stop>(())
        };
        for entry in items {
            match entry {
                SelectItem::Expr { expression, alias } => {
                    let mapped = self.expression(expression, None)?;
                    let alias = if core::ptr::eq(mapped, *expression) {
                        *alias
                    } else {
                        changed = true;
                        alias.or_else(|| {
                            let derived = super::super::exec::derived_name(expression);
                            (derived != "?column?").then_some(derived)
                        })
                    };
                    rewritten
                        .push(SelectItem::Expr {
                            expression: mapped,
                            alias,
                        })
                        .map_err(|_| Stop::full())?;
                }
                SelectItem::Wildcard(_) => {
                    changed = true;
                    for table in 0..scope.n {
                        if table == position {
                            push_pulled(&mut rewritten)?;
                        } else {
                            rewritten
                                .push(SelectItem::TableWildcard(scope.names[table]))
                                .map_err(|_| Stop::full())?;
                        }
                    }
                }
                SelectItem::TableWildcard(name) if *name == alias => {
                    changed = true;
                    push_pulled(&mut rewritten)?;
                }
                SelectItem::TableWildcard(name) => {
                    if self.whole_row(name) {
                        return Err(Stop::Keep);
                    }
                    rewritten.push(*entry).map_err(|_| Stop::full())?;
                }
                SelectItem::RecordStar(_) => return Err(Stop::Keep),
            }
        }
        if !changed {
            return Ok(items);
        }
        Ok(self
            .arena
            .alloc_slice_copy(rewritten.as_slice())
            .map_err(|_| Stop::full())?)
    }

    /// Another FROM item of the outer query. Only a lateral one can read the
    /// pulled-up item; a non-lateral subquery cannot see its siblings.
    fn table(&self, table: &TableRef<'a>, after: bool) -> Step<TableRef<'a>> {
        let mut table = *table;
        if !after {
            return Ok(table);
        }
        if let Some(arguments) = table.func_args {
            table.func_args = Some(self.expressions(arguments, None)?);
        }
        if table.rows_from.is_some() {
            return Err(Stop::Keep);
        }
        if table.lateral
            && let Some(body) = table.subquery
        {
            table.subquery = Some(self.select(body, None)?);
        }
        Ok(table)
    }

    /// The subquery's own FROM items, with their conditions and lateral
    /// references qualified.
    fn qualified_from(&self, from: &'a FromClause<'a>) -> Step<FromClause<'a>> {
        let base = self.inner_table(&from.base)?;
        let mut joins = crate::mem::arena::ArenaList::new(self.arena);
        for join in from.joins {
            joins
                .push(Join {
                    table: self.inner_table(&join.table)?,
                    on: join
                        .on
                        .map(|condition| self.expression(condition, None))
                        .transpose()?,
                    ..*join
                })
                .map_err(|_| Stop::full())?;
        }
        Ok(FromClause {
            base,
            joins: self
                .arena
                .alloc_slice_copy(joins.as_slice())
                .map_err(|_| Stop::full())?,
        })
    }

    fn inner_table(&self, table: &TableRef<'a>) -> Step<TableRef<'a>> {
        let mut table = *table;
        if table.lateral || table.rows_from.is_some() {
            return Err(Stop::Keep);
        }
        if let Some(arguments) = table.func_args {
            table.func_args = Some(self.expressions(arguments, None)?);
        }
        Ok(table)
    }

    /// A nested query: its own FROM items bind first.
    fn select(
        &self,
        select: &'a Select<'a>,
        level: Option<&Level<'_, 'a>>,
    ) -> Step<&'a Select<'a>> {
        if !select.with.is_empty() {
            return Err(Stop::Keep);
        }
        if let Some(tree) = select.set_body {
            let body = self.set_tree(tree, level)?;
            let limit = select
                .limit
                .map(|value| self.expression(value, level))
                .transpose()?;
            let offset = select
                .offset
                .map(|value| self.expression(value, level))
                .transpose()?;
            if core::ptr::eq(body, tree)
                && option_same(limit, select.limit)
                && option_same(offset, select.offset)
            {
                return Ok(select);
            }
            return Ok(&*self
                .arena
                .alloc(Select {
                    set_body: Some(body),
                    limit,
                    offset,
                    ..*select
                })
                .map_err(|_| Stop::full())?);
        }
        let scope = match select.from.as_ref() {
            Some(from) => Some(
                QueryScope::resolve_schema(self.storage, from, self.txid, self.arena, None)
                    .map_err(|_| Stop::Keep)?,
            ),
            None => None,
        };
        let nested = Level {
            scope: scope.as_ref(),
            parent: level,
        };
        let nested = Some(&nested);
        let mut from = select.from;
        if let Some(clause) = from.as_mut() {
            clause.base = self.nested_table(&clause.base, level, nested)?;
            let mut joins = crate::mem::arena::ArenaList::new(self.arena);
            for join in clause.joins {
                joins
                    .push(Join {
                        table: self.nested_table(&join.table, level, nested)?,
                        on: join
                            .on
                            .map(|condition| self.expression(condition, nested))
                            .transpose()?,
                        ..*join
                    })
                    .map_err(|_| Stop::full())?;
            }
            clause.joins = self
                .arena
                .alloc_slice_copy(joins.as_slice())
                .map_err(|_| Stop::full())?;
        }
        let mut items = crate::mem::arena::ArenaList::new(self.arena);
        for entry in select.items {
            items
                .push(match entry {
                    SelectItem::Expr { expression, alias } => SelectItem::Expr {
                        expression: self.expression(expression, nested)?,
                        alias: *alias,
                    },
                    SelectItem::RecordStar(expression) => {
                        SelectItem::RecordStar(self.expression(expression, nested)?)
                    }
                    other => *other,
                })
                .map_err(|_| Stop::full())?;
        }
        let rewritten = Select {
            items: self
                .arena
                .alloc_slice_copy(items.as_slice())
                .map_err(|_| Stop::full())?,
            distinct_on: self.expressions(select.distinct_on, nested)?,
            from,
            where_clause: select
                .where_clause
                .map(|condition| self.expression(condition, nested))
                .transpose()?,
            group_by: self.expressions(select.group_by, nested)?,
            having: select
                .having
                .map(|condition| self.expression(condition, nested))
                .transpose()?,
            order_by: self.ordering(select.order_by, nested)?,
            limit: select
                .limit
                .map(|value| self.expression(value, level))
                .transpose()?,
            offset: select
                .offset
                .map(|value| self.expression(value, level))
                .transpose()?,
            ..*select
        };
        if rewritten == *select {
            return Ok(select);
        }
        Ok(&*self.arena.alloc(rewritten).map_err(|_| Stop::full())?)
    }

    fn nested_table(
        &self,
        table: &TableRef<'a>,
        level: Option<&Level<'_, 'a>>,
        nested: Option<&Level<'_, 'a>>,
    ) -> Step<TableRef<'a>> {
        let mut table = *table;
        if table.rows_from.is_some() || (table.lateral && table.subquery.is_some()) {
            // The lateral item's left siblings are only part of the nested
            // scope; binding exactly would need a scope per position.
            return Err(Stop::Keep);
        }
        if let Some(arguments) = table.func_args {
            table.func_args = Some(self.expressions(arguments, nested)?);
        }
        if let Some(body) = table.subquery {
            table.subquery = Some(self.select(body, level)?);
        }
        Ok(table)
    }

    fn set_tree(
        &self,
        tree: &'a SetTree<'a>,
        level: Option<&Level<'_, 'a>>,
    ) -> Step<&'a SetTree<'a>> {
        let rewritten = match *tree {
            SetTree::Select(select) => {
                let mapped = self.select(select, level)?;
                if core::ptr::eq(mapped, select) {
                    return Ok(tree);
                }
                SetTree::Select(mapped)
            }
            SetTree::Op {
                syntax,
                operator,
                all,
                left,
                right,
            } => {
                let mapped_left = self.set_tree(left, level)?;
                let mapped_right = self.set_tree(right, level)?;
                if core::ptr::eq(mapped_left, left) && core::ptr::eq(mapped_right, right) {
                    return Ok(tree);
                }
                SetTree::Op {
                    syntax,
                    operator,
                    all,
                    left: mapped_left,
                    right: mapped_right,
                }
            }
        };
        Ok(&*self.arena.alloc(rewritten).map_err(|_| Stop::full())?)
    }
}

fn option_same(left: Option<&Expr<'_>>, right: Option<&Expr<'_>>) -> bool {
    match (left, right) {
        (None, None) => true,
        (Some(left), Some(right)) => core::ptr::eq(left, right),
        _ => false,
    }
}

fn has_collate(expression: &Expr<'_>) -> bool {
    if matches!(expression, Expr::Collate { .. }) {
        return true;
    }
    let mut found = false;
    let _ = super::walk_children(expression, &mut |child| {
        found |= has_collate(child);
        Ok::<(), SqlError>(())
    });
    found
}

/// Whether a subquery computes an output beyond naming a column.
fn computes_outputs(body: &Select<'_>) -> bool {
    body.items.iter().any(|item| match item {
        SelectItem::Expr { expression, .. } => {
            !matches!(expression, Expr::Column { .. } | Expr::SchemaColumn { .. })
        }
        _ => false,
    })
}
