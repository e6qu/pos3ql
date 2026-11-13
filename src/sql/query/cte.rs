//! `WITH` expansion and the AST substitution it rests on.
//!
//! An ordinary CTE is either inlined or materialized under its typed PostgreSQL
//! evaluation contract; a recursive one is materialized by fixpoint iteration.
//! Views expand the same way, from their stored text. Both rest on one thing: a
//! substituting walk that rebuilds a statement's expressions and FROM items in
//! the arena with each reference replaced, which is the rest of this module.

use crate::mem::arena::{Arena, ArenaList};
use crate::sql::ast::{
    Collation, Cte, CteCycleMark, CteMaterialization, CteSearchOrder, Delete, Expr, FromClause,
    GroupingSetQuantifier, Insert, Join, JoinKind, MaterializedCte, Merge, MergeSourceAction,
    MergeTargetAction, MergeWhen, OnConflict, OnConflictTarget, OrderBy, Returning, Select,
    SelectItem, SetOp, SetQuery, SetTree, Stmt, TableRef, Update,
};
use crate::sql::eval::{SequenceAccess, SqlError, sqlstate};
use crate::sql::exec::MAX_PROJ;
use crate::sql::types::{ArrElem, ColDesc, ColType, Datum, RecordField};
use crate::sql_err;
use crate::storage::Storage;

use super::setops::{describe_set_body, external_set_body_into, materialize_set_body};
use super::{and_where, arena_full, check_timeout};

/// Expands a statement's `WITH` list (and any view reference) for the
/// describe path, which needs the shape but not the rows.
pub fn expand_ctes<'a>(
    sel: &'a Select<'a>,
    storage: &Storage,
    txid: u32,
    arena: &'a Arena,
) -> Result<&'a Select<'a>, SqlError> {
    expand_ctes_with_path(
        sel,
        storage,
        txid,
        None,
        None,
        0,
        ViewReader::Session,
        arena,
    )
}

/// Expands a view body under the search path captured when that view was
/// created, qualifying its base relations before the ordinary describe path
/// resolves the resulting query.
pub fn expand_ctes_under<'a>(
    sel: &'a Select<'a>,
    storage: &'a Storage,
    txid: u32,
    path: crate::storage::PathContext,
    arena: &'a Arena,
) -> Result<&'a Select<'a>, SqlError> {
    expand_ctes_with_path(
        sel,
        storage,
        txid,
        Some(path),
        None,
        0,
        ViewReader::Session,
        arena,
    )
}

pub(crate) fn expand_stored_query<'a>(
    select: &'a Select<'a>,
    storage: &Storage,
    txid: u32,
    path: crate::storage::PathContext,
    dependencies: crate::storage::StoredQueryDependencyView<'_>,
    arena: &'a Arena,
) -> Result<&'a Select<'a>, SqlError> {
    expand_ctes_with_path(
        select,
        storage,
        txid,
        Some(path),
        Some(dependencies),
        0,
        ViewReader::Session,
        arena,
    )
}

/// Execution counterpart to [`expand_stored_query`]. Stored dependencies and
/// the creator's path remain attached while CTEs are materialized, so a
/// volatile or explicitly materialized CTE cannot be inlined before its
/// catalog identities are rebound.
#[allow(clippy::too_many_arguments)]
pub(crate) fn expand_stored_query_exec<'a>(
    select: &'a Select<'a>,
    storage: &Storage,
    txid: u32,
    path: crate::storage::PathContext,
    dependencies: crate::storage::StoredQueryDependencyView<'_>,
    arena: &'a Arena,
    params: &[Datum<'a>],
    sequences: Option<&dyn SequenceAccess>,
) -> Result<&'a Select<'a>, SqlError> {
    with_exec_context(
        select.with,
        storage,
        txid,
        arena,
        ExecutionSubst::materializing(params, sequences),
        &[],
        &[],
        &[],
        &[],
        0,
        Some(path),
        Some(dependencies),
        ViewReader::Session,
        None,
        |name| body_references(select, name),
        |context| subst_select_body(select, context, arena),
    )
}

pub(crate) fn expand_stored_expression<'a>(
    expression: &'a Expr<'a>,
    storage: &Storage,
    txid: u32,
    dependencies: crate::storage::StoredQueryDependencyView<'_>,
    arena: &'a Arena,
) -> Result<&'a Expr<'a>, SqlError> {
    subst_expr(
        expression,
        Subst {
            ctes: &[],
            materialized: &[],
            storage,
            txid,
            depth: 0,
            path: Some(storage.path()),
            dependencies: Some(dependencies),
            authorization_role: ViewReader::Session,
            qualifier: None,
            rule_transition: None,
            recursive_state: None,
            execution: None,
        },
        arena,
    )
}

/// The relation a stored query's FROM item names: its captured dependency,
/// else the name under `path` (or the session's path). The second value is
/// the captured dependency's relation alone.
pub(super) fn stored_relation_binding(
    t: &TableRef<'_>,
    storage: &Storage,
    txid: u32,
    path: Option<crate::storage::PathContext>,
    dependencies: Option<crate::storage::StoredQueryDependencyView<'_>>,
) -> Result<
    (
        Option<crate::storage::ResolvedRelation>,
        Option<crate::storage::ResolvedRelation>,
    ),
    SqlError,
> {
    let captured = dependencies
        .and_then(|dependencies| {
            dependencies.entries().iter().find(|dependency| {
                matches!(
                    dependency.class,
                    crate::storage::DependencyClass::Table | crate::storage::DependencyClass::View
                ) && dependency.referenced_schema.as_str() == t.schema.unwrap_or("")
                    && dependency.referenced_name.as_str() == t.table
            })
        })
        .map(|dependency| storage.resolve_stored_query_relation(dependency, txid))
        .transpose()?;
    let resolved = captured.or_else(|| match path {
        Some(p) => storage.resolve_relation_under(&p, t.schema, t.table, txid),
        None => storage.resolve_relation(t.schema, t.table, txid),
    });
    Ok((resolved, captured))
}

/// [`expand_stored_expression`] under a view creator's `path`.
pub(super) fn expand_stored_expression_under<'a>(
    expression: &'a Expr<'a>,
    storage: &Storage,
    txid: u32,
    path: crate::storage::PathContext,
    dependencies: crate::storage::StoredQueryDependencyView<'_>,
    arena: &'a Arena,
) -> Result<&'a Expr<'a>, SqlError> {
    subst_expr(
        expression,
        Subst {
            ctes: &[],
            materialized: &[],
            storage,
            txid,
            depth: 0,
            path: Some(path),
            dependencies: Some(dependencies),
            authorization_role: ViewReader::Session,
            qualifier: None,
            rule_transition: None,
            recursive_state: None,
            execution: None,
        },
        arena,
    )
}

fn expression_contains_with(expression: &Expr<'_>) -> bool {
    let nested = match expression {
        Expr::Subquery(select)
        | Expr::Exists(select)
        | Expr::ArraySubquery(select)
        | Expr::RowSubquery { select, .. } => select_contains_with(select),
        Expr::InSubquery { select, .. } | Expr::QuantifiedSubquery { select, .. } => {
            select_contains_with(select)
        }
        _ => false,
    };
    if nested {
        return true;
    }
    let mut found = false;
    super::walk_children(expression, &mut |child| {
        found |= expression_contains_with(child);
        Ok(())
    })
    .expect("WITH discovery cannot fail");
    found
}

fn table_contains_with(table: &TableRef<'_>) -> bool {
    table.subquery.is_some_and(select_contains_with)
        || table.func_args.is_some_and(|arguments| {
            arguments
                .iter()
                .any(|argument| expression_contains_with(argument))
        })
        || table
            .rows_from
            .is_some_and(|tables| tables.iter().any(table_contains_with))
        || table.sample.is_some_and(|sample| {
            expression_contains_with(sample.percentage)
                || sample.repeatable.is_some_and(expression_contains_with)
        })
}

fn set_tree_contains_with(tree: &SetTree<'_>) -> bool {
    match tree {
        SetTree::Select(select) => select_contains_with(select),
        SetTree::Op { left, right, .. } => {
            set_tree_contains_with(left) || set_tree_contains_with(right)
        }
    }
}

// A query without an outer WITH may still contain one in a derived table,
// expression subquery, or set-operation branch. Only skip a complete rewrite
// when every query level is free of WITH bindings.
fn select_contains_with(select: &Select<'_>) -> bool {
    !select.with.is_empty()
        || select.items.iter().any(|item| match item {
            SelectItem::Expr { expression, .. } | SelectItem::RecordStar(expression) => {
                expression_contains_with(expression)
            }
            SelectItem::Wildcard(_) | SelectItem::TableWildcard(_) => false,
        })
        || select.where_clause.is_some_and(expression_contains_with)
        || select.having.is_some_and(expression_contains_with)
        || select
            .group_by
            .iter()
            .any(|expression| expression_contains_with(expression))
        || select
            .distinct_on
            .iter()
            .any(|expression| expression_contains_with(expression))
        || select
            .order_by
            .iter()
            .any(|order| expression_contains_with(order.expression))
        || select.limit.is_some_and(expression_contains_with)
        || select.offset.is_some_and(expression_contains_with)
        || select.from.is_some_and(|from| {
            table_contains_with(&from.base)
                || from.joins.iter().any(|join| {
                    table_contains_with(&join.table)
                        || join.on.is_some_and(expression_contains_with)
                })
        })
        || select.set_body.is_some_and(set_tree_contains_with)
}

#[allow(clippy::too_many_arguments)]
fn expand_ctes_with_path<'a>(
    sel: &'a Select<'a>,
    storage: &Storage,
    txid: u32,
    path: Option<crate::storage::PathContext>,
    dependencies: Option<crate::storage::StoredQueryDependencyView<'_>>,
    depth: u32,
    authorization_role: ViewReader,
    arena: &'a Arena,
) -> Result<&'a Select<'a>, SqlError> {
    // Fast path: nothing to rewrite (no CTEs anywhere and no views defined).
    if !select_contains_with(sel) && dependencies.is_none() && !storage.has_any_view() {
        return Ok(sel);
    }
    with_schema_context(
        sel.with,
        storage,
        txid,
        path,
        dependencies,
        depth,
        authorization_role,
        arena,
        None,
        |context| subst_select_body(sel, context, arena),
    )
}

#[expect(
    clippy::too_many_arguments,
    reason = "schema-only stored query binding context"
)]
fn with_schema_context<'a, 's, R>(
    with: &'a [Cte<'a>],
    storage: &'s Storage,
    txid: u32,
    path: Option<crate::storage::PathContext>,
    dependencies: Option<crate::storage::StoredQueryDependencyView<'s>>,
    depth: u32,
    authorization_role: ViewReader,
    arena: &'a Arena,
    mut visit: Option<&mut dyn FnMut(ScopedCte<'a>) -> Result<(), SqlError>>,
    build: impl for<'c> FnOnce(Subst<'c, 'a, 's, 'a>) -> Result<R, SqlError>,
) -> Result<R, SqlError> {
    // Resolve CTEs in dependency order so each reads only resolved ones.
    let mut resolved = ArenaList::new(arena);
    let mut materialized = ArenaList::new(arena);
    for &index in cte_order(with, arena)? {
        let cte = &with[index];
        if with[..index].iter().any(|prior| prior.name == cte.name) {
            return Err(sql_err!(
                sqlstate::DUPLICATE_ALIAS,
                "WITH query name \"{}\" specified more than once",
                cte.name
            ));
        }
        let context = Subst {
            ctes: resolved.as_slice(),
            materialized: materialized.as_slice(),
            storage,
            txid,
            depth,
            path,
            dependencies,
            authorization_role,
            qualifier: None,
            rule_transition: None,
            recursive_state: None,
            execution: None,
        };
        let self_references = select_references(cte.query, cte.name);
        if (cte.search.is_some() || cte.cycle.is_some()) && (!cte.recursive || self_references == 0)
        {
            return Err(sql_err!(
                sqlstate::SYNTAX_ERROR,
                "WITH query is not recursive"
            ));
        }
        if let Some(dml) = cte.dml {
            let expanded = match dml {
                Stmt::Insert(insert) => Stmt::Insert(subst_insert(insert, context, arena)?),
                Stmt::Update(update) => Stmt::Update(subst_update(update, context, arena)?),
                Stmt::Delete(delete) => Stmt::Delete(subst_delete(delete, context, arena)?),
                Stmt::Merge(merge) => Stmt::Merge(subst_merge(merge, context, arena)?),
                _ => {
                    return Err(sql_err!(
                        sqlstate::FEATURE_NOT_SUPPORTED,
                        "a data-modifying WITH sub-statement must be INSERT, UPDATE, DELETE or MERGE"
                    ));
                }
            };
            let entry = describe_dml_materialized(cte, &expanded, storage, txid, arena)?;
            if let Some(visit) = &mut visit {
                visit(ScopedCte::DataModification(
                    arena.alloc(expanded).map_err(|_| arena_full())?,
                ))?;
            }
            materialized
                .push((cte.name, entry))
                .map_err(|_| arena_full())?;
        } else if cte.recursive && self_references > 0 {
            let entry = describe_recursive_materialized(cte, context, storage, txid, arena)?;
            materialized
                .push((cte.name, entry))
                .map_err(|_| arena_full())?;
            if let Some(visit) = &mut visit {
                let context = Subst {
                    ctes: resolved.as_slice(),
                    materialized: materialized.as_slice(),
                    storage,
                    txid,
                    depth,
                    path,
                    dependencies,
                    authorization_role,
                    qualifier: None,
                    rule_transition: None,
                    recursive_state: None,
                    execution: None,
                };
                visit(ScopedCte::Select(subst_select(cte.query, context, arena)?))?;
            }
        } else {
            let query = subst_select(cte.query, context, arena)?;
            check_inlined_columns(cte, query, storage, txid, arena)?;
            if let Some(visit) = &mut visit {
                visit(ScopedCte::Select(query))?;
            }
            resolved
                .push((cte.name, query, cte.columns))
                .map_err(|_| arena_full())?;
        }
    }
    // Substitute the body against all CTEs (the WITH list is dropped by
    // subst_select, which never copies it) and expand any view references.
    let context = Subst {
        ctes: resolved.as_slice(),
        materialized: materialized.as_slice(),
        storage,
        txid,
        depth,
        path,
        dependencies,
        authorization_role,
        qualifier: None,
        rule_transition: None,
        recursive_state: None,
        execution: None,
    };
    build(context)
}

#[derive(Clone, Copy)]
pub(super) enum ScopedCte<'a> {
    Select(&'a Select<'a>),
    DataModification(&'a Stmt<'a>),
}

/// Visit each CTE after binding earlier entries and a recursive self-reference.
pub(super) fn visit_cte_sources<'a>(
    with: &'a [Cte<'a>],
    storage: &Storage,
    txid: u32,
    path: Option<crate::storage::PathContext>,
    dependencies: Option<crate::storage::StoredQueryDependencyView<'_>>,
    arena: &'a Arena,
    visit: &mut dyn FnMut(ScopedCte<'a>) -> Result<(), SqlError>,
) -> Result<(), SqlError> {
    with_schema_context(
        with,
        storage,
        txid,
        path,
        dependencies,
        0,
        ViewReader::Session,
        arena,
        Some(visit),
        |_| Ok(()),
    )
}

/// Resolve input shapes without materializing a CTE or executing its defaults.
pub(crate) fn expand_dml_ctes_schema<'a>(
    statement: &'a Stmt<'a>,
    with: &'a [Cte<'a>],
    storage: &Storage,
    txid: u32,
    path: Option<crate::storage::PathContext>,
    dependencies: Option<crate::storage::StoredQueryDependencyView<'_>>,
    arena: &'a Arena,
) -> Result<&'a Stmt<'a>, SqlError> {
    with_schema_context(
        with,
        storage,
        txid,
        path,
        dependencies,
        0,
        ViewReader::Session,
        arena,
        None,
        |context| {
            let statement = match statement {
                Stmt::Insert(insert) => Stmt::Insert(subst_insert(insert, context, arena)?),
                Stmt::Update(update) => Stmt::Update(subst_update(update, context, arena)?),
                Stmt::Delete(delete) => Stmt::Delete(subst_delete(delete, context, arena)?),
                Stmt::Merge(merge) => Stmt::Merge(subst_merge(merge, context, arena)?),
                _ => {
                    return Err(sql_err!(
                        sqlstate::INTERNAL_ERROR,
                        "assignment statement kind is invalid"
                    ));
                }
            };
            Ok(&*arena.alloc(statement).map_err(|_| arena_full())?)
        },
    )
}

/// Expand stored-view sources for metadata-only DML target/source lookup.
pub(crate) fn expand_from_schema<'a>(
    from: &'a FromClause<'a>,
    storage: &Storage,
    txid: u32,
    arena: &'a Arena,
) -> Result<&'a FromClause<'a>, SqlError> {
    if !storage.has_any_view() {
        return Ok(from);
    }
    with_schema_context(
        &[],
        storage,
        txid,
        None,
        None,
        0,
        ViewReader::Session,
        arena,
        None,
        |context| {
            let expanded = subst_from(from, context, arena)?;
            Ok(&*arena.alloc(expanded).map_err(|_| arena_full())?)
        },
    )
}

/// [`expand_from_schema`] for a write: `from.base` is the write's target,
/// whose view (and the views below it) are expanded without checking SELECT,
/// since writing through a view does not read it; the other items are read.
pub(crate) fn expand_dml_from_schema<'a>(
    from: &'a FromClause<'a>,
    storage: &Storage,
    txid: u32,
    arena: &'a Arena,
) -> Result<&'a FromClause<'a>, SqlError> {
    if !storage.has_any_view() {
        return Ok(from);
    }
    with_schema_context(
        &[],
        storage,
        txid,
        None,
        None,
        0,
        ViewReader::Session,
        arena,
        None,
        |context| {
            let base = subst_tableref(
                &from.base,
                Subst {
                    authorization_role: ViewReader::WriteTarget,
                    ..context
                },
                arena,
            )?;
            let mut joins = crate::mem::arena::ArenaList::new(arena);
            for join in from.joins {
                joins
                    .push(Join {
                        table: subst_tableref(&join.table, context, arena)?,
                        on: opt_subst(join.on, context, arena)?,
                        ..*join
                    })
                    .map_err(|_| arena_full())?;
            }
            Ok(&*arena
                .alloc(FromClause {
                    base,
                    joins: joins.as_slice(),
                })
                .map_err(|_| arena_full())?)
        },
    )
}

/// Like [`expand_ctes`], but for execution: a self-referencing recursive CTE is
/// materialized to a fixpoint (base term, then the recursive term iterated with
/// the CTE name bound to the previous iteration's rows) and its references
/// resolve to the finished row set.
pub fn expand_ctes_exec<'a>(
    sel: &'a Select<'a>,
    storage: &Storage,
    txid: u32,
    arena: &'a Arena,
    params: &[Datum<'a>],
    dml_mats: &[(&'a str, &'a MaterializedCte<'a>)],
    sequences: Option<&dyn SequenceAccess>,
) -> Result<&'a Select<'a>, SqlError> {
    expand_enclosed_ctes_exec(sel, storage, txid, arena, params, dml_mats, sequences, None)
}

/// [`expand_ctes_exec`] for a query a routine runs: its `WITH` queries read
/// the routine's variables from `enclosing`, as PostgreSQL's parser hook
/// binds them.
#[allow(clippy::too_many_arguments)]
pub(crate) fn expand_enclosed_ctes_exec<'a>(
    sel: &'a Select<'a>,
    storage: &Storage,
    txid: u32,
    arena: &'a Arena,
    params: &[Datum<'a>],
    dml_mats: &[(&'a str, &'a MaterializedCte<'a>)],
    sequences: Option<&dyn SequenceAccess>,
    enclosing: Option<&dyn crate::sql::eval::ColumnLookup<'a>>,
) -> Result<&'a Select<'a>, SqlError> {
    if !select_contains_with(sel) && !storage.has_any_view() {
        return Ok(sel);
    }
    analyze_unreferenced_ctes(sel, storage, txid, arena, enclosing)?;
    with_exec_context(
        sel.with,
        storage,
        txid,
        arena,
        ExecutionSubst::materializing(params, sequences),
        dml_mats,
        &[],
        &[],
        &[],
        0,
        None,
        None,
        ViewReader::Session,
        enclosing,
        |name| body_references(sel, name),
        |context| subst_select_body(sel, context, arena),
    )
}

/// [`expand_ctes_exec`] for a result stream, which must not run a query
/// before its first row is read: `None` when some CTE, at any depth or in a
/// view body, must run once (`MATERIALIZED`, referenced twice, volatile,
/// recursive or data-modifying). Every other CTE inlines as PostgreSQL's
/// planner inlines it, and views expand, locked and with their owners'
/// privileges, as the general executor expands them.
pub(crate) fn expand_stream_ctes<'a>(
    sel: &'a Select<'a>,
    storage: &Storage,
    txid: u32,
    arena: &'a Arena,
) -> Result<Option<&'a Select<'a>>, SqlError> {
    if !select_contains_with(sel) && !storage.has_any_view() {
        return Ok(Some(sel));
    }
    if sel.with.iter().any(|cte| cte.dml.is_some()) {
        return Ok(None);
    }
    analyze_unreferenced_ctes(sel, storage, txid, arena, None)?;
    let refused = core::cell::Cell::new(false);
    let expanded = with_exec_context(
        sel.with,
        storage,
        txid,
        arena,
        ExecutionSubst {
            params: &[],
            sequences: None,
            refused: Some(&refused),
        },
        &[],
        &[],
        &[],
        &[],
        0,
        None,
        None,
        ViewReader::Session,
        None,
        |name| body_references(sel, name),
        |context| subst_select_body(sel, context, arena),
    );
    if refused.get() {
        return Ok(None);
    }
    expanded.map(Some)
}

/// PostgreSQL analyses every query in a WITH clause, including one no part
/// of the statement reads, which it then never runs: `WITH x AS (SELECT *
/// FROM missing) SELECT 1` fails. Each such query is described, with the
/// queries before it in scope, without reading a row.
fn analyze_unreferenced_ctes<'a>(
    sel: &'a Select<'a>,
    storage: &Storage,
    txid: u32,
    arena: &'a Arena,
    enclosing: Option<&dyn crate::sql::eval::ColumnLookup<'a>>,
) -> Result<(), SqlError> {
    let variables = enclosing.map(crate::sql::eval::RoutineVariables);
    for (index, cte) in sel.with.iter().enumerate() {
        if cte.dml.is_some() || select_references(sel, cte.name) != 0 {
            continue;
        }
        let probe = arena
            .alloc(Select {
                items: &[SelectItem::Wildcard(
                    crate::sql::ast::SourcePosition::UNKNOWN,
                )],
                distinct: false,
                distinct_on: &[],
                windows: &[],
                from: Some(FromClause {
                    base: TableRef {
                        schema: None,
                        table: cte.name,
                        alias: None,
                        subquery: None,
                        func_args: None,
                        func_argument_names: &[],
                        func_variadic: false,
                        func_source_position: crate::sql::ast::SourcePosition::UNKNOWN,
                        rows_from: None,
                        col_alias: None,
                        inheritance: crate::sql::ast::RelationInheritance::Descendants,
                        sample: None,
                        cte: None,
                        with_ordinality: false,
                        lateral: false,
                        authorization_role: None,
                        bound_table: None,
                        view_access: None,
                        join_members: None,
                    },
                    joins: &[],
                }),
                where_clause: None,
                group_by: &[],
                grouping_sets: &[],
                having: None,
                order_by: &[],
                limit: None,
                offset: None,
                with_ties: false,
                with: &sel.with[..=index],
                set_body: None,
                locking: &[],
                ..*sel
            })
            .map_err(|_| arena_full())?;
        let mark = arena.mark();
        let mut columns = [ColDesc::new("", 0, 0); MAX_PROJ];
        let described = expand_ctes(probe, storage, txid, arena).and_then(|probe| {
            super::describe_select(
                probe,
                storage,
                txid,
                arena,
                &mut columns,
                crate::sql::exec::DescriptionTarget::Sql,
                variables
                    .as_ref()
                    .map(|row| row as &dyn crate::sql::eval::ColumnLookup<'_>),
            )
        });
        // Only the verdict leaves; the description is discarded.
        unsafe { arena.rewind_to(mark) };
        described?;
    }
    Ok(())
}

/// Binds executor-owned relations into a query as typed materialized sources.
/// Trigger transition tables use this path: their rows already belong to the
/// statement arena and must never be rendered back into SQL text.
pub(crate) fn bind_materialized_relations<'a>(
    select: &'a Select<'a>,
    relations: &'a [(&'a str, &'a MaterializedCte<'a>)],
    storage: &Storage,
    txid: u32,
    arena: &'a Arena,
) -> Result<&'a Select<'a>, SqlError> {
    let context = materialized_input_context(relations, storage, txid);
    subst_select(select, context, arena)
}

pub(crate) fn bind_materialized_expression<'a>(
    expression: &'a Expr<'a>,
    relations: &'a [(&'a str, &'a MaterializedCte<'a>)],
    storage: &Storage,
    txid: u32,
    arena: &'a Arena,
) -> Result<&'a Expr<'a>, SqlError> {
    subst_expr(
        expression,
        materialized_input_context(relations, storage, txid),
        arena,
    )
}

/// Binds relation metadata without executing CTEs or converting row values.
pub(crate) fn bind_materialized_statement<'a>(
    statement: &'a Stmt<'a>,
    relations: &'a [(&'a str, &'a MaterializedCte<'a>)],
    storage: &Storage,
    txid: u32,
    arena: &'a Arena,
) -> Result<&'a Stmt<'a>, SqlError> {
    subst_materialized_statement(
        statement,
        materialized_input_context(relations, storage, txid),
        arena,
    )
}

fn materialized_input_context<'a, 's>(
    relations: &'a [(&'a str, &'a MaterializedCte<'a>)],
    storage: &'s Storage,
    txid: u32,
) -> Subst<'a, 'a, 's, 'a> {
    Subst {
        ctes: &[],
        materialized: relations,
        storage,
        txid,
        depth: 0,
        path: None,
        dependencies: None,
        authorization_role: ViewReader::Session,
        qualifier: None,
        rule_transition: None,
        recursive_state: None,
        execution: None,
    }
}

/// The order a WITH list's queries are analysed in: written order, or under
/// `RECURSIVE` (where any query may read any other) each query after the
/// ones it reads, as PostgreSQL's `TopologicalSort` orders them. A cycle
/// through two or more queries is PostgreSQL's 0A000.
fn cte_order<'a>(with: &[Cte<'_>], arena: &'a Arena) -> Result<&'a [usize], SqlError> {
    for (index, cte) in with.iter().enumerate() {
        if with[..index].iter().any(|prior| prior.name == cte.name) {
            return Err(sql_err!(
                sqlstate::DUPLICATE_ALIAS,
                "WITH query name \"{}\" specified more than once",
                cte.name
            ));
        }
    }
    let order = arena
        .alloc_slice_with(with.len(), |index| index)
        .map_err(|_| arena_full())?;
    if !with.iter().any(|cte| cte.recursive) {
        return Ok(order);
    }
    let depends = |reader: usize, read: usize| {
        reader != read && cte_references(&with[reader], with[read].name) > 0
    };
    // order[..done] is sorted; pick the first remaining query whose
    // dependencies are all sorted.
    for done in 0..with.len() {
        let ready = (done..with.len()).find(|&candidate| {
            (done..with.len()).all(|other| !depends(order[candidate], order[other]))
        });
        let Some(ready) = ready else {
            return Err(crate::sql::error_position::at_reference(
                sql_err!(
                    sqlstate::FEATURE_NOT_SUPPORTED,
                    "mutual recursion between WITH items is not implemented"
                ),
                with[order[done]].name,
            ));
        };
        order[done..=ready].rotate_right(1);
    }
    Ok(order)
}

fn materialized_not_shadowed<'a, 'scope>(
    with: &[Cte<'a>],
    relations: &'scope [(&'a str, &'a MaterializedCte<'a>)],
    arena: &'a Arena,
) -> Result<&'scope [(&'a str, &'a MaterializedCte<'a>)], SqlError>
where
    'a: 'scope,
{
    if !relations
        .iter()
        .any(|relation| with.iter().any(|cte| cte.name == relation.0))
    {
        return Ok(relations);
    }
    let mut visible = ArenaList::new(arena);
    for relation in relations {
        if !with.iter().any(|cte| cte.name == relation.0) {
            visible.push(*relation).map_err(|_| arena_full())?;
        }
    }
    Ok(visible.as_slice())
}

fn subst_materialized_statement<'a>(
    statement: &'a Stmt<'a>,
    context: Subst<'_, 'a, '_, '_>,
    arena: &'a Arena,
) -> Result<&'a Stmt<'a>, SqlError> {
    let statement = match statement {
        Stmt::Select(select) => Stmt::Select(*subst_select(select, context, arena)?),
        Stmt::SetQuery(query) if !query.with.is_empty() => {
            let statement = arena
                .alloc(Stmt::SetQuery(SetQuery {
                    with: &[],
                    ..*query
                }))
                .map_err(|_| arena_full())?;
            let statement = arena
                .alloc(Stmt::With {
                    ctes: query.with,
                    statement,
                })
                .map_err(|_| arena_full())?;
            return subst_materialized_statement(statement, context, arena);
        }
        Stmt::SetQuery(query) => Stmt::SetQuery(*subst_set_query_body(query, context, arena)?),
        Stmt::Insert(insert) => Stmt::Insert(subst_insert(insert, context, arena)?),
        Stmt::Update(update) => Stmt::Update(subst_update(update, context, arena)?),
        Stmt::Delete(delete) => Stmt::Delete(subst_delete(delete, context, arena)?),
        Stmt::Merge(merge) => Stmt::Merge(subst_merge(merge, context, arena)?),
        Stmt::With { ctes, statement } => {
            let mut bound = ArenaList::new(arena);
            for (index, cte) in ctes.iter().enumerate() {
                let visible = materialized_not_shadowed(
                    &ctes[..index + usize::from(cte.recursive)],
                    context.materialized,
                    arena,
                )?;
                let child = Subst {
                    materialized: visible,
                    ..context
                };
                let cte = Cte {
                    query: subst_select(cte.query, child, arena)?,
                    dml: cte
                        .dml
                        .map(|statement| subst_materialized_statement(statement, child, arena))
                        .transpose()?,
                    ..*cte
                };
                bound.push(cte).map_err(|_| arena_full())?;
            }
            let child = Subst {
                materialized: materialized_not_shadowed(ctes, context.materialized, arena)?,
                ..context
            };
            Stmt::With {
                ctes: bound.as_slice(),
                statement: subst_materialized_statement(statement, child, arena)?,
            }
        }
        _ => return Ok(statement),
    };
    arena
        .alloc(statement)
        .map(|statement| &*statement)
        .map_err(|_| arena_full())
}

/// Expands a `WITH` list into a data-modifying main statement. The returned
/// AST borrows only the statement arena and materialized CTE rows, never the
/// catalog borrow used while resolving names. That separation is what permits
/// the caller to mutate storage after expansion.
#[allow(clippy::too_many_arguments)]
pub fn expand_dml_ctes<'a>(
    statement: &'a Stmt<'a>,
    with: &'a [Cte<'a>],
    storage: &Storage,
    txid: u32,
    arena: &'a Arena,
    params: &[Datum<'a>],
    dml_mats: &[(&'a str, &'a MaterializedCte<'a>)],
    sequences: Option<&dyn SequenceAccess>,
) -> Result<&'a Stmt<'a>, SqlError> {
    expand_dml_ctes_with_relations(
        statement,
        with,
        storage,
        txid,
        arena,
        params,
        dml_mats,
        &[],
        sequences,
    )
}

/// Rebinds a SQL-standard routine statement to the catalog identities captured
/// when the routine was defined. String-literal bodies deliberately use the
/// ordinary invocation-time path and never enter this function.
#[expect(
    clippy::too_many_arguments,
    reason = "stored statements require their captured catalog and evaluation context"
)]
pub(crate) fn expand_stored_statement_exec<'a>(
    statement: &'a Stmt<'a>,
    storage: &Storage,
    txid: u32,
    path: crate::storage::PathContext,
    dependencies: crate::storage::StoredQueryDependencyView<'_>,
    arena: &'a Arena,
    params: &[Datum<'a>],
    sequences: Option<&dyn SequenceAccess>,
) -> Result<&'a Stmt<'a>, SqlError> {
    expand_stored_statement_with_transition(
        statement,
        storage,
        txid,
        path,
        dependencies,
        arena,
        params,
        sequences,
        ViewReader::Session,
        None,
    )
}

#[expect(
    clippy::too_many_arguments,
    reason = "a stored rule action carries its catalog and transition-row binding"
)]
pub(crate) fn expand_stored_rule_action_exec<'a>(
    statement: &'a Stmt<'a>,
    storage: &Storage,
    txid: u32,
    path: crate::storage::PathContext,
    dependencies: crate::storage::StoredQueryDependencyView<'_>,
    arena: &'a Arena,
    params: &[Datum<'a>],
    sequences: Option<&dyn SequenceAccess>,
    authorization_role: u16,
    names: &'a [&'a str],
    old: &'a [&'a Expr<'a>],
    new: &'a [&'a Expr<'a>],
    old_available: bool,
    new_available: bool,
) -> Result<&'a Stmt<'a>, SqlError> {
    expand_stored_statement_with_transition(
        statement,
        storage,
        txid,
        path,
        dependencies,
        arena,
        params,
        sequences,
        ViewReader::Role(authorization_role),
        Some(RuleTransition {
            names,
            old,
            new,
            old_available,
            new_available,
        }),
    )
}

#[expect(
    clippy::too_many_arguments,
    reason = "a stored rule condition carries its catalog and transition-row binding"
)]
pub(crate) fn expand_stored_rule_expression_exec<'a>(
    expression: &'a Expr<'a>,
    storage: &Storage,
    txid: u32,
    path: crate::storage::PathContext,
    dependencies: crate::storage::StoredQueryDependencyView<'_>,
    arena: &'a Arena,
    params: &[Datum<'a>],
    sequences: Option<&dyn SequenceAccess>,
    authorization_role: u16,
    names: &'a [&'a str],
    old: &'a [&'a Expr<'a>],
    new: &'a [&'a Expr<'a>],
    old_available: bool,
    new_available: bool,
) -> Result<&'a Expr<'a>, SqlError> {
    with_exec_context(
        &[],
        storage,
        txid,
        arena,
        ExecutionSubst::materializing(params, sequences),
        &[],
        &[],
        &[],
        &[],
        0,
        Some(path),
        Some(dependencies),
        ViewReader::Role(authorization_role),
        None,
        |_| 0,
        |context| {
            subst_expr(
                expression,
                Subst {
                    rule_transition: Some(RuleTransition {
                        names,
                        old,
                        new,
                        old_available,
                        new_available,
                    }),
                    ..context
                },
                arena,
            )
        },
    )
}

fn prepend_rule_source<'a>(
    source: TableRef<'a>,
    existing: Option<&FromClause<'a>>,
    arena: &'a Arena,
) -> Result<FromClause<'a>, SqlError> {
    let Some(existing) = existing else {
        return Ok(FromClause {
            base: source,
            joins: &[],
        });
    };
    let joins = arena
        .alloc_slice_with(existing.joins.len() + 1, |index| {
            if index == 0 {
                Join {
                    table: existing.base,
                    kind: JoinKind::Comma,
                    on: None,
                    using: None,
                    natural: false,
                }
            } else {
                existing.joins[index - 1]
            }
        })
        .map_err(|_| arena_full())?;
    Ok(FromClause {
        base: source,
        joins,
    })
}

fn rule_select<'a>(
    items: &'a [SelectItem<'a>],
    from: Option<FromClause<'a>>,
    where_clause: Option<&'a Expr<'a>>,
) -> Select<'a> {
    Select {
        items,
        distinct: false,
        distinct_on: &[],
        windows: &[],
        from,
        where_clause,
        group_by: &[],
        grouping_set_quantifier: GroupingSetQuantifier::All,
        grouping_sets: &[],
        having: None,
        order_by: &[],
        limit: None,
        offset: None,
        with_ties: false,
        with: &[],
        set_body: None,
        locking: &[],
    }
}

fn rule_relation_ref<'a>(
    name: crate::sql::ast::QualName<'a>,
    alias: Option<&'a str>,
) -> TableRef<'a> {
    TableRef {
        schema: name.schema,
        table: name.name,
        alias,
        subquery: None,
        func_args: None,
        func_argument_names: &[],
        func_variadic: false,
        func_source_position: crate::sql::ast::SourcePosition::UNKNOWN,
        rows_from: None,
        col_alias: None,
        inheritance: crate::sql::ast::RelationInheritance::Descendants,
        sample: None,
        cte: None,
        with_ordinality: false,
        lateral: false,
        authorization_role: None,
        bound_table: None,
        view_access: None,
        join_members: None,
    }
}

fn append_rule_input_from<'a>(
    base: TableRef<'a>,
    existing: Option<&FromClause<'a>>,
    arena: &'a Arena,
) -> Result<FromClause<'a>, SqlError> {
    let Some(existing) = existing else {
        return Ok(FromClause { base, joins: &[] });
    };
    let joins = arena
        .alloc_slice_with(existing.joins.len() + 1, |index| {
            if index == 0 {
                Join {
                    table: existing.base,
                    kind: JoinKind::Comma,
                    on: None,
                    using: None,
                    natural: false,
                }
            } else {
                existing.joins[index - 1]
            }
        })
        .map_err(|_| arena_full())?;
    Ok(FromClause { base, joins })
}

/// A query whose FROM and expressions are those of a data-modifying
/// statement's own scope (its target under its alias, `FROM`/`USING` sources,
/// and `excluded` for ON CONFLICT), for parameter type inference only: its
/// items are the assignments, conflict and RETURNING expressions and its
/// WHERE the statement's conditions. It is never executed.
pub(crate) fn dml_parameter_scope_query<'a>(
    statement: &Stmt<'a>,
    arena: &'a Arena,
) -> Result<Option<&'a Select<'a>>, SqlError> {
    let (target, alias, extra, assignments, condition, returning, excluded) = match statement {
        Stmt::Update(update) => (
            update.table,
            update.alias,
            update.from,
            update.assignments,
            update.where_clause,
            update.returning,
            false,
        ),
        Stmt::Delete(delete) => (
            delete.table,
            delete.alias,
            delete.using,
            &[][..],
            delete.where_clause,
            delete.returning,
            false,
        ),
        Stmt::Insert(insert) => (
            insert.table,
            insert.alias,
            None,
            insert
                .on_conflict
                .and_then(|conflict| conflict.update)
                .unwrap_or(&[]),
            insert
                .on_conflict
                .and_then(|conflict| conflict.update_where),
            insert.returning,
            insert
                .on_conflict
                .is_some_and(|conflict| conflict.update.is_some()),
        ),
        _ => return Ok(None),
    };
    let returned = returning
        .items
        .iter()
        .filter(|item| matches!(item, SelectItem::Expr { .. } | SelectItem::RecordStar(_)))
        .count();
    if assignments.is_empty() && condition.is_none() && returned == 0 {
        return Ok(None);
    }
    let mut from = append_rule_input_from(rule_relation_ref(target, alias), extra, arena)?;
    if excluded {
        let joins = arena
            .alloc_slice_with(from.joins.len() + 1, |index| {
                if index < from.joins.len() {
                    from.joins[index]
                } else {
                    Join {
                        table: rule_relation_ref(target, Some("excluded")),
                        kind: JoinKind::Comma,
                        on: None,
                        using: None,
                        natural: false,
                    }
                }
            })
            .map_err(|_| arena_full())?;
        from.joins = joins;
    }
    let mut items: ArenaList<'a, SelectItem<'a>> = ArenaList::new(arena);
    for (_, expression) in assignments {
        items
            .push(SelectItem::Expr {
                expression,
                alias: None,
            })
            .map_err(|_| arena_full())?;
    }
    for item in returning.items {
        if matches!(item, SelectItem::Expr { .. } | SelectItem::RecordStar(_)) {
            items.push(*item).map_err(|_| arena_full())?;
        }
    }
    let select = arena
        .alloc(rule_select(items.as_slice(), Some(from), condition))
        .map_err(|_| arena_full())?;
    Ok(Some(&*select))
}

#[derive(Clone, Copy)]
pub(crate) struct RuleTransitionSource<'a> {
    pub source: TableRef<'a>,
    pub names: &'a [&'a str],
    pub old: &'a [&'a Expr<'a>],
    pub new: &'a [&'a Expr<'a>],
}

#[derive(Clone, Copy)]
pub(crate) struct RuleTransitionExpressions<'a> {
    pub names: &'a [&'a str],
    pub old: &'a [&'a Expr<'a>],
    pub new: &'a [&'a Expr<'a>],
}

#[derive(Clone, Copy)]
pub(crate) struct RuleTransitionType<'a> {
    pub type_name: &'a str,
    pub type_mod: i32,
    pub collation: Option<crate::sql::ast::ParsedCollation<'a>>,
}

fn coerce_rule_transition<'a>(
    expression: &'a Expr<'a>,
    target: RuleTransitionType<'a>,
    arena: &'a Arena,
) -> Result<&'a Expr<'a>, SqlError> {
    let cast = arena
        .alloc(Expr::Cast {
            source_position: crate::sql::ast::SourcePosition::UNKNOWN,
            operand: expression,
            type_name: target.type_name,
            type_mod: target.type_mod,
            binding: crate::sql::ast::CastBinding::Resolve,
        })
        .map(|expression| &*expression)
        .map_err(|_| arena_full())?;
    match target.collation {
        Some(collation) => arena
            .alloc(Expr::Collate {
                operand: cast,
                collation,
            })
            .map(|expression| &*expression)
            .map_err(|_| arena_full()),
        None => Ok(cast),
    }
}

pub(crate) fn rule_original_transition<'a>(
    statement: &'a Stmt<'a>,
    columns: &'a [&'a str],
    types: &'a [RuleTransitionType<'a>],
    arena: &'a Arena,
) -> Result<RuleTransitionExpressions<'a>, SqlError> {
    if types.len() != columns.len() {
        return Err(sql_err!(
            sqlstate::INTERNAL_ERROR,
            "rewrite transition type shape does not match its columns"
        ));
    }
    let mut old = [&Expr::Null; crate::storage::MAX_COLUMNS];
    let mut new = [&Expr::Null; crate::storage::MAX_COLUMNS];
    match statement {
        Stmt::Update(update) => {
            let qualifier = update.alias.unwrap_or(update.table.name);
            for (index, name) in columns.iter().copied().enumerate() {
                old[index] = arena
                    .alloc(Expr::Column {
                        qualifier: Some(qualifier),
                        name,
                    })
                    .map(|expression| &*expression)
                    .map_err(|_| arena_full())?;
                new[index] = update
                    .assignments
                    .iter()
                    .find_map(|(written, expression)| (*written == name).then_some(*expression))
                    .unwrap_or(old[index]);
                new[index] = coerce_rule_transition(new[index], types[index], arena)?;
            }
        }
        Stmt::Delete(delete) => {
            let qualifier = delete.alias.unwrap_or(delete.table.name);
            for (index, name) in columns.iter().copied().enumerate() {
                old[index] = arena
                    .alloc(Expr::Column {
                        qualifier: Some(qualifier),
                        name,
                    })
                    .map(|expression| &*expression)
                    .map_err(|_| arena_full())?;
            }
        }
        Stmt::Insert(_) => {}
        _ => {
            return Err(sql_err!(
                sqlstate::INTERNAL_ERROR,
                "rewrite transition requires INSERT, UPDATE or DELETE"
            ));
        }
    }
    Ok(RuleTransitionExpressions {
        names: columns,
        old: arena
            .alloc_slice_copy(&old[..columns.len()])
            .map_err(|_| arena_full())?,
        new: arena
            .alloc_slice_copy(&new[..columns.len()])
            .map_err(|_| arena_full())?,
    })
}

pub(crate) fn restrict_rule_original<'a>(
    statement: &'a Stmt<'a>,
    transition: RuleTransitionSource<'a>,
    columns: &'a [&'a str],
    predicate: &'a Expr<'a>,
    arena: &'a Arena,
) -> Result<&'a Stmt<'a>, SqlError> {
    let rewritten = match statement {
        Stmt::Update(update) => Stmt::Update(Update {
            where_clause: and_where(update.where_clause, Some(predicate), arena)?,
            ..*update
        }),
        Stmt::Delete(delete) => Stmt::Delete(Delete {
            where_clause: and_where(delete.where_clause, Some(predicate), arena)?,
            ..*delete
        }),
        Stmt::Insert(insert) => {
            let items = arena
                .alloc_slice_with(columns.len(), |index| SelectItem::Expr {
                    expression: transition.new[index],
                    alias: None,
                })
                .map_err(|_| arena_full())?;
            let select = rule_select(
                items,
                Some(FromClause {
                    base: transition.source,
                    joins: &[],
                }),
                Some(predicate),
            );
            Stmt::Insert(Insert {
                columns,
                rows: &[],
                select: Some(&*arena.alloc(select).map_err(|_| arena_full())?),
                ..*insert
            })
        }
        _ => unreachable!("rewrite original is DML"),
    };
    arena
        .alloc(rewritten)
        .map(|statement| &*statement)
        .map_err(|_| arena_full())
}

/// Turns the original DML input into one derived relation. Rule actions scan
/// that relation once, preserving statement-level behavior while OLD/NEW are
/// ordinary typed column references rather than magic evaluator states.
pub(crate) fn rule_transition_source<'a>(
    statement: &'a Stmt<'a>,
    columns: &'a [&'a str],
    types: &'a [RuleTransitionType<'a>],
    defaults: &[Option<&'a Expr<'a>>],
    force_defaults: &[bool],
    arena: &'a Arena,
) -> Result<RuleTransitionSource<'a>, SqlError> {
    if columns.len() > crate::storage::MAX_COLUMNS {
        return Err(sql_err!(
            sqlstate::TOO_MANY_COLUMNS,
            "rewrite relation is too wide"
        ));
    }
    if types.len() != columns.len() {
        return Err(sql_err!(
            sqlstate::INTERNAL_ERROR,
            "rewrite transition type shape does not match its columns"
        ));
    }
    let source_alias = arena
        .alloc_str("__pos3ql_rule_transition")
        .map_err(|_| arena_full())?;
    let mut old = [&Expr::Null; crate::storage::MAX_COLUMNS];
    let mut new = [&Expr::Null; crate::storage::MAX_COLUMNS];
    let mut items = [SelectItem::Wildcard(crate::sql::ast::SourcePosition::UNKNOWN); MAX_PROJ];
    let mut item_count = 0usize;
    let mut add_output = |prefix: &str,
                          column: usize,
                          expression: &'a Expr<'a>,
                          destination: &mut [&'a Expr<'a>; crate::storage::MAX_COLUMNS]|
     -> Result<(), SqlError> {
        let alias = crate::stack_format!(63, "__pos3ql_{}_{}", prefix, column);
        let alias = arena.alloc_str(alias.as_str()).map_err(|_| arena_full())?;
        items[item_count] = SelectItem::Expr {
            expression,
            alias: Some(alias),
        };
        item_count += 1;
        destination[column] = arena
            .alloc(Expr::Column {
                qualifier: Some(source_alias),
                name: alias,
            })
            .map(|expression| &*expression)
            .map_err(|_| arena_full())?;
        Ok(())
    };

    let select = match statement {
        Stmt::Update(update) => {
            let target_qualifier = update.alias.unwrap_or(update.table.name);
            let target = rule_relation_ref(update.table, update.alias);
            let from = append_rule_input_from(target, update.from, arena)?;
            for (column, name) in columns.iter().copied().enumerate() {
                let old_expression = arena
                    .alloc(Expr::Column {
                        qualifier: Some(target_qualifier),
                        name,
                    })
                    .map(|expression| &*expression)
                    .map_err(|_| arena_full())?;
                add_output("old", column, old_expression, &mut old)?;
                let new_expression = update
                    .assignments
                    .iter()
                    .find_map(|(written, expression)| (*written == name).then_some(*expression))
                    .unwrap_or(old_expression);
                add_output(
                    "new",
                    column,
                    coerce_rule_transition(new_expression, types[column], arena)?,
                    &mut new,
                )?;
            }
            rule_select(
                arena
                    .alloc_slice_copy(&items[..item_count])
                    .map_err(|_| arena_full())?,
                Some(from),
                update.where_clause,
            )
        }
        Stmt::Delete(delete) => {
            let target_qualifier = delete.alias.unwrap_or(delete.table.name);
            let target = rule_relation_ref(delete.table, delete.alias);
            let from = append_rule_input_from(target, delete.using, arena)?;
            for (column, name) in columns.iter().copied().enumerate() {
                let old_expression = arena
                    .alloc(Expr::Column {
                        qualifier: Some(target_qualifier),
                        name,
                    })
                    .map(|expression| &*expression)
                    .map_err(|_| arena_full())?;
                add_output("old", column, old_expression, &mut old)?;
            }
            rule_select(
                arena
                    .alloc_slice_copy(&items[..item_count])
                    .map_err(|_| arena_full())?,
                Some(from),
                delete.where_clause,
            )
        }
        Stmt::Insert(insert) => {
            let input_columns = if insert.columns.is_empty() {
                if insert.select.is_none() {
                    let width = insert.rows.first().map_or(0, |row| row.len());
                    columns.get(..width).ok_or_else(|| {
                        sql_err!(
                            sqlstate::SYNTAX_ERROR,
                            "INSERT has more expressions than target columns"
                        )
                    })?
                } else {
                    columns
                }
            } else {
                insert.columns
            };
            let input = if let Some(select) = insert.select {
                select
            } else {
                let mut tree: Option<&SetTree<'a>> = None;
                for row in insert.rows {
                    let row_items = arena
                        .alloc_slice_with(row.len(), |column| SelectItem::Expr {
                            expression: if matches!(row[column], Expr::DefaultMarker)
                                || input_columns
                                    .get(column)
                                    .and_then(|name| columns.iter().position(|item| item == name))
                                    .is_some_and(|target| {
                                        force_defaults.get(target).copied().unwrap_or(false)
                                    }) {
                                input_columns
                                    .get(column)
                                    .and_then(|name| columns.iter().position(|item| item == name))
                                    .and_then(|target| defaults.get(target).copied().flatten())
                                    .unwrap_or(&Expr::Null)
                            } else {
                                row[column]
                            },
                            alias: None,
                        })
                        .map_err(|_| arena_full())?;
                    let leaf_select = arena
                        .alloc(rule_select(row_items, None, None))
                        .map_err(|_| arena_full())?;
                    let leaf = arena
                        .alloc(SetTree::Select(&*leaf_select))
                        .map(|tree| &*tree)
                        .map_err(|_| arena_full())?;
                    tree = Some(match tree {
                        None => leaf,
                        Some(left) => arena
                            .alloc(SetTree::Op {
                                operator: SetOp::Union,
                                syntax: crate::sql::ast::SetOperationSyntax::Explicit,
                                all: true,
                                left,
                                right: leaf,
                            })
                            .map(|tree| &*tree)
                            .map_err(|_| arena_full())?,
                    });
                }
                arena
                    .alloc(Select {
                        set_body: tree,
                        ..rule_select(&[], None, None)
                    })
                    .map(|select| &*select)
                    .map_err(|_| arena_full())?
            };
            let input_alias = arena
                .alloc_str("__pos3ql_rule_input")
                .map_err(|_| arena_full())?;
            let input_ref = TableRef {
                schema: None,
                table: "",
                alias: Some(input_alias),
                subquery: Some(input),
                func_args: None,
                func_argument_names: &[],
                func_variadic: false,
                func_source_position: crate::sql::ast::SourcePosition::UNKNOWN,
                rows_from: None,
                col_alias: Some(input_columns),
                inheritance: crate::sql::ast::RelationInheritance::Descendants,
                sample: None,
                cte: None,
                with_ordinality: false,
                lateral: false,
                authorization_role: None,
                bound_table: None,
                view_access: None,
                join_members: None,
            };
            for (column, name) in columns.iter().copied().enumerate() {
                let expression = input_columns
                    .iter()
                    .position(|candidate| *candidate == name)
                    .filter(|_| !force_defaults.get(column).copied().unwrap_or(false))
                    .map(|_| {
                        arena
                            .alloc(Expr::Column {
                                qualifier: Some(input_alias),
                                name,
                            })
                            .map(|expression| &*expression)
                            .map_err(|_| arena_full())
                    })
                    .transpose()?
                    .unwrap_or_else(|| {
                        defaults
                            .get(column)
                            .copied()
                            .flatten()
                            .unwrap_or(&Expr::Null)
                    });
                add_output(
                    "new",
                    column,
                    coerce_rule_transition(expression, types[column], arena)?,
                    &mut new,
                )?;
            }
            rule_select(
                arena
                    .alloc_slice_copy(&items[..item_count])
                    .map_err(|_| arena_full())?,
                Some(FromClause {
                    base: input_ref,
                    joins: &[],
                }),
                None,
            )
        }
        _ => {
            return Err(sql_err!(
                sqlstate::INTERNAL_ERROR,
                "rewrite transition source requires INSERT, UPDATE or DELETE"
            ));
        }
    };
    let source_select = arena.alloc(select).map_err(|_| arena_full())?;
    let source = TableRef {
        schema: None,
        table: "",
        alias: Some(source_alias),
        subquery: Some(&*source_select),
        func_args: None,
        func_argument_names: &[],
        func_variadic: false,
        func_source_position: crate::sql::ast::SourcePosition::UNKNOWN,
        rows_from: None,
        col_alias: None,
        inheritance: crate::sql::ast::RelationInheritance::Descendants,
        sample: None,
        cte: None,
        with_ordinality: false,
        lateral: false,
        authorization_role: None,
        bound_table: None,
        view_access: None,
        join_members: None,
    };
    Ok(RuleTransitionSource {
        source,
        names: columns,
        old: arena
            .alloc_slice_copy(&old[..columns.len()])
            .map_err(|_| arena_full())?,
        new: arena
            .alloc_slice_copy(&new[..columns.len()])
            .map_err(|_| arena_full())?,
    })
}

/// The relation a rule fires on, as the triggering statement's RETURNING
/// list names it.
#[derive(Clone, Copy)]
pub(crate) struct RuleReturningRelation<'a> {
    /// The visible name: the statement's alias, else the relation's name.
    pub name: &'a str,
    /// The relation's schema when it is not aliased.
    pub schema: Option<&'a str>,
    pub columns: &'a [&'a str],
    /// `UPDATE ... FROM` or `DELETE ... USING` sources beside the relation.
    pub other_sources: bool,
}

fn rule_returning_unsupported() -> SqlError {
    sql_err!(
        sqlstate::FEATURE_NOT_SUPPORTED,
        "RETURNING through a rule's RETURNING list can reference only the rule's relation"
    )
}

/// A rule action's RETURNING entries, one expression per entry, with `*` and
/// `target.*` spelled out as the action target's columns.
pub(crate) fn rule_returning_entries<'a>(
    action: &'a Stmt<'a>,
    storage: &Storage,
    txid: u32,
    arena: &'a Arena,
) -> Result<&'a [&'a Expr<'a>], SqlError> {
    let (returning, table, alias) = match action {
        Stmt::Insert(insert) => (insert.returning, insert.table, None),
        Stmt::Update(update) => (update.returning, update.table, update.alias),
        Stmt::Delete(delete) => (delete.returning, delete.table, delete.alias),
        _ => {
            return Err(sql_err!(
                sqlstate::INTERNAL_ERROR,
                "rule RETURNING action is not INSERT, UPDATE or DELETE"
            ));
        }
    };
    let visible = alias.unwrap_or(table.name);
    let mut entries: ArenaList<'a, &'a Expr<'a>> = ArenaList::new(arena);
    for item in returning.items {
        match item {
            SelectItem::Expr { expression, .. } => {
                entries.push(expression).map_err(|_| arena_full())?;
            }
            SelectItem::Wildcard(_) | SelectItem::TableWildcard(_) => {
                if let SelectItem::TableWildcard(qualifier) = item
                    && *qualifier != visible
                {
                    return Err(rule_returning_unsupported());
                }
                let single = arena
                    .alloc_slice_copy(core::slice::from_ref(item))
                    .map_err(|_| arena_full())?;
                let described = match action {
                    Stmt::Insert(insert) => Stmt::Insert(Insert {
                        returning: Returning {
                            items: single,
                            ..insert.returning
                        },
                        ..*insert
                    }),
                    Stmt::Update(update) => Stmt::Update(Update {
                        returning: Returning {
                            items: single,
                            ..update.returning
                        },
                        ..*update
                    }),
                    Stmt::Delete(delete) => Stmt::Delete(Delete {
                        returning: Returning {
                            items: single,
                            ..delete.returning
                        },
                        ..*delete
                    }),
                    _ => unreachable!("matched above"),
                };
                let described = &*arena.alloc(described).map_err(|_| arena_full())?;
                let mut columns = [ColDesc::new("", 0, 0); MAX_PROJ];
                let count = crate::sql::exec::describe_data_modification_columns(
                    storage,
                    described,
                    arena,
                    txid,
                    &mut columns,
                    crate::sql::exec::DescriptionTarget::Sql,
                )?;
                for column in &columns[..count] {
                    let expression = arena
                        .alloc(Expr::Column {
                            qualifier: Some(visible),
                            name: arena.alloc_str(column.name).map_err(|_| arena_full())?,
                        })
                        .map_err(|_| arena_full())?;
                    entries.push(&*expression).map_err(|_| arena_full())?;
                }
            }
            SelectItem::RecordStar(_) => return Err(rule_returning_unsupported()),
        }
    }
    Ok(entries.as_slice())
}

/// The triggering statement's RETURNING list rewritten over a rule action's
/// RETURNING entries, as PostgreSQL's `rewriteRuleAction` does: a reference
/// to the rule's relation (by its name, alias, `old` or `new`) reads the
/// action's entry at that column's position, and each output keeps the name
/// the triggering statement gives it.
pub(crate) fn rule_returning_items<'a>(
    outer: Returning<'a>,
    relation: RuleReturningRelation<'a>,
    entries: &'a [&'a Expr<'a>],
    storage: &Storage,
    txid: u32,
    arena: &'a Arena,
) -> Result<&'a [SelectItem<'a>], SqlError> {
    if entries.len() != relation.columns.len() {
        return Err(sql_err!(
            sqlstate::INTERNAL_ERROR,
            "rule RETURNING entries do not match the rule's relation"
        ));
    }
    let names: &'a [&'a str] = arena
        .alloc_slice_copy(&[relation.name, outer.old_name(), outer.new_name()])
        .map_err(|_| arena_full())?;
    let known = |qualifier: &str| names.contains(&qualifier);
    let context = Subst {
        ctes: &[],
        materialized: &[],
        storage,
        txid,
        depth: 0,
        path: None,
        dependencies: None,
        authorization_role: ViewReader::Session,
        qualifier: Some(ViewQualifier {
            from: relation.name,
            from_schema: relation.schema,
            to: "",
            to_schema: "",
            exposed_columns: relation.columns,
            base_columns: &[],
            projected_columns: Some(entries),
            projected_aliases: &names[1..],
        }),
        rule_transition: None,
        recursive_state: None,
        execution: None,
    };
    let mut items: ArenaList<'a, SelectItem<'a>> = ArenaList::new(arena);
    let expand = |items: &mut ArenaList<'a, SelectItem<'a>>| -> Result<(), SqlError> {
        for (index, name) in relation.columns.iter().enumerate() {
            items
                .push(SelectItem::Expr {
                    expression: entries[index],
                    alias: Some(name),
                })
                .map_err(|_| arena_full())?;
        }
        Ok(())
    };
    for item in outer.items {
        match item {
            SelectItem::Wildcard(_) if !relation.other_sources => expand(&mut items)?,
            SelectItem::TableWildcard(qualifier) if known(qualifier) => expand(&mut items)?,
            SelectItem::TableWildcard(qualifier) if !relation.other_sources => {
                return Err(sql_err!(
                    sqlstate::UNDEFINED_TABLE,
                    "missing FROM-clause entry for table \"{}\"",
                    qualifier
                ));
            }
            SelectItem::Expr { expression, alias } => {
                // Substitution does not follow a subquery's own scope.
                if expression.contains_subquery() {
                    return Err(sql_err!(
                        sqlstate::FEATURE_NOT_SUPPORTED,
                        "subqueries in RETURNING are not supported through a rule's RETURNING list"
                    ));
                }
                let mut invalid = None;
                expression.for_each_column_reference(&mut |qualifier, name| {
                    if invalid.is_some() {
                        return;
                    }
                    let column = relation.columns.contains(&name);
                    invalid = match qualifier {
                        None if column => None,
                        Some(qualifier) if known(qualifier) && column => None,
                        _ if relation.other_sources => Some(rule_returning_unsupported()),
                        None => Some(sql_err!(
                            sqlstate::UNDEFINED_COLUMN,
                            "column \"{}\" does not exist",
                            name
                        )),
                        Some(qualifier) if known(qualifier) => Some(sql_err!(
                            sqlstate::UNDEFINED_COLUMN,
                            "column {}.{} does not exist",
                            qualifier,
                            name
                        )),
                        Some(qualifier) => Some(sql_err!(
                            sqlstate::UNDEFINED_TABLE,
                            "missing FROM-clause entry for table \"{}\"",
                            qualifier
                        )),
                    };
                });
                if let Some(error) = invalid {
                    return Err(error);
                }
                if let Expr::WholeRow(_) = expression {
                    return Err(rule_returning_unsupported());
                }
                items
                    .push(SelectItem::Expr {
                        expression: subst_expr(expression, context, arena)?,
                        alias: Some(alias.unwrap_or(crate::sql::exec::derived_name(expression))),
                    })
                    .map_err(|_| arena_full())?;
            }
            SelectItem::Wildcard(_) | SelectItem::TableWildcard(_) | SelectItem::RecordStar(_) => {
                return Err(rule_returning_unsupported());
            }
        }
    }
    Ok(items.as_slice())
}

/// The target relation of a rule's `INSERT ... VALUES` action: its column
/// names in order and their declared type identities.
pub(crate) struct RuleInsertTarget<'a> {
    pub names: &'a [&'a str],
    pub type_oids: &'a [i32],
}

/// Attaches the original statement's transition-row source to one rebound
/// action. OLD/NEW have already become columns of `source`; this step keeps
/// the action set-oriented, so its statement triggers still fire once.
/// `target` describes an `INSERT ... VALUES` action's relation; it is read
/// only when a `VALUES` row writes `DEFAULT` or there are several rows.
pub(crate) fn attach_rule_source<'a>(
    statement: &'a Stmt<'a>,
    source: TableRef<'a>,
    condition: Option<&'a Expr<'a>>,
    target: impl FnOnce() -> Result<RuleInsertTarget<'a>, SqlError>,
    arena: &'a Arena,
) -> Result<&'a Stmt<'a>, SqlError> {
    let rewritten = match statement {
        Stmt::Select(select) => {
            let from = prepend_rule_source(source, select.from.as_ref(), arena)?;
            Stmt::Select(Select {
                from: Some(from),
                where_clause: and_where(condition, select.where_clause, arena)?,
                ..*select
            })
        }
        Stmt::SetQuery(query) => Stmt::SetQuery(SetQuery {
            body: attach_rule_source_to_set_tree(query.body, source, condition, arena)?,
            ..*query
        }),
        Stmt::Update(update) => {
            let from = prepend_rule_source(source, update.from, arena)?;
            Stmt::Update(Update {
                from: Some(&*arena.alloc(from).map_err(|_| arena_full())?),
                where_clause: and_where(condition, update.where_clause, arena)?,
                ..*update
            })
        }
        Stmt::Delete(delete) => {
            let using = prepend_rule_source(source, delete.using, arena)?;
            Stmt::Delete(Delete {
                using: Some(&*arena.alloc(using).map_err(|_| arena_full())?),
                where_clause: and_where(condition, delete.where_clause, arena)?,
                ..*delete
            })
        }
        Stmt::Insert(insert) => {
            let select = if let Some(select) = insert.select {
                let from = prepend_rule_source(source, select.from.as_ref(), arena)?;
                Select {
                    from: Some(from),
                    where_clause: and_where(condition, select.where_clause, arena)?,
                    ..*select
                }
            } else {
                return arena
                    .alloc(Stmt::Insert(rule_insert_values(
                        insert, source, condition, target, arena,
                    )?))
                    .map(|statement| &*statement)
                    .map_err(|_| arena_full());
            };
            Stmt::Insert(Insert {
                rows: &[],
                select: Some(&*arena.alloc(select).map_err(|_| arena_full())?),
                ..*insert
            })
        }
        Stmt::Notify { .. } => *statement,
        _ => {
            return Err(sql_err!(
                sqlstate::FEATURE_NOT_SUPPORTED,
                "rewrite-rule action is not executable"
            ));
        }
    };
    arena
        .alloc(rewritten)
        .map(|statement| &*statement)
        .map_err(|_| arena_full())
}

/// A rule's `INSERT ... VALUES` action as `INSERT ... SELECT` over its
/// source rows. Several rows join the source as `LATERAL (VALUES ...)`, so
/// every `VALUES` row is inserted for one source row before the next, as
/// PostgreSQL's plan does. A column written as `DEFAULT` in every row leaves
/// the target list; one written as `DEFAULT` in some rows keeps a typed null
/// placeholder, and the executor takes the default for the positions the
/// trailing row-number column selects from `values_defaults`.
fn rule_insert_values<'a>(
    insert: &'a Insert<'a>,
    source: TableRef<'a>,
    condition: Option<&'a Expr<'a>>,
    target: impl FnOnce() -> Result<RuleInsertTarget<'a>, SqlError>,
    arena: &'a Arena,
) -> Result<Insert<'a>, SqlError> {
    const VALUES_ALIAS: &str = "*VALUES*";
    let is_default = |expression: &Expr<'_>| matches!(expression, Expr::DefaultMarker);
    let rows = insert.rows;
    let first = *rows
        .first()
        .ok_or_else(|| sql_err!(sqlstate::SYNTAX_ERROR, "rewrite INSERT action has no row"))?;
    let width = first.len();
    if rows.iter().any(|row| row.len() != width) {
        return Err(sql_err!(
            sqlstate::SYNTAX_ERROR,
            "VALUES lists must all be the same length"
        ));
    }
    let any_default = rows
        .iter()
        .any(|row| row.iter().any(|item| is_default(item)));
    let single = |items: &'a [SelectItem<'a>], columns: &'a [&'a str]| {
        let select = arena
            .alloc(rule_select(
                items,
                Some(FromClause {
                    base: source,
                    joins: &[],
                }),
                condition,
            ))
            .map_err(|_| arena_full())?;
        Ok(Insert {
            columns,
            rows: &[],
            select: Some(&*select),
            ..*insert
        })
    };
    if rows.len() == 1 && !any_default {
        let items = arena
            .alloc_slice_with(width, |column| SelectItem::Expr {
                expression: first[column],
                alias: None,
            })
            .map_err(|_| arena_full())?;
        return single(items, insert.columns);
    }
    let target = target()?;
    let named = if insert.columns.is_empty() {
        target.names
    } else {
        insert.columns
    };
    if width > named.len() {
        return Err(sql_err!(
            sqlstate::SYNTAX_ERROR,
            "INSERT has more expressions than target columns"
        ));
    }
    if width > crate::storage::MAX_COLUMNS {
        return Err(sql_err!(
            sqlstate::TOO_MANY_COLUMNS,
            "target lists can have at most {} entries",
            crate::storage::MAX_COLUMNS
        ));
    }
    let mut kept = [0usize; crate::storage::MAX_COLUMNS];
    let mut kept_count = 0;
    for position in 0..width {
        if !rows.iter().all(|row| is_default(row[position])) {
            kept[kept_count] = position;
            kept_count += 1;
        }
    }
    let kept = &kept[..kept_count];
    let columns = arena
        .alloc_slice_with(kept.len(), |slot| named[kept[slot]])
        .map_err(|_| arena_full())?;
    if rows.len() == 1 {
        let items = arena
            .alloc_slice_with(kept.len(), |slot| SelectItem::Expr {
                expression: first[kept[slot]],
                alias: None,
            })
            .map_err(|_| arena_full())?;
        return single(items, columns);
    }
    let mut defaults: ArenaList<'a, crate::storage::ColumnSet> = ArenaList::new(arena);
    for row in rows {
        let mut mask = crate::storage::ColumnSet::EMPTY;
        for (slot, position) in kept.iter().enumerate() {
            if is_default(row[*position]) {
                mask.insert(slot);
            }
        }
        defaults.push(mask).map_err(|_| arena_full())?;
    }
    let tagged = defaults
        .as_slice()
        .iter()
        .any(|mask| *mask != crate::storage::ColumnSet::EMPTY);
    let mut placeholder_types = [0i32; crate::storage::MAX_COLUMNS];
    for (slot, position) in kept.iter().enumerate() {
        if rows.iter().any(|row| is_default(row[*position])) {
            let name = named[*position];
            let index = target
                .names
                .iter()
                .position(|column| *column == name)
                .ok_or_else(|| {
                    sql_err!(
                        sqlstate::UNDEFINED_COLUMN,
                        "column \"{}\" of relation \"{}\" does not exist",
                        name,
                        insert.table.name
                    )
                })?;
            placeholder_types[slot] = target.type_oids[index];
        }
    }
    // The joined VALUES list numbers the rows; each target picks its row's
    // expression with a CASE in the output list, so volatile calls run as
    // each row is inserted, as PostgreSQL evaluates a VALUES row.
    let mut leaves: ArenaList<'a, &'a SetTree<'a>> = ArenaList::new(arena);
    for number in 0..rows.len() {
        let items = arena
            .alloc_slice_with(1, |_| SelectItem::Expr {
                expression: &Expr::Null,
                alias: None,
            })
            .map_err(|_| arena_full())?;
        items[0] = SelectItem::Expr {
            expression: arena
                .alloc(Expr::Int(number as i64))
                .map_err(|_| arena_full())?,
            alias: None,
        };
        let select = arena
            .alloc(rule_select(items, None, None))
            .map_err(|_| arena_full())?;
        leaves
            .push(
                arena
                    .alloc(SetTree::Select(&*select))
                    .map(|tree| &*tree)
                    .map_err(|_| arena_full())?,
            )
            .map_err(|_| arena_full())?;
    }
    let mut tree = leaves.as_slice()[0];
    for leaf in &leaves.as_slice()[1..] {
        tree = arena
            .alloc(SetTree::Op {
                operator: SetOp::Union,
                syntax: crate::sql::ast::SetOperationSyntax::Values,
                all: true,
                left: tree,
                right: leaf,
            })
            .map(|tree| &*tree)
            .map_err(|_| arena_full())?;
    }
    let values = arena
        .alloc(Select {
            set_body: Some(tree),
            ..rule_select(&[], None, None)
        })
        .map_err(|_| arena_full())?;
    let value_names: &'a [&'a str] = arena
        .alloc_slice_copy(&["column1"])
        .map_err(|_| arena_full())?;
    let row_column = &*arena
        .alloc(Expr::Column {
            qualifier: Some(VALUES_ALIAS),
            name: value_names[0],
        })
        .map_err(|_| arena_full())?;
    let output_width = kept.len() + usize::from(tagged);
    let items = arena
        .alloc_slice_with(output_width, |_| SelectItem::Expr {
            expression: &Expr::Null,
            alias: None,
        })
        .map_err(|_| arena_full())?;
    for (slot, position) in kept.iter().enumerate() {
        let whens = arena
            .alloc_slice_with(rows.len(), |_| (&Expr::Null, &Expr::Null))
            .map_err(|_| arena_full())?;
        for (index, row) in rows.iter().enumerate() {
            let expression = if is_default(row[*position]) {
                &*arena
                    .alloc(Expr::BoundDefault {
                        expression: &Expr::Null,
                        type_oid: placeholder_types[slot],
                    })
                    .map_err(|_| arena_full())?
            } else {
                row[*position]
            };
            let number = &*arena
                .alloc(Expr::Int(index as i64))
                .map_err(|_| arena_full())?;
            whens[index] = (number, expression);
        }
        items[slot] = SelectItem::Expr {
            expression: arena
                .alloc(Expr::Case {
                    operand: Some(row_column),
                    whens,
                    otherwise: None,
                    origin: crate::sql::ast::CaseOrigin::Written,
                })
                .map_err(|_| arena_full())?,
            alias: None,
        };
    }
    if tagged {
        items[kept.len()] = SelectItem::Expr {
            expression: row_column,
            alias: None,
        };
    }
    let values_ref = TableRef {
        schema: None,
        table: "",
        alias: Some(VALUES_ALIAS),
        subquery: Some(&*values),
        func_args: None,
        func_argument_names: &[],
        func_variadic: false,
        func_source_position: crate::sql::ast::SourcePosition::UNKNOWN,
        rows_from: None,
        col_alias: Some(value_names),
        inheritance: crate::sql::ast::RelationInheritance::Descendants,
        sample: None,
        cte: None,
        with_ordinality: false,
        lateral: true,
        authorization_role: None,
        bound_table: None,
        view_access: None,
        join_members: None,
    };
    let joins = arena
        .alloc_slice_with(1, |_| Join {
            table: values_ref,
            kind: JoinKind::Comma,
            on: None,
            using: None,
            natural: false,
        })
        .map_err(|_| arena_full())?;
    let select = arena
        .alloc(rule_select(
            items,
            Some(FromClause {
                base: source,
                joins,
            }),
            condition,
        ))
        .map_err(|_| arena_full())?;
    Ok(Insert {
        columns,
        rows: &[],
        select: Some(&*select),
        values_defaults: if tagged { defaults.as_slice() } else { &[] },
        ..*insert
    })
}

fn attach_rule_source_to_set_tree<'a>(
    tree: &'a SetTree<'a>,
    source: TableRef<'a>,
    condition: Option<&'a Expr<'a>>,
    arena: &'a Arena,
) -> Result<&'a SetTree<'a>, SqlError> {
    let rewritten = match tree {
        SetTree::Select(select) => {
            let from = prepend_rule_source(source, select.from.as_ref(), arena)?;
            let select = arena
                .alloc(Select {
                    from: Some(from),
                    where_clause: and_where(condition, select.where_clause, arena)?,
                    ..**select
                })
                .map_err(|_| arena_full())?;
            SetTree::Select(&*select)
        }
        SetTree::Op {
            operator,
            all,
            left,
            right,
            syntax,
        } => SetTree::Op {
            operator: *operator,
            syntax: *syntax,
            all: *all,
            left: attach_rule_source_to_set_tree(left, source, condition, arena)?,
            right: attach_rule_source_to_set_tree(right, source, condition, arena)?,
        },
    };
    arena
        .alloc(rewritten)
        .map(|tree| &*tree)
        .map_err(|_| arena_full())
}

#[expect(
    clippy::too_many_arguments,
    reason = "stored statements require their captured catalog and evaluation context"
)]
fn expand_stored_statement_with_transition<'a>(
    statement: &'a Stmt<'a>,
    storage: &Storage,
    txid: u32,
    path: crate::storage::PathContext,
    dependencies: crate::storage::StoredQueryDependencyView<'_>,
    arena: &'a Arena,
    params: &[Datum<'a>],
    sequences: Option<&dyn SequenceAccess>,
    authorization_role: ViewReader,
    transition: Option<RuleTransition<'a>>,
) -> Result<&'a Stmt<'a>, SqlError> {
    let (outer_with, body) = match statement {
        Stmt::With { ctes, statement } => (*ctes, *statement),
        _ => (&[][..], statement),
    };
    let with = match body {
        Stmt::SetQuery(query) if outer_with.is_empty() => query.with,
        _ => outer_with,
    };
    with_exec_context(
        with,
        storage,
        txid,
        arena,
        ExecutionSubst::materializing(params, sequences),
        &[],
        &[],
        &[],
        &[],
        0,
        Some(path),
        Some(dependencies),
        authorization_role,
        None,
        |name| statement_references(body, name),
        |context| {
            let context = Subst {
                rule_transition: transition,
                ..context
            };
            let rebound = match body {
                Stmt::Insert(insert) => Stmt::Insert(subst_insert(insert, context, arena)?),
                Stmt::Update(update) => Stmt::Update(subst_update(update, context, arena)?),
                Stmt::Delete(delete) => Stmt::Delete(subst_delete(delete, context, arena)?),
                Stmt::Merge(merge) => Stmt::Merge(subst_merge(merge, context, arena)?),
                Stmt::Select(select) => Stmt::Select(*subst_select(select, context, arena)?),
                Stmt::SetQuery(query) => {
                    Stmt::SetQuery(*subst_set_query_body(query, context, arena)?)
                }
                Stmt::Notify { channel, payload } => Stmt::Notify {
                    channel,
                    payload: *payload,
                },
                _ => {
                    return Err(sql_err!(
                        sqlstate::FEATURE_NOT_SUPPORTED,
                        "stored statement kind is not executable in a rewrite rule"
                    ));
                }
            };
            Ok(&*arena.alloc(rebound).map_err(|_| arena_full())?)
        },
    )
}

fn subst_set_query_body<'a>(
    query: &'a SetQuery<'a>,
    context: Subst<'_, 'a, '_, '_>,
    arena: &'a Arena,
) -> Result<&'a SetQuery<'a>, SqlError> {
    let query = SetQuery {
        with: &[],
        body: subst_set_tree(query.body, context, arena)?,
        order_by: subst_order_by(query.order_by, context, arena)?,
        limit: opt_subst(query.limit, context, arena)?,
        offset: opt_subst(query.offset, context, arena)?,
        ..*query
    };
    arena
        .alloc(query)
        .map(|query| &*query)
        .map_err(|_| arena_full())
}

/// Binds executor-owned relations into a data-modifying statement. Unlike a
/// data-modifying CTE result, these relations are visible directly to the
/// statement's `FROM` or `USING` clause.
pub(crate) fn bind_dml_materialized_relations<'a>(
    statement: &'a Stmt<'a>,
    storage: &Storage,
    txid: u32,
    arena: &'a Arena,
    params: &[Datum<'a>],
    relations: &[(&'a str, &'a MaterializedCte<'a>)],
) -> Result<&'a Stmt<'a>, SqlError> {
    expand_dml_ctes_with_relations(
        statement,
        &[],
        storage,
        txid,
        arena,
        params,
        &[],
        relations,
        None,
    )
}

#[allow(clippy::too_many_arguments)]
fn expand_dml_ctes_with_relations<'a>(
    statement: &'a Stmt<'a>,
    with: &'a [Cte<'a>],
    storage: &Storage,
    txid: u32,
    arena: &'a Arena,
    params: &[Datum<'a>],
    dml_mats: &[(&'a str, &'a MaterializedCte<'a>)],
    relations: &[(&'a str, &'a MaterializedCte<'a>)],
    sequences: Option<&dyn SequenceAccess>,
) -> Result<&'a Stmt<'a>, SqlError> {
    with_exec_context(
        with,
        storage,
        txid,
        arena,
        ExecutionSubst::materializing(params, sequences),
        dml_mats,
        relations,
        &[],
        &[],
        0,
        None,
        None,
        ViewReader::Session,
        None,
        |name| statement_references(statement, name),
        |context| {
            let expanded = match statement {
                Stmt::Insert(insert) => Stmt::Insert(subst_insert(insert, context, arena)?),
                Stmt::Update(update) => Stmt::Update(subst_update(update, context, arena)?),
                Stmt::Delete(delete) => Stmt::Delete(subst_delete(delete, context, arena)?),
                Stmt::Merge(merge) => Stmt::Merge(subst_merge(merge, context, arena)?),
                _ => {
                    return Err(sql_err!(
                        sqlstate::FEATURE_NOT_SUPPORTED,
                        "WITH main statement must be INSERT, UPDATE, DELETE, or MERGE"
                    ));
                }
            };
            Ok(&*arena.alloc(expanded).map_err(|_| arena_full())?)
        },
    )
}

/// Places view-write restrictions before projection when PostgreSQL permits
/// it, retaining restrictions on volatile output values for the computed row.
#[expect(clippy::too_many_arguments, reason = "view predicate binding context")]
pub(crate) fn filter_view_rows<'a>(
    select: &'a Select<'a>,
    predicate: Option<&'a Expr<'a>>,
    definition: crate::storage::RelationDefinition<'_>,
    alias: Option<&str>,
    security_barrier: crate::storage::ViewSecurityBarrier,
    storage: &Storage,
    txid: u32,
    arena: &'a Arena,
) -> Result<(&'a Select<'a>, Option<&'a Expr<'a>>), SqlError> {
    if predicate.is_none()
        || select.limit.is_some()
        || select.offset.is_some()
        || !select.grouping_sets.is_empty()
    {
        return Ok((select, predicate));
    }
    if let Some(body) = select.set_body {
        let (body, residual) = filter_set_view_rows(
            body,
            predicate,
            definition,
            alias,
            security_barrier,
            storage,
            txid,
            arena,
        )?;
        return Ok((
            &*arena
                .alloc(Select {
                    set_body: Some(body),
                    ..*select
                })
                .map_err(|_| arena_full())?,
            residual,
        ));
    }
    let scope = select
        .from
        .as_ref()
        .map(|from| super::QueryScope::resolve_schema(storage, from, txid, arena, None))
        .transpose()?;
    let mut projections: ArenaList<'a, &'a Expr<'a>> = ArenaList::new(arena);
    let mut names: ArenaList<'a, &'a str> = ArenaList::new(arena);
    for (index, column) in definition.columns[..definition.n_columns]
        .iter()
        .enumerate()
    {
        let position = arena
            .alloc(Expr::Int((index + 1) as i64))
            .map_err(|_| arena_full())?;
        projections
            .push(super::resolve_position_target(
                position,
                select.items,
                scope.as_ref(),
                arena,
                storage,
                txid,
                "view output",
            )?)
            .map_err(|_| arena_full())?;
        names
            .push(
                arena
                    .alloc_str(column.name.as_str())
                    .map_err(|_| arena_full())?,
            )
            .map_err(|_| arena_full())?;
    }
    let projected = projections.as_slice();
    let source_columns = scope.as_ref().map(|scope| super::CatalogScopeCols {
        scope,
        outer: None,
        storage,
        txid,
        arena,
    });
    let empty_columns = crate::sql::exec::CatalogCols {
        definition: None,
        alias: None,
        output_aliases: &[],
        storage,
        txid,
        arena,
    };
    let source_columns: &dyn crate::sql::exec::ColTypeResolver = source_columns.as_ref().map_or(
        &empty_columns as &dyn crate::sql::exec::ColTypeResolver,
        |columns| columns,
    );
    let set_returning = |name: &str, arity| {
        super::is_builtin_set_routine(name) || storage.has_set_routine_candidate(name, arity, txid)
    };
    let windows = projected
        .iter()
        .any(|expression| super::expr_has_window(expression));
    let target_sets = projected
        .iter()
        .any(|expression| expression_contains_function(expression, &set_returning));
    let unsafe_volatile =
        select.distinct || !select.distinct_on.is_empty() || windows || target_sets;
    let safe_columns = arena
        .alloc_slice_with(definition.n_columns, |_| false)
        .map_err(|_| arena_full())?;
    for (index, expression) in projected.iter().enumerate() {
        safe_columns[index] = !super::predicate_safety::expression_properties(
            expression,
            source_columns,
            storage,
            txid,
            super::predicate_safety::AnalysisStage::ViewPushdown,
        )?
        .volatile
            && !expression_contains_function(expression, &set_returning)
            && !super::expr_has_window(expression);
        if !select.distinct_on.is_empty() {
            let mut in_distinct = false;
            for distinct in select.distinct_on {
                in_distinct |= *super::resolve_position_target(
                    distinct,
                    select.items,
                    scope.as_ref(),
                    arena,
                    storage,
                    txid,
                    "DISTINCT ON",
                )? == **expression;
            }
            safe_columns[index] &= in_distinct;
        }
        if windows {
            for output in projected {
                safe_columns[index] &= window_partitions_contain(output, expression);
            }
        }
    }
    let from = arena
        .alloc_str(alias.unwrap_or(definition.name.as_str()))
        .map_err(|_| arena_full())?;
    let from_schema = arena
        .alloc_str(definition.schema.as_str())
        .map_err(|_| arena_full())?;
    let qualifier = ViewQualifier {
        from,
        from_schema: alias.is_none().then_some(from_schema),
        to: "",
        to_schema: "",
        exposed_columns: names.as_slice(),
        base_columns: &[],
        projected_columns: Some(projected),
        projected_aliases: &[],
    };
    let context = Subst {
        ctes: &[],
        materialized: &[],
        storage,
        txid,
        depth: 0,
        path: None,
        dependencies: None,
        authorization_role: ViewReader::Session,
        qualifier: Some(qualifier),
        rule_transition: None,
        recursive_state: None,
        execution: None,
    };
    let outer_columns = crate::sql::exec::CatalogCols {
        definition: Some(definition),
        alias,
        output_aliases: &[],
        storage,
        txid,
        arena,
    };
    let (pushed, residual) = split_view_predicate(
        predicate,
        context,
        ViewPredicateSafety {
            safe_columns: &safe_columns[..definition.n_columns],
            unsafe_volatile,
            security_barrier: security_barrier == crate::storage::ViewSecurityBarrier::Enabled,
            columns: &outer_columns,
        },
        arena,
    )?;
    let mut rewritten = *select;
    let mut grouped = !select.group_by.is_empty() || select.having.is_some();
    if !grouped {
        for expression in projected {
            if super::expr_has_aggregate(expression, storage, txid, source_columns)? {
                grouped = true;
                break;
            }
        }
    }
    if grouped {
        rewritten.having = and_where(select.having, pushed, arena)?;
    } else {
        rewritten.where_clause = and_where(select.where_clause, pushed, arena)?;
    }
    if let Some(mut from) = rewritten.from
        && from.joins.is_empty()
        && let Some(body) = from.base.subquery
        && let Some(definition) = scope.as_ref().and_then(|scope| scope.defs.get(0))
    {
        let barrier = from
            .base
            .view_access
            .map_or(crate::storage::ViewSecurityBarrier::Default, |slot| {
                storage.view(slot as usize).security_barrier_for(txid)
            });
        let (body, remaining) = filter_view_rows(
            body,
            rewritten.where_clause,
            definition,
            Some(from.base.alias.unwrap_or(from.base.table)),
            barrier,
            storage,
            txid,
            arena,
        )?;
        from.base.subquery = Some(body);
        rewritten.from = Some(from);
        rewritten.where_clause = remaining;
    }
    Ok((
        &*arena.alloc(rewritten).map_err(|_| arena_full())?,
        residual,
    ))
}

#[expect(
    clippy::too_many_arguments,
    reason = "set-operation view predicate binding context"
)]
fn filter_set_view_rows<'a>(
    body: &'a SetTree<'a>,
    predicate: Option<&'a Expr<'a>>,
    definition: crate::storage::RelationDefinition<'_>,
    alias: Option<&str>,
    security_barrier: crate::storage::ViewSecurityBarrier,
    storage: &Storage,
    txid: u32,
    arena: &'a Arena,
) -> Result<(&'a SetTree<'a>, Option<&'a Expr<'a>>), SqlError> {
    let Some(predicate) = predicate else {
        return Ok((body, None));
    };
    if let Expr::Binary {
        operator: super::super::ast::BinaryOp::And,
        left,
        right,
        ..
    } = predicate
    {
        let (body, left_keep) = filter_set_view_rows(
            body,
            Some(left),
            definition,
            alias,
            security_barrier,
            storage,
            txid,
            arena,
        )?;
        let (body, right_keep) = filter_set_view_rows(
            body,
            Some(right),
            definition,
            alias,
            security_barrier,
            storage,
            txid,
            arena,
        )?;
        return Ok((body, and_where(left_keep, right_keep, arena)?));
    }
    match filter_set_view_conjunct(
        body,
        predicate,
        definition,
        alias,
        security_barrier,
        storage,
        txid,
        arena,
    )? {
        Some(filtered) => Ok((filtered, None)),
        None => Ok((body, Some(predicate))),
    }
}

#[expect(
    clippy::too_many_arguments,
    reason = "set-operation view predicate binding context"
)]
fn filter_set_view_conjunct<'a>(
    body: &'a SetTree<'a>,
    predicate: &'a Expr<'a>,
    definition: crate::storage::RelationDefinition<'_>,
    alias: Option<&str>,
    security_barrier: crate::storage::ViewSecurityBarrier,
    storage: &Storage,
    txid: u32,
    arena: &'a Arena,
) -> Result<Option<&'a SetTree<'a>>, SqlError> {
    let rewritten = match body {
        SetTree::Select(select) => {
            let columns = arena
                .alloc_slice_with(definition.n_columns, |_| ColDesc::new("", 0, 0))
                .map_err(|_| arena_full())?;
            let count = super::describe_select(
                select,
                storage,
                txid,
                arena,
                columns,
                crate::sql::exec::DescriptionTarget::Sql,
                None,
            )?;
            if count != definition.n_columns {
                return Err(sql_err!(
                    sqlstate::INTERNAL_ERROR,
                    "view set-operation row shape changed"
                ));
            }
            let mut names: ArenaList<'a, &'a str> = ArenaList::new(arena);
            let matching_types = arena
                .alloc_slice_with(count, |_| false)
                .map_err(|_| arena_full())?;
            for (index, column) in columns[..count].iter().enumerate() {
                names
                    .push(
                        arena
                            .alloc_str(definition.columns[index].name.as_str())
                            .map_err(|_| arena_full())?,
                    )
                    .map_err(|_| arena_full())?;
                matching_types[index] = column.type_oid
                    == storage
                        .declared_column_type(&definition.columns[index], txid)?
                        .catalog_oid();
            }
            let from = alias.unwrap_or(definition.name.as_str());
            let qualifier = ViewQualifier {
                from,
                from_schema: alias.is_none().then_some(definition.schema.as_str()),
                to: "",
                to_schema: "",
                exposed_columns: names.as_slice(),
                base_columns: &[],
                projected_columns: None,
                projected_aliases: &[],
            };
            if !view_predicate_columns_safe(predicate, qualifier, &matching_types[..count]) {
                return Ok(None);
            }
            let (select, remaining) = filter_view_rows(
                select,
                Some(predicate),
                definition,
                alias,
                security_barrier,
                storage,
                txid,
                arena,
            )?;
            if remaining.is_some() {
                return Ok(None);
            }
            SetTree::Select(select)
        }
        SetTree::Op {
            operator: SetOp::Except,
            ..
        } => return Ok(None),
        SetTree::Op {
            operator,
            all,
            left,
            right,
            syntax,
        } => {
            let Some(left) = filter_set_view_conjunct(
                left,
                predicate,
                definition,
                alias,
                security_barrier,
                storage,
                txid,
                arena,
            )?
            else {
                return Ok(None);
            };
            let Some(right) = filter_set_view_conjunct(
                right,
                predicate,
                definition,
                alias,
                security_barrier,
                storage,
                txid,
                arena,
            )?
            else {
                return Ok(None);
            };
            SetTree::Op {
                operator: *operator,
                syntax: *syntax,
                all: *all,
                left,
                right,
            }
        }
    };
    Ok(Some(&*arena.alloc(rewritten).map_err(|_| arena_full())?))
}

fn window_partitions_contain(expression: &Expr<'_>, column: &Expr<'_>) -> bool {
    if let Expr::Call {
        over: Some(window), ..
    } = expression
        && !window
            .partition_by
            .iter()
            .any(|partition| **partition == *column)
    {
        return false;
    }
    let mut safe = true;
    super::walk_children(expression, &mut |child| {
        safe &= window_partitions_contain(child, column);
        Ok(())
    })
    .expect("expression visitor cannot fail");
    safe
}

fn view_predicate_columns_safe(
    expression: &Expr<'_>,
    qualifier: ViewQualifier<'_>,
    safe_columns: &[bool],
) -> bool {
    let column = match expression {
        Expr::Column {
            qualifier: written,
            name,
        } => {
            if written.is_some_and(|written| written != qualifier.from) {
                return false;
            }
            Some(*name)
        }
        Expr::SchemaColumn {
            schema,
            table,
            name,
        } => {
            if *table != qualifier.from || Some(*schema) != qualifier.from_schema {
                return false;
            }
            Some(*name)
        }
        Expr::WholeRow(_) => return false,
        _ => None,
    };
    if let Some(column) = column {
        return qualifier
            .exposed_columns
            .iter()
            .position(|known| *known == column)
            .is_some_and(|index| safe_columns[index]);
    }
    let mut safe = true;
    super::walk_children(expression, &mut |child| {
        safe &= view_predicate_columns_safe(child, qualifier, safe_columns);
        Ok(())
    })
    .expect("expression visitor cannot fail");
    safe
}

#[derive(Clone, Copy)]
struct ViewPredicateSafety<'a> {
    safe_columns: &'a [bool],
    unsafe_volatile: bool,
    security_barrier: bool,
    columns: &'a dyn crate::sql::exec::ColTypeResolver,
}

fn split_view_predicate<'a>(
    predicate: Option<&'a Expr<'a>>,
    context: Subst<'_, 'a, '_, '_>,
    safety: ViewPredicateSafety<'_>,
    arena: &'a Arena,
) -> Result<(Option<&'a Expr<'a>>, Option<&'a Expr<'a>>), SqlError> {
    let Some(predicate) = predicate else {
        return Ok((None, None));
    };
    if let Expr::Binary {
        operator: super::super::ast::BinaryOp::And,
        left,
        right,
        ..
    } = predicate
    {
        let (left_push, left_keep) = split_view_predicate(Some(left), context, safety, arena)?;
        let (right_push, right_keep) = split_view_predicate(Some(right), context, safety, arena)?;
        return Ok((
            and_where(left_push, right_push, arena)?,
            and_where(left_keep, right_keep, arena)?,
        ));
    }
    if predicate.contains_subquery()
        || super::expr_has_aggregate(predicate, context.storage, context.txid, safety.columns)?
        || super::expr_has_window(predicate)
        || !view_predicate_columns_safe(
            predicate,
            context.qualifier.expect("view predicate qualifier"),
            safety.safe_columns,
        )
    {
        return Ok((None, Some(predicate)));
    }
    let properties = super::predicate_safety::expression_properties(
        predicate,
        safety.columns,
        context.storage,
        context.txid,
        super::predicate_safety::AnalysisStage::ViewPushdown,
    )?;
    if (safety.unsafe_volatile && properties.volatile)
        || (safety.security_barrier && properties.leaks_columns)
    {
        return Ok((None, Some(predicate)));
    }
    Ok((Some(subst_expr(predicate, context, arena)?), None))
}

/// Replaces the columns `names` of the relation `from` with `projected`, as
/// a view level's expressions read the composed columns of the level below.
#[allow(clippy::too_many_arguments)]
pub(super) fn substitute_relation_columns<'a>(
    expression: &'a Expr<'a>,
    from: &'a str,
    from_schema: Option<&'a str>,
    names: &'a [&'a str],
    projected: &'a [&'a Expr<'a>],
    storage: &Storage,
    txid: u32,
    arena: &'a Arena,
) -> Result<&'a Expr<'a>, SqlError> {
    let context = Subst {
        ctes: &[],
        materialized: &[],
        storage,
        txid,
        depth: 0,
        path: None,
        dependencies: None,
        authorization_role: ViewReader::Session,
        qualifier: Some(ViewQualifier {
            from,
            from_schema,
            to: from,
            to_schema: from_schema.unwrap_or(""),
            exposed_columns: names,
            base_columns: names,
            projected_columns: Some(projected),
            projected_aliases: &[],
        }),
        rule_transition: None,
        recursive_state: None,
        execution: None,
    };
    subst_expr(expression, context, arena)
}

/// Rebinds a data-modifying statement from an auto-updatable view to its base
/// table: written columns are checked and mapped level by level, view
/// columns read in expressions and `RETURNING` become their base-table
/// expressions, and every level's filter joins the statement's `WHERE`.
pub fn rewrite_view_dml<'a>(
    statement: &'a Stmt<'a>,
    view_name: &'a str,
    view: &super::UpdatableView<'a>,
    storage: &Storage,
    txid: u32,
    arena: &'a Arena,
) -> Result<super::ViewRewrite<'a>, SqlError> {
    use super::ViewWrite;
    let exposed_columns = view.columns();
    let (alias, view_schema) = match statement {
        Stmt::Insert(insert) => (insert.alias, insert.table.schema),
        Stmt::Update(update) => (update.alias, update.table.schema),
        Stmt::Delete(delete) => (delete.alias, delete.table.schema),
        Stmt::Merge(merge) => (merge.target_alias, merge.target.schema),
        _ => {
            return Err(sql_err!(
                sqlstate::INTERNAL_ERROR,
                "view rewrite expected INSERT, UPDATE, DELETE, or MERGE"
            ));
        }
    };
    let visible_name = alias.unwrap_or(view_name);
    let target = alias.unwrap_or(view.base.name);
    // The written columns, checked as PostgreSQL's rewriter checks them.
    let mut written = [false; MAX_PROJ];
    let mut mark = |name: &str| -> Result<(), SqlError> {
        require_view_column(view_name, exposed_columns, name)?;
        if let Some(index) = exposed_columns.iter().position(|column| *column == name) {
            written[index] = true;
        }
        Ok(())
    };
    let write = match statement {
        Stmt::Insert(insert) => {
            if insert.columns.is_empty() {
                let width = super::dependencies::insert_source_width(insert, storage, txid, arena)?;
                for column in &exposed_columns[..width.min(exposed_columns.len())] {
                    mark(column)?;
                }
            } else {
                for column in insert.columns {
                    mark(column)?;
                }
            }
            if let Some(assignments) = insert.on_conflict.and_then(|conflict| conflict.update) {
                for (column, _) in assignments {
                    mark(column)?;
                }
            }
            ViewWrite::Insert
        }
        Stmt::Update(update) => {
            for (column, _) in update.assignments {
                mark(column)?;
            }
            ViewWrite::Update
        }
        Stmt::Delete(_) => ViewWrite::Delete,
        Stmt::Merge(merge) => {
            let mut write = ViewWrite::MergeDelete;
            for when in merge.whens {
                match when.action() {
                    crate::sql::ast::MergeActionRef::Update(assignments) => {
                        for (column, _) in assignments {
                            mark(column)?;
                        }
                        if write == ViewWrite::MergeDelete {
                            write = ViewWrite::MergeUpdate;
                        }
                    }
                    crate::sql::ast::MergeActionRef::Insert {
                        columns, values, ..
                    } => {
                        if columns.is_empty() {
                            for column in
                                &exposed_columns[..values.len().min(exposed_columns.len())]
                            {
                                mark(column)?;
                            }
                        } else {
                            for column in columns {
                                mark(column)?;
                            }
                        }
                        write = ViewWrite::MergeInsert;
                    }
                    _ => {}
                }
            }
            write
        }
        _ => unreachable!("statement kind checked above"),
    };
    let written = view.check_written(&written[..exposed_columns.len()], write)?;
    // Names of written view columns map to their base columns; any other
    // name keeps its spelling and fails where the statement binds it.
    let mapped = arena
        .alloc_slice_with(exposed_columns.len(), |index| {
            let base = view.base_column(index);
            if base.is_empty() {
                exposed_columns[index]
            } else {
                base
            }
        })
        .map_err(|_| arena_full())?;
    let projected = view.projected_over(target, storage, txid, arena)?;
    let context = Subst {
        ctes: &[],
        materialized: &[],
        storage,
        txid,
        depth: 0,
        path: None,
        dependencies: None,
        authorization_role: ViewReader::Session,
        qualifier: Some(ViewQualifier {
            from: visible_name,
            from_schema: alias.is_none().then_some(view_schema).flatten(),
            to: target,
            to_schema: view.base.schema.unwrap_or(""),
            exposed_columns,
            base_columns: mapped,
            projected_columns: Some(projected),
            projected_aliases: &[],
        }),
        rule_transition: None,
        recursive_state: None,
        execution: None,
    };
    // `*` also covers the FROM items of `UPDATE ... FROM` and `DELETE ... USING`,
    // and MERGE's source.
    let merge_source;
    let (returning, from) = match statement {
        Stmt::Update(update) => (update.returning, update.from),
        Stmt::Delete(delete) => (delete.returning, delete.using),
        Stmt::Merge(merge) => {
            merge_source = FromClause {
                base: merge.source,
                joins: &[],
            };
            (merge.returning, Some(&merge_source))
        }
        _ => (Returning::NONE, None),
    };
    let from_star = match from {
        Some(from)
            if returning
                .items
                .iter()
                .any(|item| matches!(item, SelectItem::Wildcard(_))) =>
        {
            let from = expand_from_schema(from, storage, txid, arena)?;
            let scope = super::QueryScope::resolve_schema(storage, from, txid, arena, None)?;
            let star = arena
                .alloc_slice_with(scope.star_columns(), |_| (&Expr::Null as &Expr, ""))
                .map_err(|_| arena_full())?;
            for (index, slot) in star.iter_mut().enumerate() {
                let entry = scope.star_entry(index);
                *slot = (
                    scope.copied_output_expression(entry, arena)?,
                    arena
                        .alloc_str(scope.output_name(entry))
                        .map_err(|_| arena_full())?,
                );
            }
            &*star
        }
        _ => &[],
    };
    // MERGE's `*` lists its source before its target.
    let source_first = matches!(statement, Stmt::Merge(_));
    let expanded_returning = |items| {
        expand_view_returning(
            items,
            visible_name,
            exposed_columns,
            from_star,
            source_first,
            arena,
        )
    };
    let base = view.base;
    let rewritten = match statement {
        Stmt::Insert(insert) => {
            let columns: &'a [&'a str] = if insert.columns.is_empty() {
                let width = super::dependencies::insert_source_width(insert, storage, txid, arena)?;
                // A wider source fails as an INSERT with too many values.
                if width > exposed_columns.len() {
                    mapped
                } else {
                    &mapped[..width]
                }
            } else {
                map_view_columns(view_name, exposed_columns, mapped, insert.columns, arena)?
            };
            let adjusted = Insert {
                columns,
                returning: Returning {
                    items: expanded_returning(insert.returning.items)?,
                    ..insert.returning
                },
                on_conflict: match insert.on_conflict {
                    Some(conflict) => Some(OnConflict {
                        update: match conflict.update {
                            Some(assignments) => Some(map_view_assignments(
                                view_name,
                                exposed_columns,
                                mapped,
                                assignments,
                                arena,
                            )?),
                            None => None,
                        },
                        ..conflict
                    }),
                    None => None,
                },
                ..*insert
            };
            let adjusted = arena.alloc(adjusted).map_err(|_| arena_full())?;
            let substituted = subst_insert(&*adjusted, context, arena)?;
            Stmt::Insert(Insert {
                table: base,
                ..substituted
            })
        }
        Stmt::Update(update) => {
            let assignments = map_view_assignments(
                view_name,
                exposed_columns,
                mapped,
                update.assignments,
                arena,
            )?;
            let adjusted = Update {
                assignments,
                returning: Returning {
                    items: expanded_returning(update.returning.items)?,
                    ..update.returning
                },
                ..*update
            };
            let adjusted = arena.alloc(adjusted).map_err(|_| arena_full())?;
            let substituted = subst_update(&*adjusted, context, arena)?;
            let where_clause =
                view.filter_over(target, substituted.where_clause, storage, txid, arena)?;
            Stmt::Update(Update {
                table: base,
                inheritance: view.base_inheritance,
                where_clause,
                ..substituted
            })
        }
        Stmt::Delete(delete) => {
            let adjusted = Delete {
                returning: Returning {
                    items: expanded_returning(delete.returning.items)?,
                    ..delete.returning
                },
                ..*delete
            };
            let adjusted = arena.alloc(adjusted).map_err(|_| arena_full())?;
            let substituted = subst_delete(&*adjusted, context, arena)?;
            let where_clause =
                view.filter_over(target, substituted.where_clause, storage, txid, arena)?;
            Stmt::Delete(Delete {
                table: base,
                inheritance: view.base_inheritance,
                where_clause,
                ..substituted
            })
        }
        Stmt::Merge(merge) => {
            let mut whens: ArenaList<'a, MergeWhen<'a>> = ArenaList::new(arena);
            let map_target = |action: MergeTargetAction<'a>| -> Result<_, SqlError> {
                Ok(match action {
                    MergeTargetAction::Update(assignments) => {
                        MergeTargetAction::Update(map_view_assignments(
                            view_name,
                            exposed_columns,
                            mapped,
                            assignments,
                            arena,
                        )?)
                    }
                    other => other,
                })
            };
            for when in merge.whens {
                whens
                    .push(match *when {
                        MergeWhen::Matched { cond, action } => MergeWhen::Matched {
                            cond,
                            action: map_target(action)?,
                        },
                        MergeWhen::NotMatchedBySource { cond, action } => {
                            MergeWhen::NotMatchedBySource {
                                cond,
                                action: map_target(action)?,
                            }
                        }
                        MergeWhen::NotMatchedByTarget { cond, action } => {
                            MergeWhen::NotMatchedByTarget {
                                cond,
                                action: match action {
                                    MergeSourceAction::Insert {
                                        columns,
                                        values,
                                        default_values,
                                        overriding,
                                    } => MergeSourceAction::Insert {
                                        columns: if columns.is_empty() {
                                            if values.len() > exposed_columns.len() {
                                                mapped
                                            } else {
                                                &mapped[..values.len()]
                                            }
                                        } else {
                                            map_view_columns(
                                                view_name,
                                                exposed_columns,
                                                mapped,
                                                columns,
                                                arena,
                                            )?
                                        },
                                        values,
                                        default_values,
                                        overriding,
                                    },
                                    MergeSourceAction::DoNothing => MergeSourceAction::DoNothing,
                                },
                            }
                        }
                    })
                    .map_err(|_| arena_full())?;
            }
            let adjusted = Merge {
                whens: whens.as_slice(),
                returning: Returning {
                    items: expanded_returning(merge.returning.items)?,
                    ..merge.returning
                },
                ..*merge
            };
            let adjusted = arena.alloc(adjusted).map_err(|_| arena_full())?;
            let substituted = subst_merge(&*adjusted, context, arena)?;
            Stmt::Merge(Merge {
                target: base,
                target_inheritance: view.base_inheritance,
                target_alias: Some(target),
                ..substituted
            })
        }
        _ => unreachable!("statement kind checked above"),
    };
    Ok(super::ViewRewrite {
        statement: arena.alloc(rewritten).map_err(|_| arena_full())?,
        written,
    })
}

fn view_base_column<'a>(exposed: &[&'a str], base: &[&'a str], name: &'a str) -> &'a str {
    exposed
        .iter()
        .position(|candidate| *candidate == name)
        .map_or(name, |index| base[index])
}

fn map_view_columns<'a>(
    view_name: &str,
    exposed: &'a [&'a str],
    base: &'a [&'a str],
    source: &'a [&'a str],
    arena: &'a Arena,
) -> Result<&'a [&'a str], SqlError> {
    let mut columns = [""; crate::storage::MAX_COLUMNS];
    if source.len() > columns.len() {
        return Err(sql_err!(
            sqlstate::TOO_MANY_COLUMNS,
            "too many target columns"
        ));
    }
    for (target, name) in columns.iter_mut().zip(source) {
        require_view_column(view_name, exposed, name)?;
        *target = view_base_column(exposed, base, name);
    }
    arena
        .alloc_slice_copy(&columns[..source.len()])
        .map(|columns| &*columns)
        .map_err(|_| arena_full())
}

fn map_view_assignments<'a>(
    view_name: &str,
    exposed: &'a [&'a str],
    base: &'a [&'a str],
    source: &'a [(&'a str, &'a Expr<'a>)],
    arena: &'a Arena,
) -> Result<&'a [(&'a str, &'a Expr<'a>)], SqlError> {
    let mut assignments = [("", &Expr::Null); crate::storage::MAX_COLUMNS];
    if source.len() > assignments.len() {
        return Err(sql_err!(sqlstate::TOO_MANY_COLUMNS, "too many assignments"));
    }
    for (target, (name, expression)) in assignments.iter_mut().zip(source) {
        require_view_column(view_name, exposed, name)?;
        *target = (view_base_column(exposed, base, name), expression);
    }
    arena
        .alloc_slice_copy(&assignments[..source.len()])
        .map(|assignments| &*assignments)
        .map_err(|_| arena_full())
}

fn require_view_column(
    view_name: &str,
    exposed_columns: &[&str],
    column: &str,
) -> Result<(), SqlError> {
    if exposed_columns.contains(&column) {
        return Ok(());
    }
    Err(crate::sql::error_position::at_reference(
        sql_err!(
            sqlstate::UNDEFINED_COLUMN,
            "column \"{}\" of relation \"{}\" does not exist",
            column,
            view_name
        ),
        column,
    ))
}

fn expand_view_returning<'a>(
    items: &'a [SelectItem<'a>],
    view_name: &'a str,
    exposed_columns: &'a [&'a str],
    from_star: &[(&'a Expr<'a>, &'a str)],
    source_first: bool,
    arena: &'a Arena,
) -> Result<&'a [SelectItem<'a>], SqlError> {
    let extra = items.iter().fold(0usize, |count, item| {
        count
            + match item {
                SelectItem::Wildcard(_) => {
                    (exposed_columns.len() + from_star.len()).saturating_sub(1)
                }
                SelectItem::TableWildcard(qualifier) if *qualifier == view_name => {
                    exposed_columns.len().saturating_sub(1)
                }
                _ => 0,
            }
    });
    let output_len = items.len().saturating_add(extra);
    if output_len > MAX_PROJ {
        return Err(sql_err!(
            sqlstate::TOO_MANY_COLUMNS,
            "RETURNING list is too wide"
        ));
    }
    let mut output = [SelectItem::Wildcard(crate::sql::ast::SourcePosition::UNKNOWN); MAX_PROJ];
    let mut count = 0usize;
    for item in items {
        let wildcard = matches!(item, SelectItem::Wildcard(_));
        if wildcard
            || matches!(item, SelectItem::TableWildcard(qualifier) if *qualifier == view_name)
        {
            if wildcard && source_first {
                for &(expression, name) in from_star {
                    output[count] = SelectItem::Expr {
                        expression,
                        alias: Some(name),
                    };
                    count += 1;
                }
            }
            for column in exposed_columns {
                let expression = arena
                    .alloc(Expr::Column {
                        qualifier: Some(view_name),
                        name: column,
                    })
                    .map_err(|_| arena_full())?;
                output[count] = SelectItem::Expr {
                    expression: &*expression,
                    alias: Some(column),
                };
                count += 1;
            }
            if wildcard && !source_first {
                for &(expression, name) in from_star {
                    output[count] = SelectItem::Expr {
                        expression,
                        alias: Some(name),
                    };
                    count += 1;
                }
            }
        } else {
            // Binding a view column to storage must preserve its output name.
            output[count] = match item {
                SelectItem::Expr { expression, alias } => SelectItem::Expr {
                    expression,
                    alias: Some(
                        alias.unwrap_or_else(|| crate::sql::exec::derived_name(expression)),
                    ),
                },
                _ => *item,
            };
            count += 1;
        }
    }
    arena
        .alloc_slice_copy(&output[..count])
        .map(|items| &*items)
        .map_err(|_| arena_full())
}

#[allow(clippy::too_many_arguments)]
fn with_exec_context<'a, 's, 'e, R>(
    with: &'a [Cte<'a>],
    storage: &'s Storage,
    txid: u32,
    arena: &'a Arena,
    execution: ExecutionSubst<'e, 'a>,
    dml_mats: &[(&'a str, &'a MaterializedCte<'a>)],
    relations: &[(&'a str, &'a MaterializedCte<'a>)],
    inherited_ctes: &CteBindings<'a>,
    inherited_materialized: &[(&'a str, &'a MaterializedCte<'a>)],
    depth: u32,
    path: Option<crate::storage::PathContext>,
    dependencies: Option<crate::storage::StoredQueryDependencyView<'s>>,
    authorization_role: ViewReader,
    enclosing: Option<&dyn crate::sql::eval::ColumnLookup<'a>>,
    root_references: impl Fn(&str) -> usize,
    build: impl for<'c> FnOnce(Subst<'c, 'a, 's, 'e>) -> Result<R, SqlError>,
) -> Result<R, SqlError> {
    let mut resolved = ArenaList::new(arena);
    let mut materialized = ArenaList::new(arena);
    for &(name, relation) in relations {
        if materialized
            .as_slice()
            .iter()
            .any(|(existing, _)| *existing == name)
        {
            return Err(sql_err!(
                sqlstate::DUPLICATE_ALIAS,
                "WITH query name \"{}\" specified more than once",
                name
            ));
        }
        materialized
            .push((name, relation))
            .map_err(|_| arena_full())?;
    }
    for &cte_index in cte_order(with, arena)? {
        let cte = &with[cte_index];
        if with[..cte_index].iter().any(|prior| prior.name == cte.name) {
            return Err(sql_err!(
                sqlstate::DUPLICATE_ALIAS,
                "WITH query name \"{}\" specified more than once",
                cte.name
            ));
        }
        let mut scoped_ctes = ArenaList::new(arena);
        let mut scoped_materialized = ArenaList::new(arena);
        fill_scoped_bindings(
            with,
            resolved.as_slice(),
            inherited_ctes,
            materialized.as_slice(),
            inherited_materialized,
            &mut scoped_ctes,
            &mut scoped_materialized,
        )?;
        let context = Subst {
            ctes: scoped_ctes.as_slice(),
            materialized: scoped_materialized.as_slice(),
            storage,
            txid,
            depth,
            path,
            dependencies,
            authorization_role,
            qualifier: None,
            rule_transition: None,
            recursive_state: None,
            execution: Some(execution),
        };
        let self_references = select_references(cte.query, cte.name);
        if (cte.search.is_some() || cte.cycle.is_some())
            && (!cte.recursive || self_references == 0 || cte.dml.is_some())
        {
            return Err(sql_err!(
                sqlstate::SYNTAX_ERROR,
                "WITH query \"{}\" is not a recursive SELECT query",
                cte.name
            ));
        }
        // Under RECURSIVE an earlier CTE may read a later one too.
        let references = root_references(cte.name)
            + with
                .iter()
                .enumerate()
                .filter(|(index, _)| *index > cte_index || cte.recursive && *index < cte_index)
                .map(|(_, other)| cte_references(other, cte.name))
                .sum::<usize>();
        if cte.dml.is_some() {
            if execution.refuses_evaluate_once() {
                return Err(execution.refuse());
            }
            match dml_mats.iter().find(|(name, _)| *name == cte.name) {
                Some(&(_, materialized_cte)) => materialized
                    .push((cte.name, materialized_cte))
                    .map_err(|_| arena_full())?,
                // A query nothing reads runs after the main query.
                None if root_references(cte.name) == 0
                    && with
                        .iter()
                        .all(|other| cte_references(other, cte.name) == 0) => {}
                None => {
                    return Err(sql_err!(
                        sqlstate::INTERNAL_ERROR,
                        "data-modifying CTE \"{}\" was not materialized",
                        cte.name
                    ));
                }
            }
        } else if cte.recursive && self_references > 0 {
            // PostgreSQL never plans a CTE nothing references, so an
            // unreferenced recursive query does not run.
            if references == 0 {
                continue;
            }
            if execution.refuses_evaluate_once() {
                return Err(execution.refuse());
            }
            let recursive = materialize_recursive(
                cte,
                context,
                storage,
                txid,
                arena,
                execution.params,
                execution.sequences,
            )?;
            materialized
                .push((cte.name, recursive))
                .map_err(|_| arena_full())?;
        } else {
            let query = subst_select(cte.query, context, arena)?;
            let volatile = select_contains_volatile(query, storage, txid);
            let requires_evaluate_once = match cte.materialization {
                CteMaterialization::Default => references > 1 || volatile,
                CteMaterialization::Materialized => true,
                CteMaterialization::NotMaterialized => volatile,
            };
            if references > 0 && requires_evaluate_once {
                if execution.refuses_evaluate_once() {
                    return Err(execution.refuse());
                }
                let relation = materialize_query_cte(
                    cte.name,
                    cte.columns,
                    query,
                    storage,
                    txid,
                    arena,
                    enclosing,
                )?;
                materialized
                    .push((cte.name, relation))
                    .map_err(|_| arena_full())?;
            } else {
                check_inlined_columns(cte, query, storage, txid, arena)?;
                resolved
                    .push((cte.name, query, cte.columns))
                    .map_err(|_| arena_full())?;
            }
        }
    }
    let mut scoped_ctes = ArenaList::new(arena);
    let mut scoped_materialized = ArenaList::new(arena);
    fill_scoped_bindings(
        with,
        resolved.as_slice(),
        inherited_ctes,
        materialized.as_slice(),
        inherited_materialized,
        &mut scoped_ctes,
        &mut scoped_materialized,
    )?;
    let context = Subst {
        ctes: scoped_ctes.as_slice(),
        materialized: scoped_materialized.as_slice(),
        storage,
        txid,
        depth,
        path,
        dependencies,
        authorization_role,
        qualifier: None,
        rule_transition: None,
        recursive_state: None,
        execution: Some(execution),
    };
    build(context)
}

/// PostgreSQL analyses a CTE's column list against its query wherever it
/// is read: an inlined CTE reports a list longer than its output as the WITH
/// query's, at its name.
fn check_inlined_columns<'a>(
    cte: &Cte<'a>,
    query: &'a Select<'a>,
    storage: &Storage,
    txid: u32,
    arena: &'a Arena,
) -> Result<(), SqlError> {
    if cte.columns.is_empty() {
        return Ok(());
    }
    let mark = arena.mark();
    let described = arena
        .alloc(SetTree::Select(query))
        .map_err(|_| arena_full())
        .and_then(|tree| {
            let mut described = [ColDesc::new("", 0, 0); MAX_PROJ];
            describe_set_body(
                storage,
                tree,
                txid,
                &mut described,
                arena,
                None,
                crate::sql::exec::DescriptionTarget::Sql,
                None,
            )
        });
    // SAFETY: only the column count leaves the description.
    unsafe { arena.rewind_to(mark) };
    let available = described?;
    if cte.columns.len() > available {
        return Err(crate::sql::error_position::at_reference(
            sql_err!(
                sqlstate::INVALID_COLUMN_REFERENCE,
                "WITH query \"{}\" has {} columns available but {} columns specified",
                cte.name,
                available,
                cte.columns.len()
            ),
            cte.name,
        ));
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn fill_scoped_bindings<'a>(
    with: &[Cte<'a>],
    resolved: &[(&'a str, &'a Select<'a>, &'a [&'a str])],
    inherited_ctes: &CteBindings<'a>,
    materialized: &[(&'a str, &'a MaterializedCte<'a>)],
    inherited_materialized: &[(&'a str, &'a MaterializedCte<'a>)],
    scoped_ctes: &mut ArenaList<'a, (&'a str, &'a Select<'a>, &'a [&'a str])>,
    scoped_materialized: &mut ArenaList<'a, (&'a str, &'a MaterializedCte<'a>)>,
) -> Result<(), SqlError> {
    for binding in resolved.iter().chain(
        inherited_ctes
            .iter()
            .filter(|binding| !with.iter().any(|local| local.name == binding.0)),
    ) {
        scoped_ctes.push(*binding).map_err(|_| arena_full())?;
    }

    for binding in materialized.iter().chain(
        inherited_materialized
            .iter()
            .filter(|binding| !with.iter().any(|local| local.name == binding.0)),
    ) {
        scoped_materialized
            .push(*binding)
            .map_err(|_| arena_full())?;
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn materialize_query_cte<'a>(
    name: &str,
    aliases: &'a [&'a str],
    query: &'a Select<'a>,
    storage: &Storage,
    txid: u32,
    arena: &'a Arena,
    enclosing: Option<&dyn crate::sql::eval::ColumnLookup<'a>>,
) -> Result<&'a MaterializedCte<'a>, SqlError> {
    let tree = arena
        .alloc(SetTree::Select(query))
        .map_err(|_| arena_full())?;
    let mut described = [ColDesc::new("", 0, 0); MAX_PROJ];
    let variables = enclosing.map(crate::sql::eval::RoutineVariables);
    let ncols = describe_set_body(
        storage,
        tree,
        txid,
        &mut described,
        arena,
        None,
        crate::sql::exec::DescriptionTarget::Sql,
        variables
            .as_ref()
            .map(|row| row as &dyn crate::sql::eval::ColumnLookup<'_>),
    )?;
    if aliases.len() > ncols {
        return Err(sql_err!(
            sqlstate::INVALID_COLUMN_REFERENCE,
            "WITH query \"{}\" has {} columns available but {} columns specified",
            name,
            ncols,
            aliases.len()
        ));
    }
    let column_names = {
        let mut names = [""; MAX_PROJ];
        for (index, output) in names.iter_mut().enumerate().take(ncols) {
            *output = arena
                .alloc_str(aliases.get(index).copied().unwrap_or(described[index].name))
                .map_err(|_| arena_full())?;
        }
        arena
            .alloc_slice_copy(&names[..ncols])
            .map_err(|_| arena_full())?
    };
    let column_types = arena
        .alloc_slice_with(ncols, |index| {
            (
                described[index].type_oid,
                described[index].typlen,
                described[index].type_mod,
            )
        })
        .map_err(|_| arena_full())?;
    let column_collations = arena
        .alloc_slice_with(ncols, |index| described[index].collation)
        .map_err(|_| arena_full())?;
    let deferred = crate::sql::ast::DeferredCteQuery {
        tree,
        rows: arena.alloc_atomic_usize(0).map_err(|_| arena_full())?,
    };
    arena
        .alloc(MaterializedCte {
            column_names,
            column_types,
            column_collations,
            definition: None,
            source: crate::sql::ast::MaterializedCteSource::Deferred(deferred),
        })
        .map(|relation| &*relation)
        .map_err(|_| arena_full())
}

/// Describes a whole set-operation query (Describe path): expands CTEs and
/// views schema-only, then unifies the leaf columns.
pub fn describe_set_query<'a>(
    storage: &'a Storage,
    txid: u32,
    q: &'a SetQuery<'a>,
    columns: &mut [ColDesc<'a>],
    arena: &'a Arena,
    description_target: crate::sql::exec::DescriptionTarget,
) -> Result<usize, SqlError> {
    let body = expand_set_tree(q.with, q.body, storage, txid, None, None, arena)?;
    describe_set_body(
        storage,
        body,
        txid,
        columns,
        arena,
        None,
        description_target,
        None,
    )
}

/// Describes a stored set query with its names bound to captured identities.
pub(crate) fn describe_stored_set_query<'a>(
    storage: &'a Storage,
    txid: u32,
    q: &'a SetQuery<'a>,
    (path, dependencies): (
        crate::storage::PathContext,
        crate::storage::StoredQueryDependencyView<'_>,
    ),
    columns: &mut [ColDesc<'a>],
    arena: &'a Arena,
) -> Result<usize, SqlError> {
    let body = expand_set_tree(
        q.with,
        q.body,
        storage,
        txid,
        Some(path),
        Some(dependencies),
        arena,
    )?;
    describe_set_body(
        storage,
        body,
        txid,
        columns,
        arena,
        None,
        crate::sql::exec::DescriptionTarget::Sql,
        None,
    )
}

/// Expands WITH CTEs and view references across a whole set-operation tree
/// (schema-only: a self-referencing recursive CTE binds its non-recursive
/// term's shape, as in [`expand_ctes`]).
pub(crate) fn expand_set_tree<'a>(
    with: &'a [Cte<'a>],
    tree: &'a SetTree<'a>,
    storage: &Storage,
    txid: u32,
    path: Option<crate::storage::PathContext>,
    dependencies: Option<crate::storage::StoredQueryDependencyView<'_>>,
    arena: &'a Arena,
) -> Result<&'a SetTree<'a>, SqlError> {
    if with.is_empty()
        && !set_tree_contains_with(tree)
        && dependencies.is_none()
        && !storage.has_any_view()
    {
        return Ok(tree);
    }
    let wrapper = wrap_set_tree_with(with, tree, arena)?;
    let expanded = expand_ctes_with_path(
        wrapper,
        storage,
        txid,
        path,
        dependencies,
        0,
        ViewReader::Session,
        arena,
    )?;
    Ok(expanded.set_body.expect("wrapper keeps its set body"))
}

/// Like [`expand_set_tree`], but for execution: recursive CTEs materialize to
/// their fixpoint (see [`expand_ctes_exec`]).
#[allow(clippy::too_many_arguments)]
pub(super) fn expand_set_tree_exec<'a>(
    with: &'a [Cte<'a>],
    tree: &'a SetTree<'a>,
    storage: &'a Storage,
    txid: u32,
    arena: &'a Arena,
    params: &[Datum<'a>],
    dml_mats: &[(&'a str, &'a MaterializedCte<'a>)],
    sequences: Option<&dyn SequenceAccess>,
) -> Result<&'a SetTree<'a>, SqlError> {
    if with.is_empty() && !set_tree_contains_with(tree) && !storage.has_any_view() {
        return Ok(tree);
    }
    let wrapper = wrap_set_tree_with(with, tree, arena)?;
    let expanded = expand_ctes_exec(wrapper, storage, txid, arena, params, dml_mats, sequences)?;
    Ok(expanded.set_body.expect("wrapper keeps its set body"))
}

pub(super) struct StoredExecutionContext<'storage, 'params, 'a> {
    path: crate::storage::PathContext,
    dependencies: crate::storage::StoredQueryDependencyView<'storage>,
    params: &'params [Datum<'a>],
    sequences: Option<&'params dyn SequenceAccess>,
}

impl<'storage, 'params, 'a> StoredExecutionContext<'storage, 'params, 'a> {
    pub(super) fn new(
        path: crate::storage::PathContext,
        dependencies: crate::storage::StoredQueryDependencyView<'storage>,
        params: &'params [Datum<'a>],
        sequences: Option<&'params dyn SequenceAccess>,
    ) -> Self {
        Self {
            path,
            dependencies,
            params,
            sequences,
        }
    }
}

pub(super) fn expand_stored_set_tree_exec<'a>(
    with: &'a [Cte<'a>],
    tree: &'a SetTree<'a>,
    storage: &Storage,
    txid: u32,
    execution: StoredExecutionContext<'_, '_, 'a>,
    arena: &'a Arena,
) -> Result<&'a SetTree<'a>, SqlError> {
    let wrapper = wrap_set_tree_with(with, tree, arena)?;
    let expanded = expand_stored_query_exec(
        wrapper,
        storage,
        txid,
        execution.path,
        execution.dependencies,
        arena,
        execution.params,
        execution.sequences,
    )?;
    Ok(expanded.set_body.expect("wrapper keeps its set body"))
}

/// A synthetic Select carrying `with` and the tree as its set body, so the
/// Select-level CTE/view expansion (which already rewrites `set_body`)
/// applies to a whole set-operation query.
fn wrap_set_tree_with<'a>(
    with: &'a [Cte<'a>],
    tree: &'a SetTree<'a>,
    arena: &'a Arena,
) -> Result<&'a Select<'a>, SqlError> {
    let sel = Select {
        items: &[],
        distinct: false,
        distinct_on: &[],
        windows: &[],
        from: None,
        where_clause: None,
        group_by: &[],
        grouping_set_quantifier: crate::sql::ast::GroupingSetQuantifier::All,
        grouping_sets: &[],
        having: None,
        order_by: &[],
        limit: None,
        offset: None,
        with_ties: false,
        with,
        set_body: Some(tree),
        locking: &[],
    };
    Ok(&*arena.alloc(sel).map_err(|_| arena_full())?)
}

type CteBindings<'a> = [(&'a str, &'a Select<'a>, &'a [&'a str])];

/// Threaded through the FROM-reference rewrite: CTE bindings in scope (query
/// plus optional column-rename list), materialized recursive CTEs, storage (to
/// resolve view names), and the current view-expansion depth (a cycle /
/// runaway-nesting guard).
/// Whose SELECT privilege a view expansion checks on the views it expands.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum ViewReader {
    /// The current effective role at the statement boundary.
    Session,
    /// A view owner's role, inside that view's body.
    Role(u16),
    /// Nobody: the expanded view is a write target, whose rows a write does
    /// not read; reads of its columns are checked where they are read.
    WriteTarget,
}

#[derive(Clone, Copy)]
struct Subst<'c, 'a, 's, 'e> {
    ctes: &'c CteBindings<'a>,
    materialized: &'c [(&'a str, &'a MaterializedCte<'a>)],
    storage: &'s Storage,
    /// The requesting transaction, for catalog visibility (a view another
    /// transaction created but has not committed is invisible here).
    txid: u32,
    depth: u32,
    /// Inside a view body: the view creator's search path. Table references
    /// are rewritten fully qualified under it, so the surrounding statement's
    /// path cannot re-bind them. `None` at the statement level.
    path: Option<crate::storage::PathContext>,
    dependencies: Option<crate::storage::StoredQueryDependencyView<'s>>,
    /// Privilege identity inherited while expanding a stored view body.
    authorization_role: ViewReader,
    /// DML on an auto-updatable view is executed against its base table.
    /// Qualified target references must follow that rewrite too.
    qualifier: Option<ViewQualifier<'a>>,
    rule_transition: Option<RuleTransition<'a>>,
    recursive_state: Option<RecursiveStateSubst<'a>>,
    execution: Option<ExecutionSubst<'e, 'a>>,
}

#[derive(Clone, Copy)]
struct ExecutionSubst<'e, 'a> {
    params: &'e [Datum<'a>],
    sequences: Option<&'e dyn SequenceAccess>,
    /// Set when the caller can only use a query whose CTEs all inline (a
    /// result stream): a CTE that must run once sets it and stops the
    /// expansion instead of materializing.
    refused: Option<&'e core::cell::Cell<bool>>,
}

impl<'e, 'a> ExecutionSubst<'e, 'a> {
    fn materializing(params: &'e [Datum<'a>], sequences: Option<&'e dyn SequenceAccess>) -> Self {
        Self {
            params,
            sequences,
            refused: None,
        }
    }

    fn refuses_evaluate_once(self) -> bool {
        self.refused.is_some()
    }

    /// Marks the expansion refused; the error only unwinds it, and the
    /// caller reads the mark.
    fn refuse(self) -> SqlError {
        if let Some(refused) = self.refused {
            refused.set(true);
        }
        sql_err!(
            sqlstate::FEATURE_NOT_SUPPORTED,
            "a common table expression that runs once cannot be inlined"
        )
    }
}

#[derive(Clone, Copy)]
struct ViewQualifier<'a> {
    from: &'a str,
    from_schema: Option<&'a str>,
    to: &'a str,
    to_schema: &'a str,
    exposed_columns: &'a [&'a str],
    base_columns: &'a [&'a str],
    projected_columns: Option<&'a [&'a Expr<'a>]>,
    /// Further names that reach the projected columns (a RETURNING list's
    /// `old` and `new` spellings).
    projected_aliases: &'a [&'a str],
}

#[derive(Clone, Copy)]
struct RuleTransition<'a> {
    names: &'a [&'a str],
    old: &'a [&'a Expr<'a>],
    new: &'a [&'a Expr<'a>],
    old_available: bool,
    new_available: bool,
}

#[derive(Clone, Copy)]
struct RecursiveStateSubst<'a> {
    qualifier: &'a str,
    names: &'a [&'a str],
    types: &'a [ColType],
}

const MAX_VIEW_DEPTH: u32 = 12;

/// Number of references to the unqualified table name `name` anywhere in a
/// select — FROM items (recursing into derived-table subqueries), the set-op
/// body, and expression subqueries.
fn select_references(select: &Select, name: &str) -> usize {
    select_relations(select, &mut |table| unqualified_reference(table, name))
}

fn statement_references(statement: &Stmt<'_>, name: &str) -> usize {
    statement_relations(statement, &mut |table| unqualified_reference(table, name))
}

fn cte_references(cte: &Cte<'_>, name: &str) -> usize {
    cte_relations(cte, &mut |table| unqualified_reference(table, name))
}

fn set_tree_references(tree: &SetTree, name: &str) -> usize {
    set_tree_relations(tree, &mut |table| unqualified_reference(table, name))
}

/// [`select_relations`] for a set-operation query.
pub(crate) fn set_query_relations(
    query: &crate::sql::ast::SetQuery<'_>,
    visit: &mut dyn FnMut(&TableRef<'_>) -> usize,
) -> usize {
    statement_relations(&Stmt::SetQuery(*query), visit)
}

fn unqualified_reference(table: &TableRef<'_>, name: &str) -> usize {
    usize::from(table.schema.is_none() && !table.is_function_source() && table.table == name)
}

/// Visits every relation reference of `select`, in its `FROM` lists, `WITH`
/// queries and expression subqueries, summing what `visit` counts. A derived
/// table is visited through its query.
pub(crate) fn select_relations(s: &Select, visit: &mut dyn FnMut(&TableRef<'_>) -> usize) -> usize {
    let mut count = s.set_body.map_or(0, |tree| set_tree_relations(tree, visit));
    if let Some(f) = &s.from {
        count += from_relations(f, visit);
    }
    for it in s.items {
        count += select_item_relations(it, visit);
    }
    count += s
        .where_clause
        .map_or(0, |expression| expr_relations(expression, visit));
    count += s
        .having
        .map_or(0, |expression| expr_relations(expression, visit));
    count += s
        .group_by
        .iter()
        .map(|expression| expr_relations(expression, visit))
        .sum::<usize>();
    count += s
        .distinct_on
        .iter()
        .map(|expression| expr_relations(expression, visit))
        .sum::<usize>();
    count += s
        .order_by
        .iter()
        .map(|order| expr_relations(order.expression, visit))
        .sum::<usize>();
    count += s
        .limit
        .map_or(0, |expression| expr_relations(expression, visit));
    count += s
        .offset
        .map_or(0, |expression| expr_relations(expression, visit));
    count += s
        .with
        .iter()
        .map(|cte| cte_relations(cte, visit))
        .sum::<usize>();
    count
}

/// References to `name` from `select` outside its own `WITH` list, which
/// counts each CTE's references separately: PostgreSQL's reference count of
/// a CTE is the main query's references plus those of the other CTEs.
fn body_references(select: &Select, name: &str) -> usize {
    select_references(select, name)
        - select
            .with
            .iter()
            .map(|cte| cte_references(cte, name))
            .sum::<usize>()
}

fn cte_relations(cte: &Cte<'_>, visit: &mut dyn FnMut(&TableRef<'_>) -> usize) -> usize {
    match cte.dml {
        Some(statement) => statement_relations(statement, visit),
        None => select_relations(cte.query, visit),
    }
}

fn select_item_relations(
    item: &SelectItem<'_>,
    visit: &mut dyn FnMut(&TableRef<'_>) -> usize,
) -> usize {
    match item {
        SelectItem::Expr { expression, .. } | SelectItem::RecordStar(expression) => {
            expr_relations(expression, visit)
        }
        SelectItem::Wildcard(_) | SelectItem::TableWildcard(_) => 0,
    }
}

fn from_relations(from: &FromClause<'_>, visit: &mut dyn FnMut(&TableRef<'_>) -> usize) -> usize {
    tref_relations(&from.base, visit)
        + from
            .joins
            .iter()
            .map(|join| {
                tref_relations(&join.table, visit)
                    + join.on.map_or(0, |on| expr_relations(on, visit))
            })
            .sum::<usize>()
}

fn returning_relations(
    items: &[SelectItem<'_>],
    visit: &mut dyn FnMut(&TableRef<'_>) -> usize,
) -> usize {
    items
        .iter()
        .map(|item| select_item_relations(item, visit))
        .sum()
}

fn statement_relations(
    statement: &Stmt<'_>,
    visit: &mut dyn FnMut(&TableRef<'_>) -> usize,
) -> usize {
    match statement {
        Stmt::Select(select) => select_relations(select, visit),
        Stmt::SetQuery(query) => {
            set_tree_relations(query.body, visit)
                + query
                    .order_by
                    .iter()
                    .map(|order| expr_relations(order.expression, visit))
                    .sum::<usize>()
                + query
                    .limit
                    .map_or(0, |expression| expr_relations(expression, visit))
                + query
                    .offset
                    .map_or(0, |expression| expr_relations(expression, visit))
        }
        Stmt::With { ctes, statement } => {
            ctes.iter()
                .map(|cte| cte_relations(cte, visit))
                .sum::<usize>()
                + statement_relations(statement, visit)
        }
        Stmt::Insert(insert) => {
            insert
                .select
                .map_or(0, |select| select_relations(select, visit))
                + insert
                    .rows
                    .iter()
                    .flat_map(|row| row.iter())
                    .map(|expression| expr_relations(expression, visit))
                    .sum::<usize>()
                + insert.on_conflict.map_or(0, |conflict| {
                    conflict
                        .target
                        .iter()
                        .map(|target| expr_relations(target.expression, visit))
                        .sum::<usize>()
                        + conflict.update.map_or(0, |assignments| {
                            assignments
                                .iter()
                                .map(|(_, expression)| expr_relations(expression, visit))
                                .sum::<usize>()
                        })
                        + conflict
                            .update_where
                            .map_or(0, |expression| expr_relations(expression, visit))
                })
                + returning_relations(insert.returning.items, visit)
        }
        Stmt::Update(update) => {
            update.from.map_or(0, |from| from_relations(from, visit))
                + update
                    .assignments
                    .iter()
                    .map(|(_, expression)| expr_relations(expression, visit))
                    .sum::<usize>()
                + update
                    .where_clause
                    .map_or(0, |expression| expr_relations(expression, visit))
                + returning_relations(update.returning.items, visit)
        }
        Stmt::Delete(delete) => {
            delete.using.map_or(0, |from| from_relations(from, visit))
                + delete
                    .where_clause
                    .map_or(0, |expression| expr_relations(expression, visit))
                + returning_relations(delete.returning.items, visit)
        }
        Stmt::Merge(merge) => {
            tref_relations(&merge.source, visit)
                + expr_relations(merge.on, visit)
                + merge
                    .whens
                    .iter()
                    .map(|when| {
                        when.condition()
                            .map_or(0, |condition| expr_relations(condition, visit))
                            + match when.action() {
                                crate::sql::ast::MergeActionRef::Update(assignments) => assignments
                                    .iter()
                                    .map(|(_, expression)| expr_relations(expression, visit))
                                    .sum(),
                                crate::sql::ast::MergeActionRef::Insert { values, .. } => values
                                    .iter()
                                    .map(|expression| expr_relations(expression, visit))
                                    .sum(),
                                crate::sql::ast::MergeActionRef::Delete
                                | crate::sql::ast::MergeActionRef::DoNothing => 0,
                            }
                    })
                    .sum::<usize>()
                + returning_relations(merge.returning.items, visit)
        }
        _ => 0,
    }
}

fn tref_relations(t: &TableRef, visit: &mut dyn FnMut(&TableRef<'_>) -> usize) -> usize {
    if let Some(sub) = t.subquery {
        return select_relations(sub, visit);
    }
    let argument_references = t.func_args.map_or(0, |arguments| {
        arguments
            .iter()
            .map(|argument| expr_relations(argument, visit))
            .sum()
    });
    let grouped_references = t.rows_from.map_or(0, |functions| {
        functions
            .iter()
            .map(|function| tref_relations(function, visit))
            .sum()
    });
    let sample_references = t.sample.map_or(0, |sample| {
        expr_relations(sample.percentage, visit)
            + sample
                .repeatable
                .map_or(0, |repeatable| expr_relations(repeatable, visit))
    });
    argument_references + grouped_references + sample_references + visit(t)
}

fn set_tree_relations(tree: &SetTree, visit: &mut dyn FnMut(&TableRef<'_>) -> usize) -> usize {
    match tree {
        SetTree::Select(s) => select_relations(s, visit),
        SetTree::Op { left, right, .. } => {
            set_tree_relations(left, visit) + set_tree_relations(right, visit)
        }
    }
}

/// Number of references to `name` inside expression subqueries of `e`.
fn expr_relations(e: &Expr, visit: &mut dyn FnMut(&TableRef<'_>) -> usize) -> usize {
    match e {
        Expr::Subquery(s)
        | Expr::Exists(s)
        | Expr::ArraySubquery(s)
        | Expr::RowSubquery { select: s, .. } => select_relations(s, visit),
        Expr::InSubquery {
            operand, select, ..
        }
        | Expr::QuantifiedSubquery {
            operand, select, ..
        } => expr_relations(operand, visit) + select_relations(select, visit),
        Expr::Unary { operand, .. }
        | Expr::Cast { operand, .. }
        | Expr::BoundDefault {
            expression: operand,
            ..
        }
        | Expr::Collate { operand, .. }
        | Expr::IsNull { operand, .. } => expr_relations(operand, visit),
        Expr::Binary { left, right, .. } => {
            expr_relations(left, visit) + expr_relations(right, visit)
        }
        Expr::Call {
            args,
            order_by,
            over,
            filter,
            ..
        } => {
            args.iter()
                .map(|argument| expr_relations(argument, visit))
                .sum::<usize>()
                + order_by
                    .iter()
                    .map(|order| expr_relations(order.expression, visit))
                    .sum::<usize>()
                + over.map_or(0, |window| {
                    window
                        .partition_by
                        .iter()
                        .map(|expression| expr_relations(expression, visit))
                        .sum::<usize>()
                        + window
                            .order_by
                            .iter()
                            .map(|order| expr_relations(order.expression, visit))
                            .sum::<usize>()
                        + window.frame.map_or(0, |frame| {
                            frame_bound_relations(frame.start, visit)
                                + frame_bound_relations(frame.end, visit)
                        })
                })
                + filter.map_or(0, |expression| expr_relations(expression, visit))
        }
        Expr::InList { operand, list, .. } => {
            expr_relations(operand, visit)
                + list.iter().map(|x| expr_relations(x, visit)).sum::<usize>()
        }
        Expr::Like {
            operand,
            pattern,
            escape,
            ..
        } => {
            expr_relations(operand, visit)
                + expr_relations(pattern, visit)
                + escape.map_or(0, |expression| expr_relations(expression, visit))
        }
        Expr::Match {
            operand, pattern, ..
        } => expr_relations(operand, visit) + expr_relations(pattern, visit),
        Expr::Case {
            operand,
            whens,
            otherwise,
            ..
        } => {
            operand.map_or(0, |o| expr_relations(o, visit))
                + whens
                    .iter()
                    .map(|(c, r)| expr_relations(c, visit) + expr_relations(r, visit))
                    .sum::<usize>()
                + otherwise.map_or(0, |o| expr_relations(o, visit))
        }
        Expr::Array(items) => items.iter().map(|x| expr_relations(x, visit)).sum(),
        Expr::Subscript { base, index } => {
            expr_relations(base, visit) + expr_relations(index, visit)
        }
        Expr::Slice { base, lower, upper } => {
            expr_relations(base, visit)
                + lower.map_or(0, |expression| expr_relations(expression, visit))
                + upper.map_or(0, |expression| expr_relations(expression, visit))
        }
        Expr::Field { base, .. } => expr_relations(base, visit),
        Expr::AnyAll { operand, array, .. } => {
            expr_relations(operand, visit) + expr_relations(array, visit)
        }
        _ => 0,
    }
}

fn frame_bound_relations(
    bound: crate::sql::ast::FrameBound<'_>,
    visit: &mut dyn FnMut(&TableRef<'_>) -> usize,
) -> usize {
    match bound {
        crate::sql::ast::FrameBound::Preceding(expression)
        | crate::sql::ast::FrameBound::Following(expression) => expr_relations(expression, visit),
        crate::sql::ast::FrameBound::UnboundedPreceding
        | crate::sql::ast::FrameBound::CurrentRow
        | crate::sql::ast::FrameBound::UnboundedFollowing => 0,
    }
}

/// The query a `WITH` list belongs to.
#[derive(Clone, Copy)]
pub(crate) enum CteReader<'a> {
    Select(&'a Select<'a>),
    Statement(&'a Stmt<'a>),
}

/// PostgreSQL runs a data-modifying `WITH` query when its rows are first read,
/// and one that nothing reads after the main query has finished.
pub(crate) fn dml_cte_runs_after(with: &[Cte<'_>], index: usize, main: CteReader<'_>) -> bool {
    let name = with[index].name;
    let read_by_main = match main {
        CteReader::Select(select) => select_references(select, name),
        CteReader::Statement(statement) => statement_references(statement, name),
    };
    read_by_main == 0
        && with
            .iter()
            .enumerate()
            .all(|(other, cte)| other == index || cte_references(cte, name) == 0)
}

fn select_mentions_column(select: &Select<'_>, qualifier: &str, name: &str) -> bool {
    select.items.iter().any(|item| match item {
        SelectItem::Expr { expression, .. } | SelectItem::RecordStar(expression) => {
            expr_mentions_column(expression, qualifier, name)
        }
        SelectItem::Wildcard(_) | SelectItem::TableWildcard(_) => false,
    }) || select
        .where_clause
        .is_some_and(|expression| expr_mentions_column(expression, qualifier, name))
        || select
            .having
            .is_some_and(|expression| expr_mentions_column(expression, qualifier, name))
        || select
            .group_by
            .iter()
            .chain(select.distinct_on)
            .any(|expression| expr_mentions_column(expression, qualifier, name))
        || select
            .order_by
            .iter()
            .any(|order| expr_mentions_column(order.expression, qualifier, name))
        || select
            .limit
            .is_some_and(|expression| expr_mentions_column(expression, qualifier, name))
        || select
            .offset
            .is_some_and(|expression| expr_mentions_column(expression, qualifier, name))
        || select.from.is_some_and(|from| {
            from.joins.iter().any(|join| {
                join.on
                    .is_some_and(|expression| expr_mentions_column(expression, qualifier, name))
            })
        })
}

pub(super) fn expr_mentions_column(expression: &Expr<'_>, qualifier: &str, name: &str) -> bool {
    let child = |expression| expr_mentions_column(expression, qualifier, name);
    match expression {
        Expr::Column {
            qualifier: written,
            name: written_name,
        } => *written_name == name && written.is_none_or(|written| written == qualifier),
        Expr::Subquery(select)
        | Expr::Exists(select)
        | Expr::ArraySubquery(select)
        | Expr::RowSubquery { select, .. } => select_mentions_column(select, qualifier, name),
        Expr::InSubquery {
            operand, select, ..
        }
        | Expr::QuantifiedSubquery {
            operand, select, ..
        } => child(operand) || select_mentions_column(select, qualifier, name),
        Expr::Unary { operand, .. }
        | Expr::Cast { operand, .. }
        | Expr::BoundDefault {
            expression: operand,
            ..
        }
        | Expr::Collate { operand, .. }
        | Expr::IsNull { operand, .. }
        | Expr::Field { base: operand, .. } => child(operand),
        Expr::Binary { left, right, .. } => child(left) || child(right),
        Expr::Call {
            args,
            order_by,
            over,
            filter,
            ..
        } => {
            args.iter().any(|expression| child(expression))
                || order_by.iter().any(|order| child(order.expression))
                || over.is_some_and(|window| {
                    window
                        .partition_by
                        .iter()
                        .any(|expression| child(expression))
                        || window.order_by.iter().any(|order| child(order.expression))
                        || window.frame.is_some_and(|frame| {
                            frame_bound_mentions_column(frame.start, qualifier, name)
                                || frame_bound_mentions_column(frame.end, qualifier, name)
                        })
                })
                || filter.is_some_and(child)
        }
        Expr::InList { operand, list, .. } => {
            child(operand) || list.iter().any(|expression| child(expression))
        }
        Expr::Like {
            operand,
            pattern,
            escape,
            ..
        } => child(operand) || child(pattern) || escape.is_some_and(child),
        Expr::Match {
            operand, pattern, ..
        } => child(operand) || child(pattern),
        Expr::Case {
            operand,
            whens,
            otherwise,
            ..
        } => {
            operand.is_some_and(child)
                || whens
                    .iter()
                    .any(|(condition, result)| child(condition) || child(result))
                || otherwise.is_some_and(child)
        }
        Expr::Array(items) => items.iter().any(|expression| child(expression)),
        Expr::Subscript { base, index } => child(base) || child(index),
        Expr::Slice { base, lower, upper } => {
            child(base) || lower.is_some_and(child) || upper.is_some_and(child)
        }
        Expr::AnyAll { operand, array, .. } => child(operand) || child(array),
        _ => false,
    }
}

fn frame_bound_mentions_column(
    bound: crate::sql::ast::FrameBound<'_>,
    qualifier: &str,
    name: &str,
) -> bool {
    match bound {
        crate::sql::ast::FrameBound::Preceding(expression)
        | crate::sql::ast::FrameBound::Following(expression) => {
            expr_mentions_column(expression, qualifier, name)
        }
        _ => false,
    }
}

pub(super) fn select_contains_volatile(select: &Select<'_>, storage: &Storage, txid: u32) -> bool {
    select_contains_function(select, &|name, arguments| {
        super::super::ast::is_volatile_function(name)
            || storage.has_volatile_function_routine_candidate(name, arguments, txid)
    })
}

pub(super) fn expression_contains_function(
    expression: &Expr<'_>,
    matches: &impl Fn(&str, usize) -> bool,
) -> bool {
    if matches!(expression, Expr::Call { name, args, .. } if matches(name, args.len())) {
        return true;
    }
    let nested = match expression {
        Expr::Subquery(select)
        | Expr::Exists(select)
        | Expr::ArraySubquery(select)
        | Expr::RowSubquery { select, .. } => select_contains_function(select, matches),
        Expr::InSubquery {
            operand, select, ..
        }
        | Expr::QuantifiedSubquery {
            operand, select, ..
        } => {
            expression_contains_function(operand, matches)
                || select_contains_function(select, matches)
        }
        _ => false,
    };
    if nested {
        return true;
    }
    let mut found = false;
    super::walk_children(expression, &mut |child| {
        found |= expression_contains_function(child, matches);
        Ok(())
    })
    .expect("expression visitor cannot fail");
    found
}

pub(super) fn select_contains_function(
    select: &Select<'_>,
    matches: &impl Fn(&str, usize) -> bool,
) -> bool {
    let expression_has_it =
        |expression: &Expr<'_>| expression_contains_function(expression, matches);
    if select.items.iter().any(|item| match item {
        SelectItem::Expr { expression, .. } | SelectItem::RecordStar(expression) => {
            expression_has_it(expression)
        }
        SelectItem::Wildcard(_) | SelectItem::TableWildcard(_) => false,
    }) || select.where_clause.is_some_and(expression_has_it)
        || select.having.is_some_and(expression_has_it)
        || select
            .group_by
            .iter()
            .any(|expression| expression_has_it(expression))
        || select
            .distinct_on
            .iter()
            .any(|expression| expression_has_it(expression))
        || select
            .order_by
            .iter()
            .any(|order| expression_has_it(order.expression))
        || select.limit.is_some_and(expression_has_it)
        || select.offset.is_some_and(expression_has_it)
    {
        return true;
    }
    if let Some(from) = select.from {
        let table_has_it = |table: &TableRef<'_>| {
            table
                .subquery
                .is_some_and(|query| select_contains_function(query, matches))
                // A WITH query not yet read runs, with its calls, where this
                // query first reads it.
                || table.cte.is_some_and(|cte| match cte.source {
                    crate::sql::ast::MaterializedCteSource::Deferred(deferred) => {
                        deferred.rows.load(core::sync::atomic::Ordering::Relaxed) == 0
                            && set_tree_contains_function(deferred.tree, matches)
                    }
                    _ => false,
                })
                || table.func_args.is_some_and(|arguments| {
                    matches(table.table, arguments.len())
                        || arguments.iter().any(|argument| expression_has_it(argument))
                })
                || table.rows_from.is_some_and(|tables| {
                    tables.iter().any(|table| {
                        table.func_args.is_some_and(|arguments| {
                            matches(table.table, arguments.len())
                                || arguments.iter().any(|argument| expression_has_it(argument))
                        }) || table
                            .subquery
                            .is_some_and(|query| select_contains_function(query, matches))
                    })
                })
                || table.sample.is_some_and(|sample| {
                    expression_has_it(sample.percentage)
                        || sample.repeatable.is_some_and(expression_has_it)
                })
        };
        if table_has_it(&from.base)
            || from
                .joins
                .iter()
                .any(|join| table_has_it(&join.table) || join.on.is_some_and(expression_has_it))
        {
            return true;
        }
    }
    select
        .set_body
        .is_some_and(|tree| set_tree_contains_function(tree, matches))
        || select
            .with
            .iter()
            .any(|cte| select_contains_function(cte.query, matches))
}

pub(super) fn set_tree_contains_function(
    tree: &SetTree<'_>,
    matches: &impl Fn(&str, usize) -> bool,
) -> bool {
    match tree {
        SetTree::Select(select) => select_contains_function(select, matches),
        SetTree::Op { left, right, .. } => {
            set_tree_contains_function(left, matches) || set_tree_contains_function(right, matches)
        }
    }
}

/// Number of *direct* FROM references to `name` in the top-level selects of a
/// set tree (base table or join item; a reference inside a derived-table
/// subquery or an expression subquery does not count).
fn direct_references(tree: &SetTree, name: &str) -> usize {
    let direct = |t: &TableRef| -> usize {
        usize::from(
            t.schema.is_none()
                && t.subquery.is_none()
                && !t.is_function_source()
                && t.table == name,
        )
    };
    match tree {
        SetTree::Select(s) => {
            let mut count = 0;
            if let Some(f) = &s.from {
                count += direct(&f.base);
                for j in f.joins {
                    count += direct(&j.table);
                }
            }
            count
        }
        SetTree::Op { left, right, .. } => {
            direct_references(left, name) + direct_references(right, name)
        }
    }
}

/// Splits a recursive CTE body into `(non-recursive term, recursive term,
/// union-all)`, enforcing PostgreSQL's required shape.
fn recursive_parts<'a>(
    q: &'a Select<'a>,
    name: &str,
) -> Result<(&'a SetTree<'a>, &'a SetTree<'a>, bool), SqlError> {
    let Some(&SetTree::Op {
        operator: SetOp::Union,
        all,
        left,
        right,
        ..
    }) = q.set_body
    else {
        return Err(sql_err!(
            crate::sql::eval::sqlstate::INVALID_RECURSION,
            "recursive query \"{}\" does not have the form non-recursive-term UNION [ALL] recursive-term",
            name
        ));
    };
    // PostgreSQL names the first such clause, in this order.
    let unsupported = if !q.order_by.is_empty() {
        Some("ORDER BY")
    } else if q.offset.is_some() {
        Some("OFFSET")
    } else if q.limit.is_some() {
        Some("LIMIT")
    } else if !q.locking.is_empty() {
        Some("FOR UPDATE/SHARE")
    } else {
        None
    };
    if let Some(clause) = unsupported {
        return Err(sql_err!(
            sqlstate::FEATURE_NOT_SUPPORTED,
            "{} in a recursive query is not implemented",
            clause
        ));
    }
    if set_tree_references(left, name) > 0 {
        return Err(sql_err!(
            crate::sql::eval::sqlstate::INVALID_RECURSION,
            "recursive reference to query \"{}\" must not appear within its non-recursive term",
            name
        ));
    }
    Ok((left, right, all))
}

#[derive(Clone, Copy)]
struct RecursiveSearch<'a> {
    order: CteSearchOrder,
    keys: &'a [usize],
    output: usize,
}

#[derive(Clone, Copy)]
struct RecursiveCycle<'a> {
    keys: &'a [usize],
    mark: usize,
    path: usize,
    mark_value: Datum<'a>,
    mark_default: Datum<'a>,
}

#[derive(Clone, Copy)]
struct RecursiveDecoration<'a> {
    visible: usize,
    column_names: &'a [&'a str],
    column_types: &'a [(i32, i16, i32)],
    column_collations: &'a [Collation],
    state_types: &'a [ColType],
    search: Option<RecursiveSearch<'a>>,
    cycle: Option<RecursiveCycle<'a>>,
    name_collision: Option<(&'static str, &'a str)>,
}

fn recursive_key_columns<'a>(
    requested: &[&str],
    names: &[&str],
    clause: &str,
    arena: &'a Arena,
) -> Result<&'a [usize], SqlError> {
    let mut resolved = [0usize; MAX_PROJ];
    if requested.is_empty() || requested.len() > resolved.len() {
        return Err(sql_err!(
            sqlstate::SYNTAX_ERROR,
            "recursive search key list is empty or too wide"
        ));
    }
    for (index, requested) in requested.iter().enumerate() {
        let mut matches = names
            .iter()
            .enumerate()
            .filter(|(_, candidate)| *candidate == requested);
        let Some((column, _)) = matches.next() else {
            return Err(sql_err!(
                sqlstate::SYNTAX_ERROR,
                "{} column \"{}\" not in WITH query column list",
                clause,
                requested
            ));
        };
        if matches.next().is_some() {
            return Err(sql_err!(
                sqlstate::AMBIGUOUS_COLUMN,
                "column reference \"{}\" is ambiguous",
                requested
            ));
        }
        if resolved[..index].contains(&column) {
            return Err(sql_err!(
                sqlstate::DUPLICATE_COLUMN,
                "{} column \"{}\" specified more than once",
                clause,
                requested
            ));
        }
        resolved[index] = column;
    }
    arena
        .alloc_slice_copy(&resolved[..requested.len()])
        .map(|columns| &*columns)
        .map_err(|_| arena_full())
}

fn prepare_recursive_decoration<'a>(
    cte: &'a Cte<'a>,
    base_names: &[&'a str],
    base_types: &[(i32, i16, i32)],
    base_collations: &[Collation],
    storage: &Storage,
    txid: u32,
    arena: &'a Arena,
) -> Result<RecursiveDecoration<'a>, SqlError> {
    let generated = usize::from(cte.search.is_some()) + usize::from(cte.cycle.is_some()) * 2;
    if base_names.len() + generated > MAX_PROJ
        || base_names.len() + generated > crate::storage::MAX_COLUMNS
    {
        return Err(sql_err!(sqlstate::TOO_MANY_COLUMNS, "too many columns"));
    }
    let mut names = [""; MAX_PROJ];
    let mut types = [(0i32, 0i16, -1i32); MAX_PROJ];
    let mut collations = [Collation::None; MAX_PROJ];
    let visible = base_names.len();
    names[..visible].copy_from_slice(base_names);
    types[..visible].copy_from_slice(base_types);
    collations[..visible].copy_from_slice(base_collations);
    let mut next = visible;
    let mut state_types = [ColType::Bool; 3];
    let mut state_count = 0usize;
    let mut name_collision = None;
    let search = if let Some(search) = cte.search {
        if names[..next].contains(&search.sequence_column) {
            name_collision = Some(("search sequence", search.sequence_column));
        }
        let keys = recursive_key_columns(search.columns, base_names, "search", arena)?;
        names[next] = search.sequence_column;
        let ctype = match search.order {
            CteSearchOrder::BreadthFirst => ColType::Record,
            CteSearchOrder::DepthFirst => ColType::Array(ArrElem::Record),
        };
        types[next] = (ctype.oid(), ctype.typlen(), -1);
        state_types[state_count] = ctype;
        state_count += 1;
        let output = next;
        next += 1;
        Some(RecursiveSearch {
            order: search.order,
            keys,
            output,
        })
    } else {
        None
    };
    let cycle = if let Some(cycle) = cte.cycle {
        for generated_name in [cycle.mark_column, cycle.path_column] {
            if names[..next].contains(&generated_name) {
                name_collision.get_or_insert(("cycle", generated_name));
            }
        }
        if cycle.mark_column == cycle.path_column {
            name_collision.get_or_insert(("cycle", cycle.mark_column));
        }
        let keys = recursive_key_columns(cycle.columns, base_names, "cycle", arena)?;
        let (mark_value, mark_default, mark_type) = match cycle.mark {
            CteCycleMark::Boolean => (Datum::Bool(true), Datum::Bool(false), ColType::Bool),
            CteCycleMark::Custom { value, default } => {
                let value_type =
                    crate::sql::exec::infer_type_catalog(value, None, storage, txid, arena)?.0;
                let default_type =
                    crate::sql::exec::infer_type_catalog(default, None, storage, txid, arena)?.0;
                let value_type = (value_type != crate::sql::types::oid::UNKNOWN)
                    .then(|| crate::sql::exec::catalog_column_type(storage, txid, value_type))
                    .flatten()
                    .map(|(ctype, _)| ctype);
                let default_type = (default_type != crate::sql::types::oid::UNKNOWN)
                    .then(|| crate::sql::exec::catalog_column_type(storage, txid, default_type))
                    .flatten()
                    .map(|(ctype, _)| ctype);
                let mark_type = match (value_type, default_type) {
                    (Some(left), Some(right)) => super::setops::unify_set_type(left, right),
                    (Some(known), None) | (None, Some(known)) => Some(known),
                    (None, None) => Some(ColType::Text),
                }
                .ok_or_else(|| {
                    sql_err!(sqlstate::DATATYPE_MISMATCH, "CYCLE types cannot be matched")
                })?;
                let catalog = super::storage_catalog(storage, arena, txid);
                let hooks = crate::sql::eval::EvalHooks {
                    catalog: Some(&catalog),
                    ..crate::sql::eval::NO_HOOKS
                };
                let value = crate::sql::eval::eval_full(
                    value,
                    arena,
                    crate::sql::eval::NO_PARAMS,
                    &crate::sql::eval::NoColumns,
                    &hooks,
                )?;
                let default = crate::sql::eval::eval_full(
                    default,
                    arena,
                    crate::sql::eval::NO_PARAMS,
                    &crate::sql::eval::NoColumns,
                    &hooks,
                )?;
                (
                    super::setops::coerce_set_value(value, mark_type, arena)?,
                    super::setops::coerce_set_value(default, mark_type, arena)?,
                    mark_type,
                )
            }
        };
        names[next] = cycle.mark_column;
        types[next] = (mark_type.oid(), mark_type.typlen(), -1);
        collations[next] = if mark_type.is_collatable() {
            Collation::Default
        } else {
            Collation::None
        };
        let mark = next;
        state_types[state_count] = mark_type;
        state_count += 1;
        next += 1;
        names[next] = cycle.path_column;
        let path_type = ColType::Array(ArrElem::Record);
        types[next] = (path_type.oid(), path_type.typlen(), -1);
        state_types[state_count] = path_type;
        state_count += 1;
        let path = next;
        next += 1;
        Some(RecursiveCycle {
            keys,
            mark,
            path,
            mark_value,
            mark_default,
        })
    } else {
        None
    };
    Ok(RecursiveDecoration {
        visible,
        column_names: arena
            .alloc_slice_copy(&names[..next])
            .map_err(|_| arena_full())?,
        column_types: arena
            .alloc_slice_copy(&types[..next])
            .map_err(|_| arena_full())?,
        column_collations: arena
            .alloc_slice_copy(&collations[..next])
            .map_err(|_| arena_full())?,
        state_types: arena
            .alloc_slice_copy(&state_types[..state_count])
            .map_err(|_| arena_full())?,
        search,
        cycle,
        name_collision,
    })
}

fn reject_recursive_name_collision<'a>(
    cte: &'a Cte<'a>,
    recursive_tree: &'a SetTree<'a>,
    decoration: RecursiveDecoration<'a>,
    storage: &Storage,
    txid: u32,
    arena: &'a Arena,
) -> Result<(), SqlError> {
    let Some((clause, name)) = decoration.name_collision else {
        return Ok(());
    };
    if let SetTree::Select(select) = recursive_tree
        && let Some(qualifier) = recursive_table_qualifier(select, cte.name)
        && select_mentions_column(select, qualifier, name)
    {
        return Err(sql_err!(
            sqlstate::AMBIGUOUS_COLUMN,
            "column reference \"{}\" is ambiguous",
            name
        ));
    }
    // PostgreSQL resolves the recursive term against the rewritten output
    // before reporting the clause-level duplicate. An explicit or unqualified
    // reference to the duplicated name is therefore 42702; a term that never
    // references it reaches the later 42601 clause error.
    let relation = arena
        .alloc(MaterializedCte {
            column_names: decoration.column_names,
            column_types: decoration.column_types,
            column_collations: decoration.column_collations,
            definition: None,
            source: crate::sql::ast::MaterializedCteSource::Inline(&[]),
        })
        .map_err(|_| arena_full())?;
    let binding = [(cte.name, &*relation)];
    let rewritten = subst_set_tree(
        recursive_tree,
        Subst {
            ctes: &[],
            materialized: &binding,
            storage,
            txid,
            depth: 0,
            path: None,
            dependencies: None,
            authorization_role: ViewReader::Session,
            qualifier: None,
            rule_transition: None,
            recursive_state: None,
            execution: None,
        },
        arena,
    )?;
    let mut columns = [ColDesc::new("", 0, 0); MAX_PROJ];
    describe_set_body(
        storage,
        rewritten,
        txid,
        &mut columns,
        arena,
        None,
        crate::sql::exec::DescriptionTarget::Sql,
        None,
    )?;
    Err(sql_err!(
        sqlstate::SYNTAX_ERROR,
        "{} column name \"{}\" already used in WITH query column list",
        clause,
        name
    ))
}

fn recursive_key<'a>(
    row: &'a [u8],
    keys: &[usize],
    decoration: RecursiveDecoration<'a>,
    arena: &'a Arena,
) -> Result<Datum<'a>, SqlError> {
    let fields = arena
        .alloc_slice_with(keys.len(), |_| RecordField {
            name: "",
            type_oid: 0,
            type_mod: -1,
            value: Datum::Null,
        })
        .map_err(|_| arena_full())?;
    for (field, &column) in fields.iter_mut().zip(keys) {
        field.name = decoration.column_names[column];
        field.type_oid = decoration.column_types[column].0;
        field.value = crate::sql::exec::decode_projected_col_record(row, column, arena)?;
    }
    Ok(Datum::Record(fields))
}

fn recursive_path<'a>(
    parent: Option<Datum<'a>>,
    key: Datum<'a>,
    arena: &'a Arena,
) -> Result<Datum<'a>, SqlError> {
    let parent_count = match parent {
        Some(Datum::Array {
            element: ArrElem::Record,
            raw,
        }) => crate::sql::array::len(raw),
        _ => 0,
    };
    let values = crate::sql::array::alloc_items(arena, parent_count + 1)?;
    let mut count = 0usize;
    if let Some(Datum::Array {
        element: ArrElem::Record,
        raw,
    }) = parent
    {
        count = crate::sql::array::len(raw);
        for (value, payload) in values
            .iter_mut()
            .zip(crate::sql::array::element_payloads(raw))
        {
            *value = crate::sql::exec::decode_projected_col_record(payload, 0, arena)?;
        }
    }
    values[count] = key;
    Ok(Datum::Array {
        element: ArrElem::Record,
        raw: crate::sql::array::build(values, arena)?,
    })
}

fn recursive_path_contains<'a>(
    path: Datum<'a>,
    key: &Datum<'a>,
    arena: &'a Arena,
) -> Result<bool, SqlError> {
    let Datum::Array {
        element: ArrElem::Record,
        raw,
    } = path
    else {
        return Err(sql_err!(
            sqlstate::INTERNAL_ERROR,
            "invalid recursive path state"
        ));
    };
    for payload in crate::sql::array::element_payloads(raw) {
        let member = crate::sql::exec::decode_projected_col_record(payload, 0, arena)?;
        if crate::sql::eval::membership_eq(&member, key, None)? == Some(true) {
            return Ok(true);
        }
    }
    Ok(false)
}

fn decorate_recursive_row<'a>(
    source: &'a [u8],
    decoration: RecursiveDecoration<'a>,
    base: bool,
    arena: &'a Arena,
) -> Result<(&'a [u8], bool), SqlError> {
    let mut values = [Datum::Null; MAX_PROJ];
    for (column, value) in values.iter_mut().enumerate().take(decoration.visible) {
        *value = crate::sql::exec::decode_projected_col_record(source, column, arena)?;
    }
    if let Some(search) = decoration.search {
        let key = recursive_key(source, search.keys, decoration, arena)?;
        values[search.output] = match search.order {
            CteSearchOrder::DepthFirst => {
                let parent = (!base).then(|| {
                    crate::sql::exec::decode_projected_col_record(source, search.output, arena)
                });
                recursive_path(parent.transpose()?, key, arena)?
            }
            CteSearchOrder::BreadthFirst => {
                let depth = if base {
                    0
                } else {
                    let parent = crate::sql::exec::decode_projected_col_record(
                        source,
                        search.output,
                        arena,
                    )?;
                    let Datum::Record(fields) = parent else {
                        return Err(sql_err!(
                            sqlstate::INTERNAL_ERROR,
                            "invalid breadth-first recursive state"
                        ));
                    };
                    match fields.first().map(|field| field.value) {
                        Some(Datum::Int8(depth)) => depth + 1,
                        _ => {
                            return Err(sql_err!(
                                sqlstate::INTERNAL_ERROR,
                                "invalid breadth-first recursive depth"
                            ));
                        }
                    }
                };
                let Datum::Record(key_fields) = key else {
                    unreachable!("recursive key is a record")
                };
                let fields = arena
                    .alloc_slice_with(key_fields.len() + 1, |_| RecordField {
                        name: "",
                        type_oid: 0,
                        type_mod: -1,
                        value: Datum::Null,
                    })
                    .map_err(|_| arena_full())?;
                fields[0] = RecordField {
                    name: "*DEPTH*",
                    type_oid: crate::sql::types::oid::INT8,
                    type_mod: -1,
                    value: Datum::Int8(depth),
                };
                fields[1..].copy_from_slice(key_fields);
                Datum::Record(fields)
            }
        };
    }
    let mut is_cycle = false;
    if let Some(cycle) = decoration.cycle {
        let key = recursive_key(source, cycle.keys, decoration, arena)?;
        let parent = if base {
            None
        } else {
            Some(crate::sql::exec::decode_projected_col_record(
                source, cycle.path, arena,
            )?)
        };
        if let Some(path) = parent {
            is_cycle = recursive_path_contains(path, &key, arena)?;
        }
        values[cycle.mark] = if is_cycle {
            cycle.mark_value
        } else {
            cycle.mark_default
        };
        values[cycle.path] = recursive_path(parent, key, arena)?;
    }
    let row =
        crate::sql::exec::encode_projected_pub(&values[..decoration.column_names.len()], arena)?;
    Ok((row, is_cycle))
}

fn recursive_noncycle_rows<'a>(
    rows: &'a [&'a [u8]],
    decoration: RecursiveDecoration<'a>,
    arena: &'a Arena,
) -> Result<&'a [&'a [u8]], SqlError> {
    let Some(cycle) = decoration.cycle else {
        return Ok(rows);
    };
    let output = arena
        .alloc_slice_with(rows.len(), |_| &[] as &[u8])
        .map_err(|_| arena_full())?;
    let mut count = 0usize;
    for &row in rows {
        let mark = crate::sql::exec::decode_projected_col_record(row, cycle.mark, arena)?;
        if crate::sql::eval::membership_eq(&mark, &cycle.mark_value, None)? != Some(true) {
            output[count] = row;
            count += 1;
        }
    }
    Ok(&output[..count])
}

fn recursive_table_qualifier<'a>(select: &'a Select<'a>, name: &str) -> Option<&'a str> {
    recursive_select_reference(select, name).map(|table| table.alias.unwrap_or(table.table))
}

fn recursive_select_reference<'a>(select: &'a Select<'a>, name: &str) -> Option<&'a TableRef<'a>> {
    let matches = |table: &'a TableRef<'a>| {
        (table.schema.is_none()
            && table.subquery.is_none()
            && !table.is_function_source()
            && table.table == name)
            .then_some(table)
    };
    let from = select.from.as_ref()?;
    matches(&from.base).or_else(|| from.joins.iter().find_map(|join| matches(&join.table)))
}

fn recursive_tree_reference<'a>(tree: &'a SetTree<'a>, name: &str) -> Option<&'a TableRef<'a>> {
    match tree {
        SetTree::Select(select) => recursive_select_reference(select, name),
        SetTree::Op { left, right, .. } => {
            recursive_tree_reference(left, name).or_else(|| recursive_tree_reference(right, name))
        }
    }
}

fn append_recursive_state<'a>(
    tree: &'a SetTree<'a>,
    cte_name: &str,
    decoration: RecursiveDecoration<'a>,
    storage: &Storage,
    txid: u32,
    arena: &'a Arena,
) -> Result<&'a SetTree<'a>, SqlError> {
    let SetTree::Select(select) = tree else {
        return Err(sql_err!(
            sqlstate::SYNTAX_ERROR,
            "with a SEARCH or CYCLE clause, the recursive query must be a UNION of two SELECT commands"
        ));
    };
    let qualifier = recursive_table_qualifier(select, cte_name).ok_or_else(|| {
        sql_err!(
            sqlstate::INVALID_RECURSION,
            "recursive reference to query \"{}\" is not in the recursive term",
            cte_name
        )
    })?;
    let select = subst_select_body(
        select,
        Subst {
            ctes: &[],
            materialized: &[],
            storage,
            txid,
            depth: 0,
            path: None,
            dependencies: None,
            authorization_role: ViewReader::Session,
            qualifier: None,
            rule_transition: None,
            recursive_state: Some(RecursiveStateSubst {
                qualifier,
                names: &decoration.column_names[decoration.visible..],
                types: decoration.state_types,
            }),
            execution: None,
        },
        arena,
    )?;
    let state_count = decoration.column_names.len() - decoration.visible;
    if select.items.len() + state_count > MAX_PROJ {
        return Err(sql_err!(
            sqlstate::TOO_MANY_COLUMNS,
            "target lists can have at most 1664 entries"
        ));
    }
    let mut items = [SelectItem::Wildcard(crate::sql::ast::SourcePosition::UNKNOWN); MAX_PROJ];
    items[..select.items.len()].copy_from_slice(select.items);
    for state in 0..state_count {
        let ctype = decoration.state_types[state];
        let expression = arena
            .alloc(Expr::RecursiveState {
                qualifier,
                index: state as u8,
                ctype,
            })
            .map_err(|_| arena_full())?;
        items[select.items.len() + state] = SelectItem::Expr {
            expression,
            alias: None,
        };
    }
    let items = arena
        .alloc_slice_copy(&items[..select.items.len() + state_count])
        .map_err(|_| arena_full())?;
    let select = arena
        .alloc(Select { items, ..*select })
        .map_err(|_| arena_full())?;
    arena
        .alloc(SetTree::Select(&*select))
        .map(|tree| &*tree)
        .map_err(|_| arena_full())
}

/// Only the output schema is needed by Describe; no CTE may change data here.
fn describe_dml_materialized<'a>(
    cte: &'a Cte<'a>,
    statement: &Stmt<'_>,
    storage: &Storage,
    txid: u32,
    arena: &'a Arena,
) -> Result<&'a MaterializedCte<'a>, SqlError> {
    let mut described = [ColDesc::new("", 0, 0); MAX_PROJ];
    let count = crate::sql::exec::describe_data_modification_columns(
        storage,
        statement,
        arena,
        txid,
        &mut described,
        crate::sql::exec::DescriptionTarget::Sql,
    )?;
    if cte.columns.len() > count {
        return Err(sql_err!(
            sqlstate::INVALID_COLUMN_REFERENCE,
            "WITH query \"{}\" has {} columns available but {} columns specified",
            cte.name,
            count,
            cte.columns.len()
        ));
    }
    let mut names = [""; MAX_PROJ];
    for (index, name) in names.iter_mut().enumerate().take(count) {
        *name = arena
            .alloc_str(
                cte.columns
                    .get(index)
                    .copied()
                    .unwrap_or(described[index].name),
            )
            .map_err(|_| arena_full())?;
    }
    let column_names = arena
        .alloc_slice_copy(&names[..count])
        .map_err(|_| arena_full())?;
    let column_types = arena
        .alloc_slice_with(count, |index| {
            (
                described[index].type_oid,
                described[index].typlen,
                described[index].type_mod,
            )
        })
        .map_err(|_| arena_full())?;
    let column_collations = arena
        .alloc_slice_with(count, |index| described[index].collation)
        .map_err(|_| arena_full())?;
    arena
        .alloc(MaterializedCte {
            column_names,
            column_types,
            column_collations,
            definition: None,
            source: crate::sql::ast::MaterializedCteSource::Inline(&[]),
        })
        .map(|relation| &*relation)
        .map_err(|_| arena_full())
}

fn describe_recursive_materialized<'a>(
    cte: &'a Cte<'a>,
    outer: Subst<'_, 'a, '_, '_>,
    storage: &Storage,
    txid: u32,
    arena: &'a Arena,
) -> Result<&'a MaterializedCte<'a>, SqlError> {
    let (base_tree, recursive_tree, _) = recursive_parts(cte.query, cte.name)?;
    let base_tree = subst_set_tree(base_tree, outer, arena)?;
    let mut described = [ColDesc::new("", 0, 0); MAX_PROJ];
    let ncols = describe_set_body(
        storage,
        base_tree,
        txid,
        &mut described,
        arena,
        None,
        crate::sql::exec::DescriptionTarget::Sql,
        None,
    )?;
    if cte.columns.len() > ncols {
        return Err(sql_err!(
            sqlstate::INVALID_COLUMN_REFERENCE,
            "WITH query \"{}\" has {} columns available but {} columns specified",
            cte.name,
            ncols,
            cte.columns.len()
        ));
    }
    let mut names = [""; MAX_PROJ];
    for (index, output) in names.iter_mut().enumerate().take(ncols) {
        *output = arena
            .alloc_str(
                cte.columns
                    .get(index)
                    .copied()
                    .unwrap_or(described[index].name),
            )
            .map_err(|_| arena_full())?;
    }
    let base_column_names = arena
        .alloc_slice_copy(&names[..ncols])
        .map_err(|_| arena_full())?;
    let base_column_types = arena
        .alloc_slice_with(ncols, |index| {
            (
                described[index].type_oid,
                described[index].typlen,
                described[index].type_mod,
            )
        })
        .map_err(|_| arena_full())?;
    let base_column_collations = arena
        .alloc_slice_with(ncols, |index| described[index].collation)
        .map_err(|_| arena_full())?;
    let decoration = prepare_recursive_decoration(
        cte,
        base_column_names,
        base_column_types,
        base_column_collations,
        storage,
        txid,
        arena,
    )?;
    let recursive_tree = subst_set_tree(recursive_tree, outer, arena)?;
    reject_recursive_name_collision(cte, recursive_tree, decoration, storage, txid, arena)?;

    arena
        .alloc(MaterializedCte {
            column_names: decoration.column_names,
            column_types: decoration.column_types,
            column_collations: decoration.column_collations,
            definition: None,
            source: crate::sql::ast::MaterializedCteSource::Inline(&[]),
        })
        .map(|relation| &*relation)
        .map_err(|_| arena_full())
}

/// Wraps a set tree as a `Select` (a lone leaf is returned as-is).
fn wrap_set_tree<'a>(tree: &'a SetTree<'a>, arena: &'a Arena) -> Result<&'a Select<'a>, SqlError> {
    if let SetTree::Select(s) = tree {
        return Ok(s);
    }
    let sel = Select {
        items: &[],
        distinct: false,
        distinct_on: &[],
        windows: &[],
        from: None,
        where_clause: None,
        group_by: &[],
        grouping_set_quantifier: crate::sql::ast::GroupingSetQuantifier::All,
        grouping_sets: &[],
        having: None,
        order_by: &[],
        limit: None,
        offset: None,
        with_ties: false,
        with: &[],
        set_body: Some(tree),
        locking: &[],
    };
    Ok(&*arena.alloc(sel).map_err(|_| arena_full())?)
}

fn external_recursive_tree(
    tree: &SetTree<'_>,
    storage: &Storage,
    txid: u32,
    arena: &Arena,
    params: &[Datum<'_>],
    sequences: Option<&dyn SequenceAccess>,
    sorted: bool,
) -> Result<Option<crate::sql::external::ExternalRun>, SqlError> {
    let mut sorter = storage.external_sorter()?;
    sorter.reset();
    let mut compare = |left: &[u8], right: &[u8]| {
        if sorted {
            crate::sql::exec::compare_projected_rows(storage, txid, left, right)
        } else {
            Ok(core::cmp::Ordering::Equal)
        }
    };
    external_set_body_into(
        storage,
        txid,
        tree,
        &[],
        None,
        None,
        false,
        arena,
        params,
        sequences,
        &mut |values| {
            storage
                .with_block_store(|blocks| {
                    sorter.push_projected_by(
                        blocks,
                        values.len(),
                        |column| values[column],
                        &mut compare,
                    )
                })
                .expect("recursive work table has a block store")
        },
        None,
    )?;
    storage
        .with_block_store(|blocks| sorter.finish(blocks, &mut compare))
        .expect("recursive work table has a block store")
}

#[expect(
    clippy::too_many_arguments,
    reason = "the recursive run decorator receives its snapshot and run state explicitly"
)]
fn external_decorate_recursive_run(
    storage: &Storage,
    txid: u32,
    input: Option<crate::sql::external::ExternalRun>,
    decoration: RecursiveDecoration<'_>,
    base: bool,
    sorted: bool,
    exclude_cycles: bool,
    arena: &Arena,
) -> Result<Option<crate::sql::external::ExternalRun>, SqlError> {
    let Some(input) = input else {
        return Ok(None);
    };
    let mut reader = storage.external_run_reader()?;
    let mut sorter = storage.external_sorter()?;
    sorter.reset();
    let mut compare = |left: &[u8], right: &[u8]| {
        if sorted {
            crate::sql::exec::compare_projected_rows(storage, txid, left, right)
        } else {
            Ok(core::cmp::Ordering::Equal)
        }
    };
    storage
        .with_block_store(|blocks| reader.start(blocks, input))
        .expect("recursive work table has a block store")?;
    while let Some(source) = reader.row() {
        let mark = arena.mark();
        let (row, is_cycle) = decorate_recursive_row(source, decoration, base, arena)?;
        if !exclude_cycles || !is_cycle {
            storage
                .with_block_store(|blocks| sorter.push_encoded(blocks, row, &mut compare))
                .expect("recursive work table has a block store")?;
        }
        storage
            .with_block_store(|blocks| reader.advance(blocks))
            .expect("recursive work table has a block store")?;
        // The external sorter copied the encoded row into its fixed buffer or
        // an immutable run block. No decoded record/path scratch escapes.
        unsafe { arena.rewind_to(mark) };
    }
    storage
        .with_block_store(|blocks| sorter.finish(blocks, &mut compare))
        .expect("recursive work table has a block store")
}

fn external_recursive_noncycle_run(
    storage: &Storage,
    txid: u32,
    input: Option<crate::sql::external::ExternalRun>,
    decoration: RecursiveDecoration<'_>,
    sorted: bool,
    arena: &Arena,
) -> Result<Option<crate::sql::external::ExternalRun>, SqlError> {
    let Some(cycle) = decoration.cycle else {
        return Ok(input);
    };
    let Some(input) = input else {
        return Ok(None);
    };
    let mut reader = storage.external_run_reader()?;
    let mut sorter = storage.external_sorter()?;
    sorter.reset();
    let mut compare = |left: &[u8], right: &[u8]| {
        if sorted {
            crate::sql::exec::compare_projected_rows(storage, txid, left, right)
        } else {
            Ok(core::cmp::Ordering::Equal)
        }
    };
    storage
        .with_block_store(|blocks| reader.start(blocks, input))
        .expect("recursive work table has a block store")?;
    while let Some(row) = reader.row() {
        let mark = arena.mark();
        let cycle_mark = crate::sql::exec::decode_projected_col_record(row, cycle.mark, arena)?;
        if crate::sql::eval::membership_eq(&cycle_mark, &cycle.mark_value, None)? != Some(true) {
            storage
                .with_block_store(|blocks| sorter.push_encoded(blocks, row, &mut compare))
                .expect("recursive work table has a block store")?;
        }
        storage
            .with_block_store(|blocks| reader.advance(blocks))
            .expect("recursive work table has a block store")?;
        unsafe { arena.rewind_to(mark) };
    }
    storage
        .with_block_store(|blocks| sorter.finish(blocks, &mut compare))
        .expect("recursive work table has a block store")
}

fn external_unique_run(
    storage: &Storage,
    txid: u32,
    run: Option<crate::sql::external::ExternalRun>,
) -> Result<Option<crate::sql::external::ExternalRun>, SqlError> {
    let Some(run) = run else {
        return Ok(None);
    };
    let mut reader = storage.external_run_reader()?;
    let mut sorter = storage.external_sorter()?;
    sorter.reset();
    let mut compare = |left: &[u8], right: &[u8]| {
        crate::sql::exec::compare_projected_rows(storage, txid, left, right)
    };
    storage
        .with_block_store(|blocks| reader.start(blocks, run))
        .expect("recursive work table has a block store")?;
    while reader.row().is_some() {
        let keep = {
            let context = reader.context().expect("checked");
            match context.previous {
                Some(previous) => {
                    !crate::sql::exec::compare_projected_rows(storage, txid, previous, context.row)?
                        .is_eq()
                }
                None => true,
            }
        };
        if keep {
            let row = reader.row().expect("checked");
            storage
                .with_block_store(|blocks| sorter.push_encoded(blocks, row, &mut compare))
                .expect("recursive work table has a block store")?;
        }
        storage
            .with_block_store(|blocks| reader.advance(blocks))
            .expect("recursive work table has a block store")?;
    }
    storage
        .with_block_store(|blocks| sorter.finish(blocks, &mut compare))
        .expect("recursive work table has a block store")
}

fn external_recursive_difference(
    storage: &Storage,
    txid: u32,
    candidates: Option<crate::sql::external::ExternalRun>,
    seen: Option<crate::sql::external::ExternalRun>,
) -> Result<Option<crate::sql::external::ExternalRun>, SqlError> {
    let Some(candidates) = candidates else {
        return Ok(None);
    };
    let Some(seen) = seen else {
        return Ok(Some(candidates));
    };
    let mut candidate_reader = storage.external_run_reader()?;
    let mut seen_reader = storage.external_run_reader()?;
    let mut sorter = storage.external_sorter()?;
    sorter.reset();
    let mut compare = |left: &[u8], right: &[u8]| {
        crate::sql::exec::compare_projected_rows(storage, txid, left, right)
    };
    storage
        .with_block_store(|blocks| candidate_reader.start(blocks, candidates))
        .expect("recursive work table has a block store")?;
    storage
        .with_block_store(|blocks| seen_reader.start(blocks, seen))
        .expect("recursive work table has a block store")?;
    let compare_seen = |seen: Option<&[u8]>, candidate: &[u8]| {
        seen.map(|seen| crate::sql::exec::compare_projected_rows(storage, txid, seen, candidate))
            .transpose()
    };
    while let Some(candidate) = candidate_reader.row() {
        while compare_seen(seen_reader.row(), candidate)?.is_some_and(|order| order.is_lt()) {
            storage
                .with_block_store(|blocks| seen_reader.advance(blocks))
                .expect("recursive work table has a block store")?;
        }
        if !compare_seen(seen_reader.row(), candidate)?.is_some_and(|order| order.is_eq()) {
            storage
                .with_block_store(|blocks| sorter.push_encoded(blocks, candidate, &mut compare))
                .expect("recursive work table has a block store")?;
        }
        storage
            .with_block_store(|blocks| candidate_reader.advance(blocks))
            .expect("recursive work table has a block store")?;
    }
    storage
        .with_block_store(|blocks| sorter.finish(blocks, &mut compare))
        .expect("recursive work table has a block store")
}

fn external_recursive_union(
    storage: &Storage,
    txid: u32,
    left: Option<crate::sql::external::ExternalRun>,
    right: Option<crate::sql::external::ExternalRun>,
    sorted: bool,
) -> Result<Option<crate::sql::external::ExternalRun>, SqlError> {
    let mut sorter = storage.external_sorter()?;
    sorter.reset();
    let mut compare = |left: &[u8], right: &[u8]| {
        if sorted {
            crate::sql::exec::compare_projected_rows(storage, txid, left, right)
        } else {
            Ok(core::cmp::Ordering::Equal)
        }
    };
    let mut reader = storage.external_run_reader()?;
    for run in [left, right].into_iter().flatten() {
        storage
            .with_block_store(|blocks| reader.start(blocks, run))
            .expect("recursive work table has a block store")?;
        while let Some(row) = reader.row() {
            storage
                .with_block_store(|blocks| sorter.push_encoded(blocks, row, &mut compare))
                .expect("recursive work table has a block store")?;
            storage
                .with_block_store(|blocks| reader.advance(blocks))
                .expect("recursive work table has a block store")?;
        }
    }
    storage
        .with_block_store(|blocks| sorter.finish(blocks, &mut compare))
        .expect("recursive work table has a block store")
}

/// Materializes a self-referencing recursive CTE to its fixpoint: the
/// non-recursive term's rows first, then the recursive term evaluated
/// repeatedly with the CTE name bound to the previous iteration's rows,
/// accumulating until an iteration adds nothing (UNION deduplicates against
/// everything seen; UNION ALL keeps duplicates and stops on an empty
/// iteration). Row storage is arena-bounded: runaway recursion fails loudly
/// with arena exhaustion, and the statement timeout is honored per iteration.
fn materialize_recursive<'a>(
    cte: &'a Cte<'a>,
    outer: Subst<'_, 'a, '_, '_>,
    storage: &Storage,
    txid: u32,
    arena: &'a Arena,
    params: &[Datum<'a>],
    sequences: Option<&dyn SequenceAccess>,
) -> Result<&'a MaterializedCte<'a>, SqlError> {
    let (base_tree, recursive_tree, union_all) = recursive_parts(cte.query, cte.name)?;
    // References to earlier CTEs inline now; the self-reference stays a bare
    // table name (it is not in `outer`'s bindings) for per-iteration binding.
    let base_tree = subst_set_tree(base_tree, outer, arena)?;
    let mut recursive_tree = subst_set_tree(recursive_tree, outer, arena)?;
    let total = set_tree_references(recursive_tree, cte.name);
    let direct = direct_references(recursive_tree, cte.name);
    if total > direct {
        return Err(sql_err!(
            crate::sql::eval::sqlstate::INVALID_RECURSION,
            "recursive reference to query \"{}\" must not appear within a subquery",
            cte.name
        ));
    }
    if direct > 1 {
        return Err(sql_err!(
            crate::sql::eval::sqlstate::INVALID_RECURSION,
            "recursive reference to query \"{}\" must not appear more than once",
            cte.name
        ));
    }
    // Column names and types come from the non-recursive term, with the CTE's
    // rename list applied.
    let mut described = [ColDesc::new("", 0, 0); MAX_PROJ];
    let ncols = describe_set_body(
        storage,
        base_tree,
        txid,
        &mut described,
        arena,
        None,
        crate::sql::exec::DescriptionTarget::Sql,
        None,
    )?;
    if cte.columns.len() > ncols {
        return Err(sql_err!(
            sqlstate::INVALID_COLUMN_REFERENCE,
            "WITH query \"{}\" has {} columns available but {} columns specified",
            cte.name,
            ncols,
            cte.columns.len()
        ));
    }
    let base_column_names: &'a [&'a str] = {
        let mut names: [&str; MAX_PROJ] = [""; MAX_PROJ];
        for (i, slot) in names.iter_mut().enumerate().take(ncols) {
            let name = cte.columns.get(i).copied().unwrap_or(described[i].name);
            // A described base-table column name may borrow the catalog.
            // Recursive CTE metadata survives past that immutable borrow, so
            // own every output name in the statement arena.
            *slot = arena.alloc_str(name).map_err(|_| arena_full())?;
        }
        arena
            .alloc_slice_copy(&names[..ncols])
            .map_err(|_| arena_full())?
    };
    let base_column_types: &'a [(i32, i16, i32)] = {
        let mut types = [(0i32, 0i16, -1i32); MAX_PROJ];
        for (i, slot) in types.iter_mut().enumerate().take(ncols) {
            *slot = (
                described[i].type_oid,
                described[i].typlen,
                described[i].type_mod,
            );
        }
        arena
            .alloc_slice_copy(&types[..ncols])
            .map_err(|_| arena_full())?
    };
    let base_column_collations = arena
        .alloc_slice_with(ncols, |index| described[index].collation)
        .map_err(|_| arena_full())?;
    let mut working_column_names = [""; MAX_PROJ];
    working_column_names[..ncols].copy_from_slice(base_column_names);
    if let Some(aliases) =
        recursive_tree_reference(recursive_tree, cte.name).and_then(|reference| reference.col_alias)
    {
        if aliases.len() > ncols {
            return Err(sql_err!(
                sqlstate::INVALID_COLUMN_REFERENCE,
                "table \"{}\" has {} columns available but {} columns specified",
                cte.name,
                ncols,
                aliases.len()
            ));
        }
        working_column_names[..aliases.len()].copy_from_slice(aliases);
    }
    let decoration = prepare_recursive_decoration(
        cte,
        base_column_names,
        base_column_types,
        base_column_collations,
        storage,
        txid,
        arena,
    )?;
    reject_recursive_name_collision(cte, recursive_tree, decoration, storage, txid, arena)?;
    let decorated = decoration.column_names.len() != decoration.visible;
    if decorated {
        recursive_tree =
            append_recursive_state(recursive_tree, cte.name, decoration, storage, txid, arena)?;
    }
    let column_names = decoration.column_names;
    let column_types = decoration.column_types;
    let column_collations = decoration.column_collations;
    working_column_names[ncols..column_names.len()].copy_from_slice(&column_names[ncols..]);
    let working_definition = super::scope::materialized_definition(
        storage,
        txid,
        cte.name,
        &working_column_names[..column_names.len()],
        column_types,
        column_collations,
        arena,
    )?;
    let working_definition = crate::storage::RelationDefinition {
        n_columns: ncols,
        ..working_definition
    };
    let definition = if working_column_names[..column_names.len()] == column_names[..] {
        crate::storage::RelationDefinition {
            n_columns: column_names.len(),
            ..working_definition
        }
    } else {
        super::scope::materialized_definition(
            storage,
            txid,
            cte.name,
            column_names,
            column_types,
            column_collations,
            arena,
        )?
    };

    if storage.spill_attached() {
        let base_raw = external_recursive_tree(
            base_tree, storage, txid, arena, params, sequences, !union_all,
        )?;
        let base = if decorated {
            external_decorate_recursive_run(
                storage, txid, base_raw, decoration, true, !union_all, false, arena,
            )?
        } else {
            base_raw
        };
        let mut all = if union_all {
            base
        } else {
            external_unique_run(storage, txid, base)?
        };
        let mut working = all;
        let working_source = arena.alloc_atomic_usize(0).map_err(|_| arena_full())?;
        let working_cte = arena
            .alloc(MaterializedCte {
                column_names: base_column_names,
                column_types: base_column_types,
                column_collations: base_column_collations,
                definition: Some(working_definition),
                source: crate::sql::ast::MaterializedCteSource::RecursiveExternal(working_source),
            })
            .map_err(|_| arena_full())?;
        let binding = [(cte.name, &*working_cte)];
        let context = Subst {
            ctes: &[],
            materialized: &binding,
            storage,
            txid: outer.txid,
            depth: 0,
            path: None,
            dependencies: None,
            authorization_role: ViewReader::Session,
            qualifier: None,
            rule_transition: None,
            recursive_state: None,
            execution: outer.execution,
        };
        let step_tree = subst_set_tree(recursive_tree, context, arena)?;
        let mut step_desc = [ColDesc::new("", 0, 0); MAX_PROJ];
        let stepn = describe_set_body(
            storage,
            step_tree,
            txid,
            &mut step_desc,
            arena,
            None,
            crate::sql::exec::DescriptionTarget::Sql,
            None,
        )?;
        let expected_step_columns = ncols + decoration.state_types.len();
        if stepn != expected_step_columns {
            return Err(sql_err!(
                sqlstate::SYNTAX_ERROR,
                "each UNION query must have the same number of columns"
            ));
        }
        for column in 0..ncols {
            if step_desc[column].type_oid != base_column_types[column].0 {
                return Err(sql_err!(
                    sqlstate::DATATYPE_MISMATCH,
                    "recursive query \"{}\" column {} has type {} in non-recursive term but type {} overall",
                    cte.name,
                    column + 1,
                    base_column_types[column].0,
                    step_desc[column].type_oid
                ));
            }
        }
        while working.is_some_and(|run| run.rows() > 0) {
            check_timeout()?;
            let run = arena
                .alloc(working.expect("loop condition requires a run"))
                .map_err(|_| arena_full())?;
            working_source.store(
                run as *const crate::sql::external::ExternalRun as usize,
                core::sync::atomic::Ordering::Relaxed,
            );
            let iteration_mark = arena.mark();
            let raw_candidates = external_recursive_tree(
                step_tree, storage, txid, arena, params, sequences, !union_all,
            )?;
            let candidates = if decorated {
                external_decorate_recursive_run(
                    storage,
                    txid,
                    raw_candidates,
                    decoration,
                    false,
                    !union_all,
                    false,
                    arena,
                )?
            } else {
                raw_candidates
            };
            let fresh = if union_all {
                candidates
            } else {
                let unique = external_unique_run(storage, txid, candidates)?;
                external_recursive_difference(storage, txid, unique, all)?
            };
            if fresh.is_none_or(|run| run.rows() == 0) {
                unsafe { arena.rewind_to(iteration_mark) };
                break;
            }
            let next_working = external_recursive_noncycle_run(
                storage, txid, fresh, decoration, !union_all, arena,
            )?;
            all = external_recursive_union(storage, txid, all, fresh, !union_all)?;
            working = next_working;
            // Completed runs retain only block identities; all query scopes,
            // decoded records, and sorter scratch from this iteration are dead.
            unsafe { arena.rewind_to(iteration_mark) };
        }
        return Ok(&*arena
            .alloc(MaterializedCte {
                column_names,
                column_types,
                column_collations,
                definition: Some(definition),
                source: crate::sql::ast::MaterializedCteSource::External(all),
            })
            .map_err(|_| arena_full())?);
    }

    // Base rows; UNION (without ALL) deduplicates them among themselves.
    let (base_rows, _, _) =
        materialize_set_body(storage, txid, base_tree, arena, params, sequences)?;
    const EMPTY: &[u8] = &[];
    let base_rows = if decorated {
        let rows = arena
            .alloc_slice_with(base_rows.len(), |_| EMPTY)
            .map_err(|_| arena_full())?;
        for (output, &source) in rows.iter_mut().zip(base_rows) {
            *output = decorate_recursive_row(source, decoration, true, arena)?.0;
        }
        &*rows
    } else {
        base_rows
    };
    let mut all_rows: &'a [&'a [u8]] = if union_all {
        base_rows
    } else {
        let deduped = arena
            .alloc_slice_with(base_rows.len(), |_| EMPTY)
            .map_err(|_| arena_full())?;
        let mut kept = 0usize;
        for &r in base_rows.iter() {
            if !contains_projected_row(storage, txid, &deduped[..kept], r)? {
                deduped[kept] = r;
                kept += 1;
            }
        }
        &deduped[..kept]
    };
    let mut working: &'a [&'a [u8]] = all_rows;

    // Bind the recursive reference once. Only the work-table row slice changes
    // between iterations; rebuilding the substituted AST each time retains a
    // full plan per iteration in the bump arena and makes shallow recursion
    // exhaust memory before row capacity does.
    let working_source = arena.alloc_atomic_usize(0).map_err(|_| arena_full())?;
    let working_cte = arena
        .alloc(MaterializedCte {
            column_names: base_column_names,
            column_types: base_column_types,
            column_collations: base_column_collations,
            definition: Some(working_definition),
            source: crate::sql::ast::MaterializedCteSource::RecursiveInline(working_source),
        })
        .map_err(|_| arena_full())?;
    let binding = [(cte.name, &*working_cte)];
    let context = Subst {
        ctes: &[],
        materialized: &binding,
        storage,
        txid: outer.txid,
        depth: 0,
        path: None,
        dependencies: None,
        authorization_role: ViewReader::Session,
        qualifier: None,
        rule_transition: None,
        recursive_state: None,
        execution: outer.execution,
    };
    let step_tree = subst_set_tree(recursive_tree, context, arena)?;
    let mut step_desc = [ColDesc::new("", 0, 0); MAX_PROJ];
    let stepn = describe_set_body(
        storage,
        step_tree,
        txid,
        &mut step_desc,
        arena,
        None,
        crate::sql::exec::DescriptionTarget::Sql,
        None,
    )?;
    let expected_step_columns = ncols + decoration.state_types.len();
    if stepn != expected_step_columns {
        return Err(sql_err!(
            sqlstate::SYNTAX_ERROR,
            "each UNION query must have the same number of columns"
        ));
    }
    for c in 0..ncols {
        if step_desc[c].type_oid != base_column_types[c].0 {
            return Err(sql_err!(
                sqlstate::DATATYPE_MISMATCH,
                "recursive query \"{}\" column {} has type {} in non-recursive term but type {} overall",
                cte.name,
                c + 1,
                base_column_types[c].0,
                step_desc[c].type_oid
            ));
        }
    }

    while !working.is_empty() {
        check_timeout()?;
        let source = arena
            .alloc(crate::sql::ast::MaterializedCteInlineSource {
                address: working.as_ptr() as usize,
                length: working.len(),
            })
            .map_err(|_| arena_full())?;
        working_source.store(
            source as *const crate::sql::ast::MaterializedCteInlineSource as usize,
            core::sync::atomic::Ordering::Relaxed,
        );
        let (step_rows, _, _) =
            materialize_set_body(storage, txid, step_tree, arena, params, sequences)?;
        let step_rows = if decorated {
            let rows = arena
                .alloc_slice_with(step_rows.len(), |_| EMPTY)
                .map_err(|_| arena_full())?;
            for (output, &source) in rows.iter_mut().zip(step_rows) {
                *output = decorate_recursive_row(source, decoration, false, arena)?.0;
            }
            &*rows
        } else {
            step_rows
        };
        // Keep the rows this iteration added: all of them under UNION ALL, only
        // never-seen ones under UNION.
        let fresh: &'a [&'a [u8]] = if union_all {
            step_rows
        } else {
            let kept_rows = arena
                .alloc_slice_with(step_rows.len(), |_| EMPTY)
                .map_err(|_| arena_full())?;
            let mut kept = 0usize;
            for &r in step_rows.iter() {
                if !contains_projected_row(storage, txid, all_rows, r)?
                    && !contains_projected_row(storage, txid, &kept_rows[..kept], r)?
                {
                    kept_rows[kept] = r;
                    kept += 1;
                }
            }
            &kept_rows[..kept]
        };
        if fresh.is_empty() {
            break;
        }
        let combined = arena
            .alloc_slice_with(all_rows.len() + fresh.len(), |_| EMPTY)
            .map_err(|_| arena_full())?;
        combined[..all_rows.len()].copy_from_slice(all_rows);
        combined[all_rows.len()..].copy_from_slice(fresh);
        all_rows = combined;
        working = recursive_noncycle_rows(fresh, decoration, arena)?;
    }

    Ok(&*arena
        .alloc(MaterializedCte {
            column_names,
            column_types,
            column_collations,
            definition: Some(definition),
            source: crate::sql::ast::MaterializedCteSource::Inline(all_rows),
        })
        .map_err(|_| arena_full())?)
}

/// Whether `rows` holds a row equal to `row` under SQL equality; recursive
/// UNION treats `1.0` and `1.00` as one row.
fn contains_projected_row(
    storage: &Storage,
    txid: u32,
    rows: &[&[u8]],
    row: &[u8],
) -> Result<bool, SqlError> {
    for &candidate in rows {
        if crate::sql::exec::compare_projected_rows(storage, txid, candidate, row)?.is_eq() {
            return Ok(true);
        }
    }
    Ok(false)
}

fn subst_select<'a>(
    select: &'a Select<'a>,
    context: Subst<'_, 'a, '_, '_>,
    arena: &'a Arena,
) -> Result<&'a Select<'a>, SqlError> {
    if select.with.is_empty() {
        return subst_select_body(select, context, arena);
    }
    if let Some(execution) = context.execution {
        return with_exec_context(
            select.with,
            context.storage,
            context.txid,
            arena,
            execution,
            &[],
            &[],
            context.ctes,
            context.materialized,
            context.depth,
            context.path,
            context.dependencies,
            context.authorization_role,
            None,
            |name| body_references(select, name),
            |inner| subst_select_body(select, inner, arena),
        );
    }

    let mut resolved = ArenaList::new(arena);
    for &index in cte_order(select.with, arena)? {
        let cte = &select.with[index];
        if select.with[..index]
            .iter()
            .any(|prior| prior.name == cte.name)
        {
            return Err(sql_err!(
                sqlstate::DUPLICATE_ALIAS,
                "WITH query name \"{}\" specified more than once",
                cte.name
            ));
        }
        if cte.dml.is_some() {
            return Err(sql_err!(
                sqlstate::FEATURE_NOT_SUPPORTED,
                "WITH clause containing a data-modifying statement must be at the top level"
            ));
        }
        let mut scoped = ArenaList::new(arena);
        for binding in resolved.as_slice() {
            scoped.push(*binding).map_err(|_| arena_full())?;
        }
        for binding in context.ctes {
            if select.with.iter().any(|local| local.name == binding.0) {
                continue;
            }
            scoped.push(*binding).map_err(|_| arena_full())?;
        }
        let child = Subst {
            ctes: scoped.as_slice(),
            materialized: materialized_not_shadowed(
                if cte.recursive {
                    select.with
                } else {
                    &select.with[..index]
                },
                context.materialized,
                arena,
            )?,
            ..context
        };
        let query = if cte.recursive && select_references(cte.query, cte.name) > 0 {
            let (base, _, _) = recursive_parts(cte.query, cte.name)?;
            subst_select(wrap_set_tree(base, arena)?, child, arena)?
        } else {
            subst_select(cte.query, child, arena)?
        };
        resolved
            .push((cte.name, query, cte.columns))
            .map_err(|_| arena_full())?;
    }
    let mut scoped = ArenaList::new(arena);
    for binding in resolved.as_slice() {
        scoped.push(*binding).map_err(|_| arena_full())?;
    }
    for binding in context.ctes {
        if select.with.iter().any(|local| local.name == binding.0) {
            continue;
        }
        scoped.push(*binding).map_err(|_| arena_full())?;
    }
    subst_select_body(
        select,
        Subst {
            ctes: scoped.as_slice(),
            materialized: materialized_not_shadowed(select.with, context.materialized, arena)?,
            ..context
        },
        arena,
    )
}

fn subst_select_body<'a>(
    s: &'a Select<'a>,
    mut context: Subst<'_, 'a, '_, '_>,
    arena: &'a Arena,
) -> Result<&'a Select<'a>, SqlError> {
    if let (Some(qualifier), Some(from)) = (context.qualifier, s.from)
        && from_shadows_qualifier(&from, qualifier.from)
    {
        context.qualifier = None;
    }
    let from = match &s.from {
        Some(f) => Some(subst_from(f, context, arena)?),
        None => None,
    };
    let items = subst_select_items(s.items, context, arena)?;
    let distinct_on = subst_expr_slice(s.distinct_on, context, arena)?;
    let group_by = subst_expr_slice(s.group_by, context, arena)?;
    // Grouping-set bitmasks index into `group_by`; substitution preserves the
    // column order and count, so they carry over unchanged.
    let grouping_sets = arena
        .alloc_slice_copy(s.grouping_sets)
        .map_err(|_| arena_full())?;
    let order_by = subst_order_by(s.order_by, context, arena)?;
    let set_body = match s.set_body {
        Some(tree) => Some(subst_set_tree(tree, context, arena)?),
        None => None,
    };
    let new = Select {
        items,
        distinct: s.distinct,
        distinct_on,
        windows: s.windows,
        from,
        where_clause: opt_subst(s.where_clause, context, arena)?,
        group_by,
        grouping_set_quantifier: s.grouping_set_quantifier,
        grouping_sets,
        having: opt_subst(s.having, context, arena)?,
        order_by,
        limit: opt_subst(s.limit, context, arena)?,
        offset: opt_subst(s.offset, context, arena)?,
        with_ties: s.with_ties,
        with: &[],
        set_body,
        locking: s.locking,
    };
    Ok(&*arena.alloc(new).map_err(|_| arena_full())?)
}

fn subst_order_by<'a>(
    source: &'a [OrderBy<'a>],
    context: Subst<'_, 'a, '_, '_>,
    arena: &'a Arena,
) -> Result<&'a [OrderBy<'a>], SqlError> {
    let mut order: ArenaList<'a, OrderBy<'a>> = ArenaList::new(arena);
    for item in source {
        order
            .push(OrderBy {
                expression: subst_expr(item.expression, context, arena)?,
                ..*item
            })
            .map_err(|_| arena_full())?;
    }
    Ok(order.as_slice())
}

fn from_shadows_qualifier(from: &FromClause<'_>, qualifier: &str) -> bool {
    let shadows = |table: &TableRef<'_>| table.alias.unwrap_or(table.table) == qualifier;
    shadows(&from.base) || from.joins.iter().any(|join| shadows(&join.table))
}

fn subst_select_items<'a>(
    source: &'a [SelectItem<'a>],
    context: Subst<'_, 'a, '_, '_>,
    arena: &'a Arena,
) -> Result<&'a [SelectItem<'a>], SqlError> {
    let mut items = [SelectItem::Wildcard(crate::sql::ast::SourcePosition::UNKNOWN); MAX_PROJ];
    let mut count = 0usize;
    for item in source {
        if let SelectItem::TableWildcard(qualifier) = item
            && let Some(transition) = context.rule_transition
            && (qualifier.eq_ignore_ascii_case("old") || qualifier.eq_ignore_ascii_case("new"))
        {
            let old = qualifier.eq_ignore_ascii_case("old");
            if (old && !transition.old_available) || (!old && !transition.new_available) {
                return Err(sql_err!(
                    sqlstate::INVALID_OBJECT_DEFINITION,
                    "ON {} rule cannot use {}",
                    if old { "INSERT" } else { "DELETE" },
                    if old { "OLD" } else { "NEW" }
                ));
            }
            for (index, name) in transition.names.iter().copied().enumerate() {
                let slot = items.get_mut(count).ok_or_else(|| {
                    sql_err!(
                        sqlstate::TOO_MANY_COLUMNS,
                        "target lists can have at most 1664 entries"
                    )
                })?;
                *slot = SelectItem::Expr {
                    expression: if old {
                        transition.old[index]
                    } else {
                        transition.new[index]
                    },
                    alias: Some(name),
                };
                count += 1;
            }
            continue;
        }
        let rewritten = match item {
            SelectItem::Wildcard(at) => SelectItem::Wildcard(*at),
            SelectItem::TableWildcard(qualifier) => {
                SelectItem::TableWildcard(match context.qualifier {
                    Some(rewrite) if *qualifier == rewrite.from => rewrite.to,
                    _ => qualifier,
                })
            }
            SelectItem::RecordStar(base) => {
                SelectItem::RecordStar(subst_expr(base, context, arena)?)
            }
            SelectItem::Expr { expression, alias } => SelectItem::Expr {
                expression: subst_expr(expression, context, arena)?,
                alias: *alias,
            },
        };
        let slot = items.get_mut(count).ok_or_else(|| {
            sql_err!(
                sqlstate::TOO_MANY_COLUMNS,
                "target lists can have at most 1664 entries"
            )
        })?;
        *slot = rewritten;
        count += 1;
    }
    arena
        .alloc_slice_copy(&items[..count])
        .map(|items| &*items)
        .map_err(|_| arena_full())
}

fn subst_assignments<'a>(
    source: &'a [(&'a str, &'a Expr<'a>)],
    context: Subst<'_, 'a, '_, '_>,
    arena: &'a Arena,
) -> Result<&'a [(&'a str, &'a Expr<'a>)], SqlError> {
    let mut assignments: ArenaList<'a, (&'a str, &'a Expr<'a>)> = ArenaList::new(arena);
    // A multi-column assignment projects several fields from one query node.
    // Preserve that node identity so subquery evaluation runs its effects once.
    let mut row_sources: ArenaList<'a, (&'a Expr<'a>, &'a Expr<'a>)> = ArenaList::new(arena);
    for (column, expression) in source.iter().copied() {
        let expression = match expression {
            Expr::RecordFieldIndex { base, index } => {
                let rewritten = row_sources
                    .as_slice()
                    .iter()
                    .find(|(original, _)| core::ptr::eq(*original, *base))
                    .map(|(_, rewritten)| *rewritten);
                let base = match rewritten {
                    Some(base) => base,
                    None => {
                        let rewritten = subst_expr(base, context, arena)?;
                        row_sources
                            .push((*base, rewritten))
                            .map_err(|_| arena_full())?;
                        rewritten
                    }
                };
                &*arena
                    .alloc(Expr::RecordFieldIndex {
                        base,
                        index: *index,
                    })
                    .map_err(|_| arena_full())?
            }
            _ => subst_expr(expression, context, arena)?,
        };
        assignments
            .push((column, expression))
            .map_err(|_| arena_full())?;
    }
    Ok(assignments.as_slice())
}

fn subst_on_conflict_targets<'a>(
    source: &'a [OnConflictTarget<'a>],
    context: Subst<'_, 'a, '_, '_>,
    arena: &'a Arena,
) -> Result<&'a [OnConflictTarget<'a>], SqlError> {
    let mut targets: ArenaList<'a, OnConflictTarget<'a>> = ArenaList::new(arena);
    for target in source {
        targets
            .push(OnConflictTarget {
                column: target.column,
                expression: subst_expr(target.expression, context, arena)?,
                expression_text: target.expression_text,
            })
            .map_err(|_| arena_full())?;
    }
    Ok(targets.as_slice())
}

fn subst_insert<'a>(
    statement: &'a Insert<'a>,
    context: Subst<'_, 'a, '_, '_>,
    arena: &'a Arena,
) -> Result<Insert<'a>, SqlError> {
    let mut rows: ArenaList<'a, &'a [&'a Expr<'a>]> = ArenaList::new(arena);
    for row in statement.rows {
        rows.push(subst_expr_slice(row, context, arena)?)
            .map_err(|_| arena_full())?;
    }
    let rows = rows.as_slice();
    let select = match statement.select {
        Some(select) => Some(subst_select(select, context, arena)?),
        None => None,
    };
    let on_conflict = match statement.on_conflict {
        Some(conflict) => Some(OnConflict {
            target: subst_on_conflict_targets(conflict.target, context, arena)?,
            constraint: conflict.constraint,
            update: match conflict.update {
                Some(assignments) => Some(subst_assignments(assignments, context, arena)?),
                None => None,
            },
            update_where: opt_subst(conflict.update_where, context, arena)?,
        }),
        None => None,
    };
    Ok(Insert {
        source_position: statement.source_position,
        table: rewrite_stored_relation_name(statement.table, context, arena)?,
        alias: statement.alias,
        columns: statement.columns,
        rows,
        select,
        on_conflict,
        returning: Returning {
            items: subst_select_items(statement.returning.items, context, arena)?,
            ..statement.returning
        },
        overriding: statement.overriding,
        values_defaults: statement.values_defaults,
    })
}

fn subst_update<'a>(
    statement: &'a Update<'a>,
    context: Subst<'_, 'a, '_, '_>,
    arena: &'a Arena,
) -> Result<Update<'a>, SqlError> {
    Ok(Update {
        source_position: statement.source_position,
        table: rewrite_stored_relation_name(statement.table, context, arena)?,
        inheritance: statement.inheritance,
        alias: statement.alias,
        assignments: subst_assignments(statement.assignments, context, arena)?,
        from: match statement.from {
            Some(from) => Some(
                &*arena
                    .alloc(subst_from(from, context, arena)?)
                    .map_err(|_| arena_full())?,
            ),
            None => None,
        },
        where_clause: opt_subst(statement.where_clause, context, arena)?,
        returning: Returning {
            items: subst_select_items(statement.returning.items, context, arena)?,
            ..statement.returning
        },
    })
}

fn subst_delete<'a>(
    statement: &'a Delete<'a>,
    context: Subst<'_, 'a, '_, '_>,
    arena: &'a Arena,
) -> Result<Delete<'a>, SqlError> {
    Ok(Delete {
        table: rewrite_stored_relation_name(statement.table, context, arena)?,
        inheritance: statement.inheritance,
        alias: statement.alias,
        using: match statement.using {
            Some(using) => Some(
                &*arena
                    .alloc(subst_from(using, context, arena)?)
                    .map_err(|_| arena_full())?,
            ),
            None => None,
        },
        where_clause: opt_subst(statement.where_clause, context, arena)?,
        returning: Returning {
            items: subst_select_items(statement.returning.items, context, arena)?,
            ..statement.returning
        },
    })
}

fn subst_merge<'a>(
    statement: &'a Merge<'a>,
    context: Subst<'_, 'a, '_, '_>,
    arena: &'a Arena,
) -> Result<Merge<'a>, SqlError> {
    let mut whens: ArenaList<'a, MergeWhen<'a>> = ArenaList::new(arena);
    for when in statement.whens {
        let cond = opt_subst(when.condition(), context, arena)?;
        whens
            .push(match *when {
                MergeWhen::Matched { action, .. } => MergeWhen::Matched {
                    cond,
                    action: match action {
                        MergeTargetAction::Update(assignments) => MergeTargetAction::Update(
                            subst_assignments(assignments, context, arena)?,
                        ),
                        MergeTargetAction::Delete => MergeTargetAction::Delete,
                        MergeTargetAction::DoNothing => MergeTargetAction::DoNothing,
                    },
                },
                MergeWhen::NotMatchedBySource { action, .. } => MergeWhen::NotMatchedBySource {
                    cond,
                    action: match action {
                        MergeTargetAction::Update(assignments) => MergeTargetAction::Update(
                            subst_assignments(assignments, context, arena)?,
                        ),
                        MergeTargetAction::Delete => MergeTargetAction::Delete,
                        MergeTargetAction::DoNothing => MergeTargetAction::DoNothing,
                    },
                },
                MergeWhen::NotMatchedByTarget { action, .. } => MergeWhen::NotMatchedByTarget {
                    cond,
                    action: match action {
                        MergeSourceAction::Insert {
                            columns,
                            values,
                            default_values,
                            overriding,
                        } => MergeSourceAction::Insert {
                            columns,
                            values: subst_expr_slice(values, context, arena)?,
                            default_values,
                            overriding,
                        },
                        MergeSourceAction::DoNothing => MergeSourceAction::DoNothing,
                    },
                },
            })
            .map_err(|_| arena_full())?;
    }
    Ok(Merge {
        source_position: statement.source_position,
        target: rewrite_stored_relation_name(statement.target, context, arena)?,
        target_inheritance: statement.target_inheritance,
        target_alias: statement.target_alias,
        source: subst_tableref(&statement.source, context, arena)?,
        on: subst_expr(statement.on, context, arena)?,
        whens: whens.as_slice(),
        returning: Returning {
            items: subst_select_items(statement.returning.items, context, arena)?,
            ..statement.returning
        },
    })
}

fn rewrite_stored_relation_name<'a>(
    name: crate::sql::ast::QualName<'a>,
    context: Subst<'_, 'a, '_, '_>,
    arena: &'a Arena,
) -> Result<crate::sql::ast::QualName<'a>, SqlError> {
    let Some(dependencies) = context.dependencies else {
        return Ok(name);
    };
    let Some(dependency) = dependencies.entries().iter().find(|dependency| {
        matches!(
            dependency.class,
            crate::storage::DependencyClass::Table | crate::storage::DependencyClass::View
        ) && dependency.referenced_schema.as_str() == name.schema.unwrap_or("")
            && dependency.referenced_name.as_str() == name.name
    }) else {
        return Ok(name);
    };
    let (schema, relation) = match context
        .storage
        .resolve_stored_query_relation(dependency, context.txid)?
    {
        crate::storage::ResolvedRelation::Table(slot) => {
            let definition = context.storage.table_def(slot, context.txid);
            (definition.schema, definition.name)
        }
        crate::storage::ResolvedRelation::View(slot) => {
            let definition = context.storage.view(slot);
            (
                definition.schema_for(context.txid),
                definition.name_for(context.txid),
            )
        }
        crate::storage::ResolvedRelation::Catalog => {
            unreachable!("stored physical relation binding")
        }
    };
    Ok(crate::sql::ast::QualName {
        schema: Some(arena.alloc_str(schema.as_str()).map_err(|_| arena_full())?),
        name: arena
            .alloc_str(relation.as_str())
            .map_err(|_| arena_full())?,
    })
}

/// Substitutes parameters through every leaf SELECT of a set-operation tree,
/// mirroring [`subst_select`] for a set-operator subquery body.
fn subst_set_tree<'a>(
    tree: &'a SetTree<'a>,
    context: Subst<'_, 'a, '_, '_>,
    arena: &'a Arena,
) -> Result<&'a SetTree<'a>, SqlError> {
    let out = match tree {
        SetTree::Select(s) => SetTree::Select(subst_select(s, context, arena)?),
        SetTree::Op {
            operator,
            all,
            left,
            right,
            syntax,
        } => SetTree::Op {
            operator: *operator,
            syntax: *syntax,
            all: *all,
            left: subst_set_tree(left, context, arena)?,
            right: subst_set_tree(right, context, arena)?,
        },
    };
    Ok(&*arena.alloc(out).map_err(|_| arena_full())?)
}

fn subst_from<'a>(
    f: &'a FromClause<'a>,
    context: Subst<'_, 'a, '_, '_>,
    arena: &'a Arena,
) -> Result<FromClause<'a>, SqlError> {
    let base = subst_tableref(&f.base, context, arena)?;
    let mut joins = crate::mem::arena::ArenaList::new(arena);
    for j in f.joins {
        joins
            .push(Join {
                table: subst_tableref(&j.table, context, arena)?,
                kind: j.kind,
                on: opt_subst(j.on, context, arena)?,
                using: j.using,
                natural: j.natural,
            })
            .map_err(|_| arena_full())?;
    }
    Ok(FromClause {
        base,
        joins: joins.as_slice(),
    })
}

fn subst_tableref<'a>(
    t: &TableRef<'a>,
    context: Subst<'_, 'a, '_, '_>,
    arena: &'a Arena,
) -> Result<TableRef<'a>, SqlError> {
    let rewritten = TableRef {
        sample: match t.sample {
            Some(sample) => Some(crate::sql::ast::TableSample {
                method: sample.method,
                percentage: subst_expr(sample.percentage, context, arena)?,
                repeatable: opt_subst(sample.repeatable, context, arena)?,
            }),
            None => None,
        },
        ..*t
    };
    let t = &rewritten;
    if let Some(functions) = t.rows_from {
        let mut rewritten: ArenaList<'a, TableRef<'a>> = ArenaList::new(arena);
        for function in functions {
            rewritten
                .push(subst_tableref(function, context, arena)?)
                .map_err(|_| arena_full())?;
        }
        return Ok(TableRef {
            rows_from: Some(rewritten.as_slice()),
            ..*t
        });
    }
    if let Some(arguments) = t.func_args {
        let written = match t.schema {
            Some(schema) => crate::stack_format!(192, "{}.{}", schema, t.table),
            None => crate::stack_format!(192, "{}", t.table),
        };
        let dependency = match context.dependencies {
            Some(dependencies) => super::dependencies::stored_routine_dependency_for_call(
                written.as_str(),
                arguments,
                t.func_argument_names,
                t.func_variadic,
                context.storage,
                context.txid,
                dependencies,
                arena,
            )?,
            None => None,
        };
        let Some(dependency) = dependency else {
            return Ok(TableRef {
                func_args: Some(subst_expr_slice(arguments, context, arena)?),
                ..*t
            });
        };
        let routine = context
            .storage
            .routine_for(dependency.slot as usize, context.txid);
        return Ok(TableRef {
            schema: Some(
                arena
                    .alloc_str(routine.schema_for(context.txid).as_str())
                    .map_err(|_| arena_full())?,
            ),
            table: arena
                .alloc_str(routine.name_for(context.txid).as_str())
                .map_err(|_| arena_full())?,
            func_args: Some(subst_expr_slice(arguments, context, arena)?),
            ..*t
        });
    }
    if let Some(sub) = t.subquery {
        return Ok(TableRef {
            subquery: Some(subst_select(sub, context, arena)?),
            ..*t
        });
    }
    // An unqualified name matching a materialized (recursive) CTE resolves to
    // its precomputed row set.
    if t.schema.is_none()
        && !t.is_function_source()
        && let Some((_, m)) = context
            .materialized
            .iter()
            .find(|(name, _)| *name == t.table)
    {
        return Ok(TableRef {
            schema: None,
            table: t.table,
            alias: Some(t.alias.unwrap_or(t.table)),
            subquery: None,
            func_args: None,
            func_argument_names: &[],
            func_variadic: false,
            func_source_position: crate::sql::ast::SourcePosition::UNKNOWN,
            rows_from: None,
            col_alias: t.col_alias,
            inheritance: t.inheritance,
            sample: t.sample,
            cte: Some(m),
            with_ordinality: false,
            lateral: false,
            authorization_role: None,
            bound_table: None,
            view_access: None,
            join_members: None,
        });
    }
    // An unqualified name matching a CTE becomes a derived table over the
    // (already-substituted) CTE query, exposed under its alias or CTE name.
    // The CTE's own column-rename list applies unless the reference carries an
    // explicit one (`FROM t AS x(c1, ...)`).
    if t.schema.is_none()
        && let Some((_, q, columns)) = context.ctes.iter().find(|(name, _, _)| *name == t.table)
    {
        let renames = t.col_alias.or(if columns.is_empty() {
            None
        } else {
            Some(columns)
        });
        return Ok(TableRef {
            schema: None,
            table: "",
            alias: Some(t.alias.unwrap_or(t.table)),
            subquery: Some(q),
            func_args: None,
            func_argument_names: &[],
            func_variadic: false,
            func_source_position: crate::sql::ast::SourcePosition::UNKNOWN,
            rows_from: None,
            col_alias: renames,
            inheritance: t.inheritance,
            sample: t.sample,
            cte: None,
            with_ordinality: false,
            lateral: false,
            authorization_role: None,
            bound_table: None,
            view_access: None,
            join_members: None,
        });
    }
    // A name resolving to a view (not shadowed by a CTE, and not out-resolved
    // by a table earlier in the path) expands to a derived table over the
    // view's stored SELECT, recursively expanded under the view creator's
    // search path.
    let (resolved, captured) = stored_relation_binding(
        t,
        context.storage,
        context.txid,
        context.path,
        context.dependencies,
    )?;
    if let Some(crate::storage::ResolvedRelation::View(slot)) = resolved {
        if context.txid != 0 && context.execution.is_some() {
            context.storage.lock_object(
                context.txid,
                crate::storage::ObjectLockTarget::View(slot),
                crate::sql::ast::TableLockMode::AccessShare,
                false,
            )?;
        }
        if context.depth >= MAX_VIEW_DEPTH {
            return Err(sql_err!(
                sqlstate::FEATURE_NOT_SUPPORTED,
                "view \"{}\" nests too deeply (or references itself)",
                t.table
            ));
        }
        let view = context.storage.view(slot);
        let view_object = crate::storage::AccessObject {
            class: crate::storage::AccessClass::View,
            slot: slot as u16,
        };
        let requester = match context.authorization_role {
            ViewReader::Role(role) => Some(role as usize),
            ViewReader::Session => Some(
                context
                    .storage
                    .current_role_slot(context.txid)
                    .ok_or_else(|| {
                        sql_err!(
                            sqlstate::INSUFFICIENT_PRIVILEGE,
                            "current role is not present in the role catalog"
                        )
                    })?,
            ),
            ViewReader::WriteTarget => None,
        };
        let view_schema = view.schema_for(context.txid);
        let catalog_describes_other_temporary_view = view.persistence
            == crate::storage::RelationPersistence::Temporary
            && !context
                .storage
                .is_current_temporary_schema(view_schema.as_str());
        if let Some(requester) = requester
            && !catalog_describes_other_temporary_view
        {
            context.storage.require_schema_usage_as(
                view_schema.as_str(),
                requester,
                context.txid,
            )?;
            if !context.storage.has_object_privilege(
                view_object,
                requester,
                crate::storage::PrivilegeSet::SELECT,
                context.txid,
            ) && !context.storage.has_any_column_privilege(
                view_object,
                requester,
                crate::storage::PrivilegeSet::SELECT,
                context.txid,
            ) {
                return Err(sql_err!(
                    sqlstate::INSUFFICIENT_PRIVILEGE,
                    "permission denied for view {}",
                    view.name_for(context.txid).as_str()
                ));
            }
        }
        let authorization_role = match (context.authorization_role, view.security_for(context.txid))
        {
            (ViewReader::WriteTarget, _) => ViewReader::WriteTarget,
            (_, crate::storage::ViewSecurity::Definer) => {
                ViewReader::Role(context.storage.object_owner(view_object, context.txid) as u16)
            }
            (reader, crate::storage::ViewSecurity::Invoker) => reader,
        };
        let query = context
            .storage
            .snapshot_view_query(slot, context.txid, arena)?;
        let view_sql = arena
            .alloc_str(query.sql.as_str())
            .map_err(|_| arena_full())?;
        let user = crate::sql::eval::funcs::system::session_user_owned();
        let view_path =
            context
                .storage
                .compute_path(query.creation_path.as_str(), user.as_str(), context.txid);
        // The view body has its own scope: no outer CTEs, deeper view depth,
        // and the creator's path for its own references.
        let dependencies = query.dependencies;
        let vsel = super::parse_stored_query(
            view_sql,
            query.input_source.unwrap_or(view_sql),
            context.storage,
            context.txid,
            dependencies.view(),
            arena,
        )?;
        let expanded = if let Some(execution) = context.execution {
            with_exec_context(
                vsel.with,
                context.storage,
                context.txid,
                arena,
                execution,
                &[],
                &[],
                &[],
                &[],
                context.depth + 1,
                Some(view_path),
                Some(dependencies.view()),
                authorization_role,
                None,
                |name| body_references(vsel, name),
                |inner| subst_select_body(vsel, inner, arena),
            )?
        } else {
            expand_ctes_with_path(
                vsel,
                context.storage,
                context.txid,
                Some(view_path),
                Some(dependencies.view()),
                context.depth + 1,
                authorization_role,
                arena,
            )?
        };
        let view_columns = view.columns_for(context.txid);
        if !view_columns.has_aliases() {
            return Ok(TableRef {
                schema: None,
                table: "",
                alias: Some(t.alias.unwrap_or(t.table)),
                subquery: Some(expanded),
                func_args: None,
                func_argument_names: &[],
                func_variadic: false,
                func_source_position: crate::sql::ast::SourcePosition::UNKNOWN,
                rows_from: None,
                col_alias: None,
                inheritance: t.inheritance,
                sample: t.sample,
                cte: None,
                with_ordinality: false,
                lateral: false,
                authorization_role: None,
                bound_table: None,
                view_access: Some(slot as u16),
                join_members: None,
            });
        }
        let aliases = if view_columns.has_aliases() {
            let mut names = [""; crate::storage::MAX_COLUMNS];
            for (slot, name) in names.iter_mut().zip(view_columns.names()) {
                *slot = arena.alloc_str(name.as_str()).map_err(|_| arena_full())?;
            }
            Some(
                arena
                    .alloc_slice_copy(&names[..view_columns.len()])
                    .map_err(|_| arena_full())?,
            )
        } else {
            None
        };
        return Ok(TableRef {
            schema: None,
            table: "",
            alias: Some(t.alias.unwrap_or(t.table)),
            subquery: Some(expanded),
            func_args: None,
            func_argument_names: &[],
            func_variadic: false,
            func_source_position: crate::sql::ast::SourcePosition::UNKNOWN,
            rows_from: None,
            col_alias: aliases.map(|aliases| &*aliases),
            inheritance: t.inheritance,
            sample: t.sample,
            cte: None,
            with_ordinality: false,
            lateral: false,
            authorization_role: None,
            bound_table: None,
            view_access: Some(slot as u16),
            join_members: None,
        });
    }
    // Inside a view body, pin a table reference to the schema it resolved to
    // under the creator's path, so the reader's session path cannot re-bind
    // it.
    if let (Some(_), Some(crate::storage::ResolvedRelation::Table(slot))) = (context.path, resolved)
    {
        let def = context.storage.table_def(slot, context.txid);
        let schema = arena
            .alloc_str(def.schema.as_str())
            .map_err(|_| arena_full())?;
        let table = arena
            .alloc_str(def.name.as_str())
            .map_err(|_| arena_full())?;
        return Ok(TableRef {
            schema: Some(schema),
            table,
            alias: Some(t.alias.unwrap_or(t.table)),
            bound_table: captured.and_then(|relation| match relation {
                crate::storage::ResolvedRelation::Table(slot) => u16::try_from(slot).ok(),
                _ => None,
            }),
            authorization_role: match context.authorization_role {
                ViewReader::Role(role) => Some(role),
                ViewReader::Session | ViewReader::WriteTarget => None,
            },
            ..*t
        });
    }
    Ok(*t)
}

fn opt_subst<'a>(
    e: Option<&'a Expr<'a>>,
    context: Subst<'_, 'a, '_, '_>,
    arena: &'a Arena,
) -> Result<Option<&'a Expr<'a>>, SqlError> {
    match e {
        Some(x) => Ok(Some(subst_expr(x, context, arena)?)),
        None => Ok(None),
    }
}

fn subst_expr_slice<'a>(
    xs: &'a [&'a Expr<'a>],
    context: Subst<'_, 'a, '_, '_>,
    arena: &'a Arena,
) -> Result<&'a [&'a Expr<'a>], SqlError> {
    if context.qualifier.is_none()
        && context.rule_transition.is_none()
        && context.recursive_state.is_none()
        && context.dependencies.is_none()
        && !xs.iter().any(|x| x.contains_subquery())
    {
        return Ok(xs);
    }
    let mut tmp: ArenaList<'a, &'a Expr<'a>> = ArenaList::new(arena);
    for x in xs {
        tmp.push(subst_expr(x, context, arena)?)
            .map_err(|_| arena_full())?;
    }
    Ok(tmp.as_slice())
}

fn subst_frame_bound<'a>(
    bound: crate::sql::ast::FrameBound<'a>,
    context: Subst<'_, 'a, '_, '_>,
    arena: &'a Arena,
) -> Result<crate::sql::ast::FrameBound<'a>, SqlError> {
    Ok(match bound {
        crate::sql::ast::FrameBound::Preceding(expression) => {
            crate::sql::ast::FrameBound::Preceding(subst_expr(expression, context, arena)?)
        }
        crate::sql::ast::FrameBound::Following(expression) => {
            crate::sql::ast::FrameBound::Following(subst_expr(expression, context, arena)?)
        }
        other => other,
    })
}

fn stored_type_reference(
    type_name: &str,
    dependencies: crate::storage::StoredQueryDependencyView<'_>,
) -> Option<(
    crate::storage::DependencyClass,
    crate::storage::TypeReference,
    bool,
)> {
    let (bare, array) = type_name
        .strip_suffix("[]")
        .map_or((type_name, false), |bare| (bare, true));
    let (referenced_schema, referenced_name) = bare.split_once('.').unwrap_or(("", bare));
    dependencies
        .entries()
        .iter()
        .find(|dependency| {
            matches!(
                dependency.class,
                crate::storage::DependencyClass::Domain
                    | crate::storage::DependencyClass::Enum
                    | crate::storage::DependencyClass::Composite
            ) && dependency.referenced_schema.as_str() == referenced_schema
                && dependency.referenced_name.as_str() == referenced_name
        })
        .and_then(|dependency| match dependency.identity {
            crate::storage::StoredDependencyIdentity::Type(reference) => {
                Some((dependency.class, reference, array))
            }
            _ => None,
        })
}

/// Resolved dependency slots retain the identity of the original SQL spelling,
/// even when its old name has since been reused by another type.
pub(crate) fn captured_type_oid(
    type_name: &str,
    dependencies: crate::storage::StoredQueryDependencyView<'_>,
) -> Option<i32> {
    use crate::sql::types::oid;
    let (class, reference, array) = stored_type_reference(type_name, dependencies)?;
    let slot = reference.creation();
    Some(match (class, array) {
        (crate::storage::DependencyClass::Domain, false) => oid::domain_oid(slot),
        (crate::storage::DependencyClass::Domain, true) => oid::domain_array_oid(slot),
        (crate::storage::DependencyClass::Enum, false) => oid::enum_oid(slot),
        (crate::storage::DependencyClass::Enum, true) => oid::enum_array_oid(slot),
        (crate::storage::DependencyClass::Composite, false) => oid::composite_oid(slot),
        (crate::storage::DependencyClass::Composite, true) => oid::composite_array_oid(slot),
        _ => unreachable!("stored type reference selects only named types"),
    })
}

fn rewrite_stored_type_name<'a>(
    type_name: &'a str,
    context: Subst<'_, 'a, '_, '_>,
    arena: &'a Arena,
) -> Result<&'a str, SqlError> {
    let Some(dependencies) = context.dependencies else {
        return Ok(type_name);
    };
    let Some((class, reference, array)) = stored_type_reference(type_name, dependencies) else {
        return Ok(type_name);
    };
    let (schema, name) = match class {
        crate::storage::DependencyClass::Domain => {
            let definition = context
                .storage
                .read_domain_definition(
                    crate::storage::TypeDefinitionTarget::Bound(reference),
                    context.txid,
                )?
                .ok_or_else(|| {
                    sql_err!(
                        sqlstate::UNDEFINED_OBJECT,
                        "captured type source no longer exists"
                    )
                })?;
            (definition.schema, definition.name)
        }
        crate::storage::DependencyClass::Enum => {
            let definition = context
                .storage
                .read_enum_definition(
                    crate::storage::TypeDefinitionTarget::Bound(reference),
                    context.txid,
                )?
                .ok_or_else(|| {
                    sql_err!(
                        sqlstate::UNDEFINED_OBJECT,
                        "captured type source no longer exists"
                    )
                })?;
            (definition.schema, definition.name)
        }
        crate::storage::DependencyClass::Composite => {
            let definition = context
                .storage
                .read_composite_definition(
                    crate::storage::TypeDefinitionTarget::Bound(reference),
                    context.txid,
                )?
                .ok_or_else(|| {
                    sql_err!(
                        sqlstate::UNDEFINED_OBJECT,
                        "captured type source no longer exists"
                    )
                })?;
            (definition.schema, definition.name)
        }
        _ => unreachable!("stored type-name rewriting records only type dependencies"),
    };
    let rendered = if array {
        crate::stack_format!(192, "{}.{}[]", schema.as_str(), name.as_str())
    } else {
        crate::stack_format!(192, "{}.{}", schema.as_str(), name.as_str())
    };
    arena.alloc_str(rendered.as_str()).map_err(|_| arena_full())
}

fn rewrite_stored_collation<'a>(
    collation: crate::sql::ast::ParsedCollation<'a>,
    context: Subst<'_, 'a, '_, '_>,
) -> Result<crate::sql::ast::ParsedCollation<'a>, SqlError> {
    let crate::sql::ast::ParsedCollation::Named(name) = collation else {
        return Ok(collation);
    };
    let Some(dependencies) = context.dependencies else {
        return Ok(collation);
    };
    let dependency = dependencies
        .entries()
        .iter()
        .find(|dependency| {
            dependency.class == crate::storage::DependencyClass::Collation
                && dependency.referenced_schema.as_str() == name.schema.unwrap_or("")
                && dependency.referenced_name.as_str() == name.name
        })
        .ok_or_else(|| {
            sql_err!(
                sqlstate::UNDEFINED_OBJECT,
                "stored collation dependency \"{}\" does not exist",
                name.name
            )
        })?;
    match dependency.identity {
        crate::storage::StoredDependencyIdentity::Collation(selected) => {
            Ok(crate::sql::ast::ParsedCollation::Builtin(
                crate::sql::ast::Collation::Catalog(selected),
            ))
        }
        _ => Err(sql_err!(
            sqlstate::INTERNAL_ERROR,
            "stored collation identity has the wrong class"
        )),
    }
}

fn rewrite_stored_regcollation_operand<'a>(
    operand: &'a Expr<'a>,
    type_name: &str,
    context: Subst<'_, 'a, '_, '_>,
    arena: &'a Arena,
) -> Result<&'a Expr<'a>, SqlError> {
    if type_name != "regcollation" {
        return Ok(operand);
    }
    let Expr::Str(written) = operand else {
        return Ok(operand);
    };
    let Some(dependencies) = context.dependencies else {
        return Ok(operand);
    };
    let Some((referenced_schema, referenced_name)) =
        crate::sql::catalog::split_qualified_name(written)
    else {
        return Ok(operand);
    };
    let referenced_schema = referenced_schema.unwrap_or("");
    let Some(dependency) = dependencies.entries().iter().find(|dependency| {
        dependency.class == crate::storage::DependencyClass::Collation
            && dependency.referenced_schema.as_str() == referenced_schema
            && dependency.referenced_name.as_str() == referenced_name
    }) else {
        return Ok(operand);
    };
    let oid = match dependency.identity {
        crate::storage::StoredDependencyIdentity::Collation(selected) => selected.oid(),
        _ => {
            return Err(sql_err!(
                sqlstate::INTERNAL_ERROR,
                "stored collation identity has the wrong class"
            ));
        }
    };
    let rendered = arena.alloc_str_display(oid).map_err(|_| arena_full())?;
    arena
        .alloc(Expr::Str(rendered))
        .map(|expression| &*expression)
        .map_err(|_| arena_full())
}

fn text_search_configuration_argument<'a>(
    function: &str,
    args: &'a [&'a Expr<'a>],
) -> Option<&'a str> {
    let explicit = match function {
        "to_tsvector"
        | "to_tsquery"
        | "plainto_tsquery"
        | "phraseto_tsquery"
        | "websearch_to_tsquery" => args.len() == 2,
        "json_to_tsvector" | "jsonb_to_tsvector" => args.len() == 3,
        "ts_headline" => matches!(args.len(), 3 | 4),
        _ => false,
    };
    fn literal<'a>(expression: &'a Expr<'a>) -> Option<&'a str> {
        match expression {
            Expr::Str(value) => Some(value),
            Expr::Cast { operand, .. }
            | Expr::BoundDefault {
                expression: operand,
                ..
            }
            | Expr::Collate { operand, .. } => literal(operand),
            _ => None,
        }
    }
    explicit
        .then(|| args.first().copied())
        .flatten()
        .and_then(literal)
}

fn rewrite_stored_text_search_arguments<'a>(
    function: &str,
    args: &'a [&'a Expr<'a>],
    context: Subst<'_, 'a, '_, '_>,
    arena: &'a Arena,
) -> Result<&'a [&'a Expr<'a>], SqlError> {
    let Some(written) = text_search_configuration_argument(function, args) else {
        return Ok(args);
    };
    let Some(dependencies) = context.dependencies else {
        return Ok(args);
    };
    let (referenced_schema, referenced_name) = written.split_once('.').unwrap_or(("", written));
    let Some(dependency) = dependencies.entries().iter().find(|dependency| {
        dependency.class == crate::storage::DependencyClass::TextSearchConfiguration
            && dependency.referenced_schema.as_str() == referenced_schema
            && dependency.referenced_name.as_str() == referenced_name
    }) else {
        return Ok(args);
    };
    let oid = match dependency.identity {
        crate::storage::StoredDependencyIdentity::TextSearchConfigurationOid(oid) => oid,
        _ => {
            return Err(sql_err!(
                sqlstate::INTERNAL_ERROR,
                "stored text search identity has the wrong class"
            ));
        }
    };
    if !context
        .storage
        .text_search_objects_visible_to(context.txid)
        .any(|(_, definition)| definition.oid() == oid)
    {
        return Err(sql_err!(
            sqlstate::UNDEFINED_OBJECT,
            "stored text search configuration dependency does not exist"
        ));
    }
    // Keep the selected identifier through later execution; a new name lookup
    // after a wait could select an object which reused the old spelling.
    let operand = arena
        .alloc(Expr::Int(i64::from(oid)))
        .map_err(|_| arena_full())?;
    let first = arena
        .alloc(Expr::Cast {
            source_position: crate::sql::ast::SourcePosition::UNKNOWN,
            operand,
            type_name: "regconfig",
            type_mod: -1,
            binding: crate::sql::ast::CastBinding::Resolve,
        })
        .map_err(|_| arena_full())?;
    let rewritten = arena.alloc_slice_copy(args).map_err(|_| arena_full())?;
    rewritten[0] = first;
    Ok(rewritten)
}

fn rewrite_stored_sequence_arguments<'a>(
    function: &str,
    args: &'a [&'a Expr<'a>],
    context: Subst<'_, 'a, '_, '_>,
    arena: &'a Arena,
) -> Result<&'a [&'a Expr<'a>], SqlError> {
    let Some(written) = super::dependencies::bound_sequence_literal(function, args) else {
        return Ok(args);
    };
    let Some(dependencies) = context.dependencies else {
        return Ok(args);
    };
    let (referenced_schema, referenced_name) =
        match crate::sql::identifiers::ObjectReference::parse(written)? {
            crate::sql::identifiers::ObjectReference::Oid(oid) => (
                crate::storage::SqlName::EMPTY,
                crate::storage::SqlName::parse(crate::stack_format!(16, "{}", oid).as_str())?,
            ),
            crate::sql::identifiers::ObjectReference::Name(parsed) => (
                parsed.schema.unwrap_or(crate::storage::SqlName::EMPTY),
                parsed.name,
            ),
        };
    let Some(dependency) = dependencies.entries().iter().find(|dependency| {
        dependency.class == crate::storage::DependencyClass::Sequence
            && dependency.referenced_schema == referenced_schema
            && dependency.referenced_name == referenced_name
    }) else {
        return Ok(args);
    };
    let oid = crate::sql::catalog::sequence_oid(context.storage, dependency.slot as usize);
    let rendered = arena.alloc_str_display(oid).map_err(|_| arena_full())?;
    let operand = arena.alloc(Expr::Str(rendered)).map_err(|_| arena_full())?;
    let first = arena
        .alloc(Expr::Cast {
            source_position: crate::sql::ast::SourcePosition::UNKNOWN,
            operand,
            type_name: "regclass",
            type_mod: -1,
            binding: crate::sql::ast::CastBinding::Resolve,
        })
        .map_err(|_| arena_full())?;
    let rewritten = arena.alloc_slice_copy(args).map_err(|_| arena_full())?;
    rewritten[0] = first;
    Ok(rewritten)
}

fn rewrite_stored_routine_name<'a>(
    call: &'a Expr<'a>,
    name: &'a str,
    context: Subst<'_, 'a, '_, '_>,
    arena: &'a Arena,
) -> Result<(&'a str, Option<i32>), SqlError> {
    let Expr::Call {
        bound_oid,
        args,
        argument_names,
        variadic,
        ..
    } = call
    else {
        return Err(sql_err!(
            sqlstate::INTERNAL_ERROR,
            "stored routine binding requires a call"
        ));
    };
    let bound_oid = *bound_oid;
    if crate::sql::eval::is_sql_syntax_call(call) {
        return Ok((name, bound_oid));
    }
    let Some(dependencies) = context.dependencies else {
        return Ok((name, bound_oid));
    };
    let Some(dependency) = super::dependencies::stored_routine_dependency_for_call(
        name,
        args,
        argument_names,
        *variadic,
        context.storage,
        context.txid,
        dependencies,
        arena,
    )?
    else {
        return Ok((name, bound_oid));
    };
    let routine = context
        .storage
        .routine_for(dependency.slot as usize, context.txid);
    let qualified = crate::stack_format!(
        192,
        "{}.{}",
        routine.schema_for(context.txid).as_str(),
        routine.name_for(context.txid).as_str()
    );
    if qualified.is_truncated() {
        return Err(sql_err!(
            sqlstate::PROGRAM_LIMIT_EXCEEDED,
            "routine identity is too long"
        ));
    }
    let name = arena
        .alloc_str(qualified.as_str())
        .map_err(|_| arena_full())?;
    Ok((name, Some(crate::storage::routine_oid(&routine))))
}

fn rewrite_stored_operator_name<'a>(
    name: &'a str,
    args: &[&Expr<'a>],
    context: Subst<'_, 'a, '_, '_>,
    arena: &'a Arena,
) -> Result<&'a str, SqlError> {
    let Some(dependencies) = context.dependencies else {
        return Ok(name);
    };
    let Some(dependency) = super::dependencies::stored_operator_dependency_for_call(
        name,
        args,
        context.storage,
        context.txid,
        dependencies,
        arena,
    )?
    else {
        return Ok(name);
    };
    let operator = context
        .storage
        .operator_for(dependency.slot as usize, context.txid);
    let encoded = crate::stack_format!(
        260,
        "{}{}\u{1f}{}",
        crate::sql::ast::CATALOG_OPERATOR_CALL_PREFIX,
        operator.schema.as_str(),
        operator.name.as_str()
    );
    if encoded.is_truncated() {
        return Err(sql_err!(
            sqlstate::PROGRAM_LIMIT_EXCEEDED,
            "operator identity is too long"
        ));
    }
    arena.alloc_str(encoded.as_str()).map_err(|_| arena_full())
}

fn subst_expr<'a>(
    e: &'a Expr<'a>,
    context: Subst<'_, 'a, '_, '_>,
    arena: &'a Arena,
) -> Result<&'a Expr<'a>, SqlError> {
    if let Some(rewrite) = context.qualifier
        && let Some(projected) = rewrite.projected_columns
    {
        let name = match e {
            Expr::Column { qualifier, name }
                if qualifier.is_none_or(|written| {
                    written == rewrite.from || rewrite.projected_aliases.contains(&written)
                }) =>
            {
                Some(*name)
            }
            Expr::SchemaColumn {
                schema,
                table,
                name,
            } if *table == rewrite.from && Some(*schema) == rewrite.from_schema => Some(*name),
            _ => None,
        };
        if let Some(name) = name
            && let Some(index) = rewrite
                .exposed_columns
                .iter()
                .position(|known| *known == name)
        {
            return Ok(projected[index]);
        }
        // A whole-row reference reads every projected column.
        let whole_row = match e {
            Expr::WholeRow(qualifier) => *qualifier == rewrite.from,
            Expr::Column {
                qualifier: None,
                name,
            } => *name == rewrite.from,
            _ => false,
        };
        if whole_row && projected.len() == rewrite.exposed_columns.len() {
            return Ok(&*arena
                .alloc(Expr::Call {
                    kind: crate::sql::ast::CallKind::SqlSyntax,
                    name: "row",
                    bound_oid: None,
                    bound_strict: None,
                    source_position: crate::sql::ast::SourcePosition::UNKNOWN,
                    args: projected,
                    argument_names: &[],
                    variadic: false,
                    star: false,
                    distinct: false,
                    order_by: &[],
                    over: None,
                    filter: None,
                })
                .map_err(|_| arena_full())?);
        }
    }
    if context.qualifier.is_none()
        && context.rule_transition.is_none()
        && context.recursive_state.is_none()
        && context.dependencies.is_none()
        && !e.contains_subquery()
    {
        return Ok(e);
    }
    let rebuilt = match e {
        Expr::Subquery(s) => {
            let rebuilt = subst_select(s, context, arena)?;
            // An unchanged subquery keeps its node, which locates its position.
            if core::ptr::eq(rebuilt, *s) || rebuilt == *s {
                return Ok(e);
            }
            Expr::Subquery(rebuilt)
        }
        Expr::RowSubquery { select, arity } => Expr::RowSubquery {
            select: subst_select(select, context, arena)?,
            arity: *arity,
        },
        Expr::RecordFieldIndex { base, index } => Expr::RecordFieldIndex {
            base: subst_expr(base, context, arena)?,
            index: *index,
        },
        Expr::ArraySubquery(s) => Expr::ArraySubquery(subst_select(s, context, arena)?),
        Expr::Exists(s) => Expr::Exists(subst_select(s, context, arena)?),
        Expr::InSubquery {
            operand,
            select,
            negated,
        } => Expr::InSubquery {
            operand: subst_expr(operand, context, arena)?,
            select: subst_select(select, context, arena)?,
            negated: *negated,
        },
        Expr::QuantifiedSubquery {
            operand,
            operator,
            select,
            all,
        } => Expr::QuantifiedSubquery {
            operand: subst_expr(operand, context, arena)?,
            operator: *operator,
            select: subst_select(select, context, arena)?,
            all: *all,
        },
        Expr::Unary {
            operator, operand, ..
        } => Expr::Unary {
            source_position: e.source_position(),
            operator: *operator,
            operand: subst_expr(operand, context, arena)?,
        },
        Expr::Binary {
            operator,
            left,
            right,
            ..
        } => Expr::Binary {
            source_position: e.source_position(),
            operator: *operator,
            left: subst_expr(left, context, arena)?,
            right: subst_expr(right, context, arena)?,
        },
        Expr::Cast {
            operand,
            type_name,
            type_mod,
            binding,
            ..
        } => {
            let operand = subst_expr(operand, context, arena)?;
            Expr::Cast {
                source_position: e.source_position(),
                operand: rewrite_stored_regcollation_operand(operand, type_name, context, arena)?,
                type_name: rewrite_stored_type_name(type_name, context, arena)?,
                type_mod: *type_mod,
                binding: *binding,
            }
        }
        Expr::BoundDefault {
            expression,
            type_oid,
        } => Expr::BoundDefault {
            expression: subst_expr(expression, context, arena)?,
            type_oid: *type_oid,
        },
        Expr::Collate { operand, collation } => Expr::Collate {
            operand: subst_expr(operand, context, arena)?,
            collation: rewrite_stored_collation(*collation, context)?,
        },
        Expr::IsNull { operand, negated } => Expr::IsNull {
            operand: subst_expr(operand, context, arena)?,
            negated: *negated,
        },
        Expr::Call {
            name,
            args,
            argument_names,
            variadic,
            star,
            distinct,
            order_by,
            over,
            filter,
            bound_strict,
            kind,
            source_position,
            ..
        } => {
            let name = if *kind == crate::sql::ast::CallKind::SqlSyntax {
                rewrite_stored_operator_name(name, args, context, arena)?
            } else {
                *name
            };
            let (name, bound_oid) = rewrite_stored_routine_name(e, name, context, arena)?;
            let mut ob: ArenaList<'a, OrderBy<'a>> = ArenaList::new(arena);
            for o in order_by.iter() {
                ob.push(OrderBy {
                    expression: subst_expr(o.expression, context, arena)?,
                    ..*o
                })
                .map_err(|_| arena_full())?;
            }
            let order_by = ob.as_slice();
            let over = match over {
                None => None,
                Some(w) => {
                    let mut ob2: ArenaList<'a, OrderBy<'a>> = ArenaList::new(arena);
                    for o in w.order_by {
                        ob2.push(OrderBy {
                            expression: subst_expr(o.expression, context, arena)?,
                            ..*o
                        })
                        .map_err(|_| arena_full())?;
                    }
                    let frame = match w.frame {
                        Some(frame) => Some(crate::sql::ast::WindowFrame {
                            start: subst_frame_bound(frame.start, context, arena)?,
                            end: subst_frame_bound(frame.end, context, arena)?,
                            ..frame
                        }),
                        None => None,
                    };
                    let spec = crate::sql::ast::WindowSpec {
                        partition_by: subst_expr_slice(w.partition_by, context, arena)?,
                        order_by: ob2.as_slice(),
                        frame,
                    };
                    Some(&*arena.alloc(spec).map_err(|_| arena_full())?)
                }
            };
            let filter = match filter {
                None => None,
                Some(f) => Some(subst_expr(f, context, arena)?),
            };
            let args = subst_expr_slice(args, context, arena)?;
            let args = rewrite_stored_text_search_arguments(name, args, context, arena)?;
            let args = rewrite_stored_sequence_arguments(name, args, context, arena)?;
            Expr::Call {
                name,
                kind: *kind,
                bound_oid,
                bound_strict: *bound_strict,
                source_position: *source_position,
                args,
                argument_names,
                variadic: *variadic,
                star: *star,
                distinct: *distinct,
                order_by,
                over,
                filter,
            }
        }
        Expr::InList {
            operand,
            list,
            negated,
        } => Expr::InList {
            operand: subst_expr(operand, context, arena)?,
            list: subst_expr_slice(list, context, arena)?,
            negated: *negated,
        },
        Expr::Like {
            operand,
            pattern,
            negated,
            case_insensitive,
            escape,
        } => Expr::Like {
            operand: subst_expr(operand, context, arena)?,
            pattern: subst_expr(pattern, context, arena)?,
            negated: *negated,
            case_insensitive: *case_insensitive,
            escape: opt_subst(*escape, context, arena)?,
        },
        Expr::Match {
            operand,
            pattern,
            negated,
            case_insensitive,
        } => Expr::Match {
            operand: subst_expr(operand, context, arena)?,
            pattern: subst_expr(pattern, context, arena)?,
            negated: *negated,
            case_insensitive: *case_insensitive,
        },
        Expr::Case {
            operand,
            whens,
            otherwise,
            origin,
        } => {
            let operand = opt_subst(*operand, context, arena)?;
            let mut ws: ArenaList<'a, (&'a Expr<'a>, &'a Expr<'a>)> = ArenaList::new(arena);
            for (c, r) in whens.iter().copied() {
                ws.push((
                    subst_expr(c, context, arena)?,
                    subst_expr(r, context, arena)?,
                ))
                .map_err(|_| arena_full())?;
            }
            let whens = ws.as_slice();
            Expr::Case {
                operand,
                whens,
                otherwise: opt_subst(*otherwise, context, arena)?,
                origin: *origin,
            }
        }
        Expr::Array(items) => Expr::Array(subst_expr_slice(items, context, arena)?),
        Expr::Subscript { base, index } => Expr::Subscript {
            base: subst_expr(base, context, arena)?,
            index: subst_expr(index, context, arena)?,
        },
        Expr::Slice { base, lower, upper } => Expr::Slice {
            base: subst_expr(base, context, arena)?,
            lower: opt_subst(*lower, context, arena)?,
            upper: opt_subst(*upper, context, arena)?,
        },
        Expr::Field { base, field } => Expr::Field {
            base: subst_expr(base, context, arena)?,
            field,
        },
        Expr::AnyAll {
            operand,
            operator,
            array,
            all,
        } => Expr::AnyAll {
            operand: subst_expr(operand, context, arena)?,
            operator: *operator,
            array: subst_expr(array, context, arena)?,
            all: *all,
        },
        Expr::Column { qualifier, name } => {
            if let Some(transition) = context.rule_transition
                && let Some(written) = qualifier
                && (written.eq_ignore_ascii_case("old") || written.eq_ignore_ascii_case("new"))
            {
                let old = written.eq_ignore_ascii_case("old");
                if (old && !transition.old_available) || (!old && !transition.new_available) {
                    return Err(sql_err!(
                        sqlstate::INVALID_OBJECT_DEFINITION,
                        "ON {} rule cannot use {}",
                        if old { "INSERT" } else { "DELETE" },
                        if old { "OLD" } else { "NEW" }
                    ));
                }
                let index = transition
                    .names
                    .iter()
                    .position(|known| known == name)
                    .ok_or_else(|| {
                        sql_err!(
                            sqlstate::UNDEFINED_COLUMN,
                            "column \"{}\" does not exist",
                            name
                        )
                    })?;
                return Ok(if old {
                    transition.old[index]
                } else {
                    transition.new[index]
                });
            }
            if let Some(state) = context.recursive_state
                && qualifier.is_none_or(|written| written == state.qualifier)
                && let Some(index) = state.names.iter().position(|candidate| candidate == name)
            {
                Expr::RecursiveState {
                    qualifier: state.qualifier,
                    index: index as u8,
                    ctype: state.types[index],
                }
            } else {
                Expr::Column {
                    qualifier: match (*qualifier, context.qualifier) {
                        (Some(written), Some(rewrite)) if written == rewrite.from => {
                            Some(rewrite.to)
                        }
                        _ => *qualifier,
                    },
                    name: match context.qualifier {
                        Some(rewrite)
                            if qualifier.is_none_or(|written| written == rewrite.from) =>
                        {
                            view_base_column(rewrite.exposed_columns, rewrite.base_columns, name)
                        }
                        _ => name,
                    },
                }
            }
        }
        Expr::WholeRow(qualifier) => Expr::WholeRow(match context.qualifier {
            Some(rewrite) if *qualifier == rewrite.from => rewrite.to,
            _ => qualifier,
        }),
        Expr::SchemaColumn {
            schema,
            table,
            name,
        } => match context.qualifier {
            Some(rewrite) if *table == rewrite.from => Expr::SchemaColumn {
                schema: rewrite.to_schema,
                table: rewrite.to,
                name: view_base_column(rewrite.exposed_columns, rewrite.base_columns, name),
            },
            _ => Expr::SchemaColumn {
                schema,
                table,
                name,
            },
        },
        // Subquery-free leaves keep their values through substitution.
        other => *other,
    };
    Ok(&*arena.alloc(rebuilt).map_err(|_| arena_full())?)
}
