//! `WITH` expansion and the AST substitution it rests on.
//!
//! An ordinary CTE is either inlined or materialized under its typed PostgreSQL
//! evaluation contract; a recursive one is materialized by fixpoint iteration.
//! Views expand the same way, from their stored text. Both rest on one thing: a
//! substituting walk that rebuilds a statement's expressions and FROM items in
//! the arena with each reference replaced, which is the rest of this module.

use crate::mem::arena::Arena;
use crate::sql::ast::{
    Collation, Cte, CteCycleMark, CteMaterialization, CteSearchOrder, Delete, Expr, FromClause,
    Insert, Join, JoinKind, MaterializedCte, Merge, MergeAction, MergeWhen, OnConflict,
    OnConflictTarget, OrderBy, Select, SelectItem, SetOp, SetQuery, SetTree, Stmt, TableRef,
    Update,
};
use crate::sql::eval::{SequenceAccess, SqlError, sqlstate};
use crate::sql::exec::MAX_PROJ;
use crate::sql::types::{ArrElem, ColDesc, ColType, Datum, RecordField};
use crate::sql_err;
use crate::storage::Storage;

use super::setops::{describe_set_body, external_set_body_into, materialize_set_body};
use super::{MAX_JOIN_TABLES, arena_full, check_timeout};

/// Expands a statement's `WITH` list (and any view reference) for the
/// describe path, which needs the shape but not the rows.
pub fn expand_ctes<'a>(
    sel: &'a Select<'a>,
    storage: &Storage,
    txid: u32,
    arena: &'a Arena,
) -> Result<&'a Select<'a>, SqlError> {
    expand_ctes_with_path(sel, storage, txid, None, None, 0, None, arena)
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
    expand_ctes_with_path(sel, storage, txid, Some(path), None, 0, None, arena)
}

pub(crate) fn expand_stored_query<'a>(
    select: &'a Select<'a>,
    storage: &Storage,
    txid: u32,
    path: crate::storage::PathContext,
    dependencies: &crate::storage::StoredQueryDependencies,
    arena: &'a Arena,
) -> Result<&'a Select<'a>, SqlError> {
    expand_ctes_with_path(
        select,
        storage,
        txid,
        Some(path),
        Some(dependencies),
        0,
        None,
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
    storage: &'a Storage,
    txid: u32,
    path: crate::storage::PathContext,
    dependencies: &'a crate::storage::StoredQueryDependencies,
    arena: &'a Arena,
    params: &[Datum<'a>],
    sequences: Option<&dyn SequenceAccess>,
) -> Result<&'a Select<'a>, SqlError> {
    with_exec_context(
        select.with,
        select,
        storage,
        txid,
        arena,
        params,
        &[],
        &[],
        &[],
        &[],
        sequences,
        0,
        Some(path),
        Some(dependencies),
        None,
        |name| select_references(select, name),
        |context| subst_select_body(select, context, arena),
    )
}

pub(crate) fn expand_stored_expression<'a>(
    expression: &'a Expr<'a>,
    storage: &Storage,
    txid: u32,
    dependencies: &crate::storage::StoredQueryDependencies,
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
            path: Some(*storage.path()),
            dependencies: Some(dependencies),
            authorization_role: None,
            qualifier: None,
            recursive_state: None,
            execution: None,
        },
        arena,
    )
}

#[allow(clippy::too_many_arguments)]
fn expand_ctes_with_path<'a>(
    sel: &'a Select<'a>,
    storage: &Storage,
    txid: u32,
    path: Option<crate::storage::PathContext>,
    dependencies: Option<&crate::storage::StoredQueryDependencies>,
    depth: u32,
    authorization_role: Option<u16>,
    arena: &'a Arena,
) -> Result<&'a Select<'a>, SqlError> {
    // Fast path: nothing to rewrite (no CTEs anywhere and no views defined).
    if sel.with.is_empty() && dependencies.is_none() && !storage.has_any_view() {
        return Ok(sel);
    }
    if sel.with.len() > crate::sql::parser::MAX_CTES {
        return Err(sql_err!(
            sqlstate::TOO_MANY_ARGUMENTS,
            "too many WITH entries"
        ));
    }
    // Resolve CTEs left-to-right so a CTE can reference earlier ones.
    let mut resolved: [(&'a str, &'a Select<'a>, &'a [&'a str]); crate::sql::parser::MAX_CTES] =
        [("", sel, &[]); crate::sql::parser::MAX_CTES];
    let mut materialized = [("", &EMPTY_CTE); crate::sql::parser::MAX_CTES];
    let mut resolved_count = 0;
    let mut materialized_count = 0;
    for (index, cte) in sel.with.iter().enumerate() {
        if sel.with[..index].iter().any(|prior| prior.name == cte.name) {
            return Err(sql_err!(
                sqlstate::DUPLICATE_ALIAS,
                "WITH query name \"{}\" specified more than once",
                cte.name
            ));
        }
        let context = Subst {
            ctes: &resolved[..resolved_count],
            materialized: &materialized[..materialized_count],
            storage,
            txid,
            depth,
            path,
            dependencies,
            authorization_role,
            qualifier: None,
            recursive_state: None,
            execution: None,
        };
        let self_references = select_references(cte.query, cte.name);
        if (cte.search.is_some() || cte.cycle.is_some()) && (!cte.recursive || self_references == 0)
        {
            return Err(sql_err!(
                sqlstate::SYNTAX_ERROR,
                "WITH query \"{}\" is not recursive",
                cte.name
            ));
        }
        if cte.recursive && self_references > 0 {
            materialized[materialized_count] = (
                cte.name,
                describe_recursive_materialized(cte, context, storage, txid, arena)?,
            );
            materialized_count += 1;
        } else {
            let query = subst_select(cte.query, context, arena)?;
            resolved[resolved_count] = (cte.name, query, cte.columns);
            resolved_count += 1;
        }
    }
    // Substitute the body against all CTEs (the WITH list is dropped by
    // subst_select, which never copies it) and expand any view references.
    let context = Subst {
        ctes: &resolved[..resolved_count],
        materialized: &materialized[..materialized_count],
        storage,
        txid,
        depth,
        path,
        dependencies,
        authorization_role,
        qualifier: None,
        recursive_state: None,
        execution: None,
    };
    subst_select_body(sel, context, arena)
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
    if sel.with.is_empty() && !storage.has_any_view() {
        return Ok(sel);
    }
    with_exec_context(
        sel.with,
        sel,
        storage,
        txid,
        arena,
        params,
        dml_mats,
        &[],
        &[],
        &[],
        sequences,
        0,
        None,
        None,
        None,
        |name| select_references(sel, name),
        |context| subst_select_body(sel, context, arena),
    )
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
    let context = Subst {
        ctes: &[],
        materialized: relations,
        storage,
        txid,
        depth: 0,
        path: None,
        dependencies: None,
        authorization_role: None,
        qualifier: None,
        recursive_state: None,
        execution: None,
    };
    subst_select(select, context, arena)
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
    let placeholder = match statement {
        Stmt::Insert(insert) => insert.select.unwrap_or(&EMPTY_SELECT),
        _ => &EMPTY_SELECT,
    };
    with_exec_context(
        with,
        placeholder,
        storage,
        txid,
        arena,
        params,
        dml_mats,
        relations,
        &[],
        &[],
        sequences,
        0,
        None,
        None,
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

/// Rebinds a data-modifying statement from an auto-updatable view to its base
/// relation without losing qualified target references or the view's exposed
/// RETURNING shape.
#[allow(clippy::too_many_arguments)]
pub fn rewrite_view_dml<'a>(
    statement: &'a Stmt<'a>,
    view_name: &'a str,
    base_name: &'a str,
    base_schema: &'a str,
    exposed_columns: &'a [&'a str],
    storage: &Storage,
    txid: u32,
    arena: &'a Arena,
) -> Result<&'a Stmt<'a>, SqlError> {
    let context = Subst {
        ctes: &[],
        materialized: &[],
        storage,
        txid,
        depth: 0,
        path: None,
        dependencies: None,
        authorization_role: None,
        qualifier: Some(ViewQualifier {
            from: view_name,
            to: base_name,
            to_schema: base_schema,
        }),
        recursive_state: None,
        execution: None,
    };
    let expanded_returning =
        |items| expand_view_returning(items, view_name, exposed_columns, arena);
    let rewritten = match statement {
        Stmt::Insert(insert) => {
            for column in insert.columns {
                require_view_column(view_name, exposed_columns, column)?;
            }
            let adjusted = Insert {
                returning: expanded_returning(insert.returning)?,
                ..*insert
            };
            let adjusted = arena.alloc(adjusted).map_err(|_| arena_full())?;
            Stmt::Insert(subst_insert(&*adjusted, context, arena)?)
        }
        Stmt::Update(update) => {
            for (column, _) in update.assignments {
                require_view_column(view_name, exposed_columns, column)?;
            }
            let adjusted = Update {
                returning: expanded_returning(update.returning)?,
                ..*update
            };
            let adjusted = arena.alloc(adjusted).map_err(|_| arena_full())?;
            Stmt::Update(subst_update(&*adjusted, context, arena)?)
        }
        Stmt::Delete(delete) => {
            let adjusted = Delete {
                returning: expanded_returning(delete.returning)?,
                ..*delete
            };
            let adjusted = arena.alloc(adjusted).map_err(|_| arena_full())?;
            Stmt::Delete(subst_delete(&*adjusted, context, arena)?)
        }
        _ => {
            return Err(sql_err!(
                sqlstate::INTERNAL_ERROR,
                "view rewrite expected INSERT, UPDATE, or DELETE"
            ));
        }
    };
    Ok(&*arena.alloc(rewritten).map_err(|_| arena_full())?)
}

fn require_view_column(
    view_name: &str,
    exposed_columns: &[&str],
    column: &str,
) -> Result<(), SqlError> {
    if exposed_columns.contains(&column) {
        return Ok(());
    }
    Err(sql_err!(
        sqlstate::UNDEFINED_COLUMN,
        "column \"{}\" of relation \"{}\" does not exist",
        column,
        view_name
    ))
}

fn expand_view_returning<'a>(
    items: &'a [SelectItem<'a>],
    view_name: &'a str,
    exposed_columns: &'a [&'a str],
    arena: &'a Arena,
) -> Result<&'a [SelectItem<'a>], SqlError> {
    let extra = items.iter().fold(0usize, |count, item| {
        count
            + match item {
                SelectItem::Wildcard => exposed_columns.len().saturating_sub(1),
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
    let mut output = [SelectItem::Wildcard; MAX_PROJ];
    let mut count = 0usize;
    for item in items {
        if matches!(item, SelectItem::Wildcard)
            || matches!(item, SelectItem::TableWildcard(qualifier) if *qualifier == view_name)
        {
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
        } else {
            output[count] = *item;
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
    placeholder: &'a Select<'a>,
    storage: &'s Storage,
    txid: u32,
    arena: &'a Arena,
    params: &'e [Datum<'a>],
    dml_mats: &[(&'a str, &'a MaterializedCte<'a>)],
    relations: &[(&'a str, &'a MaterializedCte<'a>)],
    inherited_ctes: &CteBindings<'a>,
    inherited_materialized: &[(&'a str, &'a MaterializedCte<'a>)],
    sequences: Option<&'e dyn SequenceAccess>,
    depth: u32,
    path: Option<crate::storage::PathContext>,
    dependencies: Option<&'s crate::storage::StoredQueryDependencies>,
    authorization_role: Option<u16>,
    root_references: impl Fn(&str) -> usize,
    build: impl for<'c> FnOnce(Subst<'c, 'a, 's, 'e>) -> Result<R, SqlError>,
) -> Result<R, SqlError> {
    if with.len() > crate::sql::parser::MAX_CTES {
        return Err(sql_err!(
            sqlstate::TOO_MANY_ARGUMENTS,
            "too many WITH entries"
        ));
    }
    let mut resolved: [(&'a str, &'a Select<'a>, &'a [&'a str]); crate::sql::parser::MAX_CTES] =
        [("", placeholder, &[]); crate::sql::parser::MAX_CTES];
    let mut n = 0;
    let mut materialized: [(&'a str, &'a MaterializedCte<'a>); crate::sql::parser::MAX_CTES] =
        [("", &EMPTY_CTE); crate::sql::parser::MAX_CTES];
    let mut nm = 0;
    for &(name, relation) in relations {
        if nm == materialized.len()
            || materialized[..nm]
                .iter()
                .any(|(existing, _)| *existing == name)
        {
            return Err(sql_err!(
                sqlstate::DUPLICATE_ALIAS,
                "WITH query name \"{}\" specified more than once",
                name
            ));
        }
        materialized[nm] = (name, relation);
        nm += 1;
    }
    for (cte_index, cte) in with.iter().enumerate() {
        if with[..cte_index].iter().any(|prior| prior.name == cte.name) {
            return Err(sql_err!(
                sqlstate::DUPLICATE_ALIAS,
                "WITH query name \"{}\" specified more than once",
                cte.name
            ));
        }
        let mut scoped_ctes = [("", placeholder, &[] as &'a [&'a str]); MAX_VISIBLE_CTES];
        let mut scoped_materialized = [("", &EMPTY_CTE); MAX_VISIBLE_CTES];
        let (scoped_n, scoped_nm) = fill_scoped_bindings(
            with,
            &resolved[..n],
            inherited_ctes,
            &materialized[..nm],
            inherited_materialized,
            &mut scoped_ctes,
            &mut scoped_materialized,
        )?;
        let context = Subst {
            ctes: &scoped_ctes[..scoped_n],
            materialized: &scoped_materialized[..scoped_nm],
            storage,
            txid,
            depth,
            path,
            dependencies,
            authorization_role,
            qualifier: None,
            recursive_state: None,
            execution: Some(ExecutionSubst { params, sequences }),
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
        if cte.dml.is_some() {
            let materialized_cte = dml_mats
                .iter()
                .find(|(name, _)| *name == cte.name)
                .map(|(_, materialized_cte)| *materialized_cte)
                .ok_or_else(|| {
                    sql_err!(
                        sqlstate::INTERNAL_ERROR,
                        "data-modifying CTE \"{}\" was not materialized",
                        cte.name
                    )
                })?;
            materialized[nm] = (cte.name, materialized_cte);
            nm += 1;
        } else if cte.recursive && self_references > 0 {
            let recursive =
                materialize_recursive(cte, context, storage, txid, arena, params, sequences)?;
            materialized[nm] = (cte.name, recursive);
            nm += 1;
        } else {
            let query = subst_select(cte.query, context, arena)?;
            let references = root_references(cte.name)
                + with[cte_index + 1..]
                    .iter()
                    .map(|later| cte_references(later, cte.name))
                    .sum::<usize>();
            let volatile = select_contains_volatile(query, storage, txid);
            let requires_evaluate_once = match cte.materialization {
                CteMaterialization::Default => references > 1 || volatile,
                CteMaterialization::Materialized => true,
                CteMaterialization::NotMaterialized => volatile,
            };
            if references > 0 && requires_evaluate_once {
                let relation = materialize_query_cte(
                    cte.name,
                    cte.columns,
                    query,
                    storage,
                    txid,
                    arena,
                    params,
                    sequences,
                )?;
                materialized[nm] = (cte.name, relation);
                nm += 1;
            } else {
                resolved[n] = (cte.name, query, cte.columns);
                n += 1;
            }
        }
    }
    let mut scoped_ctes = [("", placeholder, &[] as &'a [&'a str]); MAX_VISIBLE_CTES];
    let mut scoped_materialized = [("", &EMPTY_CTE); MAX_VISIBLE_CTES];
    let (scoped_n, scoped_nm) = fill_scoped_bindings(
        with,
        &resolved[..n],
        inherited_ctes,
        &materialized[..nm],
        inherited_materialized,
        &mut scoped_ctes,
        &mut scoped_materialized,
    )?;
    let context = Subst {
        ctes: &scoped_ctes[..scoped_n],
        materialized: &scoped_materialized[..scoped_nm],
        storage,
        txid,
        depth,
        path,
        dependencies,
        authorization_role,
        qualifier: None,
        recursive_state: None,
        execution: Some(ExecutionSubst { params, sequences }),
    };
    build(context)
}

#[allow(clippy::too_many_arguments)]
fn fill_scoped_bindings<'a>(
    with: &[Cte<'a>],
    resolved: &[(&'a str, &'a Select<'a>, &'a [&'a str])],
    inherited_ctes: &CteBindings<'a>,
    materialized: &[(&'a str, &'a MaterializedCte<'a>)],
    inherited_materialized: &[(&'a str, &'a MaterializedCte<'a>)],
    scoped_ctes: &mut [(&'a str, &'a Select<'a>, &'a [&'a str]); MAX_VISIBLE_CTES],
    scoped_materialized: &mut [(&'a str, &'a MaterializedCte<'a>); MAX_VISIBLE_CTES],
) -> Result<(usize, usize), SqlError> {
    let mut scoped_n = 0;
    for binding in resolved.iter().chain(
        inherited_ctes
            .iter()
            .filter(|binding| !with.iter().any(|local| local.name == binding.0)),
    ) {
        if scoped_n == scoped_ctes.len() {
            return Err(too_many_visible_ctes());
        }
        scoped_ctes[scoped_n] = *binding;
        scoped_n += 1;
    }

    let mut scoped_nm = 0;
    for binding in materialized.iter().chain(
        inherited_materialized
            .iter()
            .filter(|binding| !with.iter().any(|local| local.name == binding.0)),
    ) {
        if scoped_nm == scoped_materialized.len() {
            return Err(too_many_visible_ctes());
        }
        scoped_materialized[scoped_nm] = *binding;
        scoped_nm += 1;
    }
    Ok((scoped_n, scoped_nm))
}

#[allow(clippy::too_many_arguments)]
fn materialize_query_cte<'a>(
    name: &str,
    aliases: &'a [&'a str],
    query: &'a Select<'a>,
    storage: &Storage,
    txid: u32,
    arena: &'a Arena,
    params: &[Datum<'a>],
    sequences: Option<&dyn SequenceAccess>,
) -> Result<&'a MaterializedCte<'a>, SqlError> {
    let tree = arena
        .alloc(SetTree::Select(query))
        .map_err(|_| arena_full())?;
    let mut described = [ColDesc::new("", 0, 0); MAX_PROJ];
    let ncols = describe_set_body(storage, tree, txid, &mut described, arena)?;
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
    let (rows, _, materialized_columns) =
        materialize_set_body(storage, txid, tree, arena, params, sequences)?;
    debug_assert_eq!(materialized_columns, ncols);
    arena
        .alloc(MaterializedCte {
            column_names,
            column_types,
            column_collations,
            source: crate::sql::ast::MaterializedCteSource::Inline(rows),
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
) -> Result<usize, SqlError> {
    let body = expand_set_tree(q.with, q.body, storage, txid, arena)?;
    describe_set_body(storage, body, txid, columns, arena)
}

/// Expands WITH CTEs and view references across a whole set-operation tree
/// (schema-only: a self-referencing recursive CTE binds its non-recursive
/// term's shape, as in [`expand_ctes`]).
pub(crate) fn expand_set_tree<'a>(
    with: &'a [Cte<'a>],
    tree: &'a SetTree<'a>,
    storage: &'a Storage,
    txid: u32,
    arena: &'a Arena,
) -> Result<&'a SetTree<'a>, SqlError> {
    if with.is_empty() && !storage.has_any_view() {
        return Ok(tree);
    }
    let wrapper = wrap_set_tree_with(with, tree, arena)?;
    let expanded = expand_ctes(wrapper, storage, txid, arena)?;
    Ok(expanded.set_body.expect("wrapper keeps its set body"))
}

/// Like [`expand_set_tree`], but for execution: recursive CTEs materialize to
/// their fixpoint (see [`expand_ctes_exec`]).
pub(super) fn expand_set_tree_exec<'a>(
    with: &'a [Cte<'a>],
    tree: &'a SetTree<'a>,
    storage: &'a Storage,
    txid: u32,
    arena: &'a Arena,
    params: &[Datum<'a>],
    sequences: Option<&dyn SequenceAccess>,
) -> Result<&'a SetTree<'a>, SqlError> {
    if with.is_empty() && !storage.has_any_view() {
        return Ok(tree);
    }
    let wrapper = wrap_set_tree_with(with, tree, arena)?;
    let expanded = expand_ctes_exec(wrapper, storage, txid, arena, params, &[], sequences)?;
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

static EMPTY_CTE: MaterializedCte<'static> = MaterializedCte {
    column_names: &[],
    column_types: &[],
    column_collations: &[],
    source: crate::sql::ast::MaterializedCteSource::Inline(&[]),
};

static EMPTY_SELECT: Select<'static> = Select {
    items: &[],
    distinct: false,
    distinct_on: &[],
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
    set_body: None,
    locking: &[],
};

type CteBindings<'a> = [(&'a str, &'a Select<'a>, &'a [&'a str])];

const MAX_VISIBLE_CTES: usize = crate::sql::parser::MAX_CTES * (MAX_VIEW_DEPTH as usize + 1);

fn too_many_visible_ctes() -> SqlError {
    sql_err!(
        sqlstate::TOO_MANY_ARGUMENTS,
        "nested WITH scopes expose more than {} common table expressions",
        MAX_VISIBLE_CTES
    )
}

/// Threaded through the FROM-reference rewrite: CTE bindings in scope (query
/// plus optional column-rename list), materialized recursive CTEs, storage (to
/// resolve view names), and the current view-expansion depth (a cycle /
/// runaway-nesting guard).
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
    dependencies: Option<&'s crate::storage::StoredQueryDependencies>,
    /// Privilege identity inherited while expanding a stored view body.
    /// `None` means the current effective role at the statement boundary.
    authorization_role: Option<u16>,
    /// DML on an auto-updatable view is executed against its base table.
    /// Qualified target references must follow that rewrite too.
    qualifier: Option<ViewQualifier<'a>>,
    recursive_state: Option<RecursiveStateSubst<'a>>,
    execution: Option<ExecutionSubst<'e, 'a>>,
}

#[derive(Clone, Copy)]
struct ExecutionSubst<'e, 'a> {
    params: &'e [Datum<'a>],
    sequences: Option<&'e dyn SequenceAccess>,
}

#[derive(Clone, Copy)]
struct ViewQualifier<'a> {
    from: &'a str,
    to: &'a str,
    to_schema: &'a str,
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
fn select_references(s: &Select, name: &str) -> usize {
    let mut count = s.set_body.map_or(0, |tree| set_tree_references(tree, name));
    if let Some(f) = &s.from {
        count += from_references(f, name);
    }
    for it in s.items {
        count += select_item_references(it, name);
    }
    count += s
        .where_clause
        .map_or(0, |expression| expr_references(expression, name));
    count += s
        .having
        .map_or(0, |expression| expr_references(expression, name));
    count += s
        .group_by
        .iter()
        .map(|expression| expr_references(expression, name))
        .sum::<usize>();
    count += s
        .distinct_on
        .iter()
        .map(|expression| expr_references(expression, name))
        .sum::<usize>();
    count += s
        .order_by
        .iter()
        .map(|order| expr_references(order.expression, name))
        .sum::<usize>();
    count += s
        .limit
        .map_or(0, |expression| expr_references(expression, name));
    count += s
        .offset
        .map_or(0, |expression| expr_references(expression, name));
    count += s
        .with
        .iter()
        .map(|cte| cte_references(cte, name))
        .sum::<usize>();
    count
}

fn cte_references(cte: &Cte<'_>, name: &str) -> usize {
    cte.dml.map_or_else(
        || select_references(cte.query, name),
        |statement| statement_references(statement, name),
    )
}

fn select_item_references(item: &SelectItem<'_>, name: &str) -> usize {
    match item {
        SelectItem::Expr { expression, .. } | SelectItem::RecordStar(expression) => {
            expr_references(expression, name)
        }
        SelectItem::Wildcard | SelectItem::TableWildcard(_) => 0,
    }
}

fn from_references(from: &FromClause<'_>, name: &str) -> usize {
    tref_references(&from.base, name)
        + from
            .joins
            .iter()
            .map(|join| {
                tref_references(&join.table, name)
                    + join.on.map_or(0, |on| expr_references(on, name))
            })
            .sum::<usize>()
}

fn returning_references(items: &[SelectItem<'_>], name: &str) -> usize {
    items
        .iter()
        .map(|item| select_item_references(item, name))
        .sum()
}

fn statement_references(statement: &Stmt<'_>, name: &str) -> usize {
    match statement {
        Stmt::Select(select) => select_references(select, name),
        Stmt::SetQuery(query) => {
            set_tree_references(query.body, name)
                + query
                    .order_by
                    .iter()
                    .map(|order| expr_references(order.expression, name))
                    .sum::<usize>()
                + query
                    .limit
                    .map_or(0, |expression| expr_references(expression, name))
                + query
                    .offset
                    .map_or(0, |expression| expr_references(expression, name))
        }
        Stmt::With { ctes, statement } => {
            ctes.iter()
                .map(|cte| cte_references(cte, name))
                .sum::<usize>()
                + statement_references(statement, name)
        }
        Stmt::Insert(insert) => {
            insert
                .select
                .map_or(0, |select| select_references(select, name))
                + insert
                    .rows
                    .iter()
                    .flat_map(|row| row.iter())
                    .map(|expression| expr_references(expression, name))
                    .sum::<usize>()
                + insert.on_conflict.map_or(0, |conflict| {
                    conflict
                        .target
                        .iter()
                        .map(|target| expr_references(target.expression, name))
                        .sum::<usize>()
                        + conflict.update.map_or(0, |assignments| {
                            assignments
                                .iter()
                                .map(|(_, expression)| expr_references(expression, name))
                                .sum::<usize>()
                        })
                        + conflict
                            .update_where
                            .map_or(0, |expression| expr_references(expression, name))
                })
                + returning_references(insert.returning, name)
        }
        Stmt::Update(update) => {
            update.from.map_or(0, |from| from_references(from, name))
                + update
                    .assignments
                    .iter()
                    .map(|(_, expression)| expr_references(expression, name))
                    .sum::<usize>()
                + update
                    .where_clause
                    .map_or(0, |expression| expr_references(expression, name))
                + returning_references(update.returning, name)
        }
        Stmt::Delete(delete) => {
            delete.using.map_or(0, |from| from_references(from, name))
                + delete
                    .where_clause
                    .map_or(0, |expression| expr_references(expression, name))
                + returning_references(delete.returning, name)
        }
        Stmt::Merge(merge) => {
            tref_references(&merge.source, name)
                + expr_references(merge.on, name)
                + merge
                    .whens
                    .iter()
                    .map(|when| {
                        when.cond
                            .map_or(0, |condition| expr_references(condition, name))
                            + match when.action {
                                MergeAction::Update(assignments) => assignments
                                    .iter()
                                    .map(|(_, expression)| expr_references(expression, name))
                                    .sum(),
                                MergeAction::Insert { values, .. } => values
                                    .iter()
                                    .map(|expression| expr_references(expression, name))
                                    .sum(),
                                MergeAction::Delete | MergeAction::DoNothing => 0,
                            }
                    })
                    .sum::<usize>()
        }
        _ => 0,
    }
}

fn tref_references(t: &TableRef, name: &str) -> usize {
    if let Some(sub) = t.subquery {
        return select_references(sub, name);
    }
    let argument_references = t.func_args.map_or(0, |arguments| {
        arguments
            .iter()
            .map(|argument| expr_references(argument, name))
            .sum()
    });
    let grouped_references = t.rows_from.map_or(0, |functions| {
        functions
            .iter()
            .map(|function| tref_references(function, name))
            .sum()
    });
    let sample_references = t.sample.map_or(0, |sample| {
        expr_references(sample.percentage, name)
            + sample
                .repeatable
                .map_or(0, |repeatable| expr_references(repeatable, name))
    });
    argument_references
        + grouped_references
        + sample_references
        + usize::from(t.schema.is_none() && !t.is_function_source() && t.table == name)
}

fn set_tree_references(tree: &SetTree, name: &str) -> usize {
    match tree {
        SetTree::Select(s) => select_references(s, name),
        SetTree::Op { left, right, .. } => {
            set_tree_references(left, name) + set_tree_references(right, name)
        }
    }
}

/// Number of references to `name` inside expression subqueries of `e`.
fn expr_references(e: &Expr, name: &str) -> usize {
    match e {
        Expr::Subquery(s) | Expr::Exists(s) | Expr::ArraySubquery(s) => select_references(s, name),
        Expr::InSubquery {
            operand, select, ..
        }
        | Expr::QuantifiedSubquery {
            operand, select, ..
        } => expr_references(operand, name) + select_references(select, name),
        Expr::Unary { operand, .. }
        | Expr::Cast { operand, .. }
        | Expr::Collate { operand, .. }
        | Expr::IsNull { operand, .. } => expr_references(operand, name),
        Expr::Binary { left, right, .. } => {
            expr_references(left, name) + expr_references(right, name)
        }
        Expr::Call {
            args,
            order_by,
            over,
            filter,
            ..
        } => {
            args.iter()
                .map(|argument| expr_references(argument, name))
                .sum::<usize>()
                + order_by
                    .iter()
                    .map(|order| expr_references(order.expression, name))
                    .sum::<usize>()
                + over.map_or(0, |window| {
                    window
                        .partition_by
                        .iter()
                        .map(|expression| expr_references(expression, name))
                        .sum::<usize>()
                        + window
                            .order_by
                            .iter()
                            .map(|order| expr_references(order.expression, name))
                            .sum::<usize>()
                        + window.frame.map_or(0, |frame| {
                            frame_bound_references(frame.start, name)
                                + frame_bound_references(frame.end, name)
                        })
                })
                + filter.map_or(0, |expression| expr_references(expression, name))
        }
        Expr::InList { operand, list, .. } => {
            expr_references(operand, name)
                + list.iter().map(|x| expr_references(x, name)).sum::<usize>()
        }
        Expr::Between {
            operand, low, high, ..
        } => {
            expr_references(operand, name)
                + expr_references(low, name)
                + expr_references(high, name)
        }
        Expr::Like {
            operand,
            pattern,
            escape,
            ..
        } => {
            expr_references(operand, name)
                + expr_references(pattern, name)
                + escape.map_or(0, |expression| expr_references(expression, name))
        }
        Expr::Match {
            operand, pattern, ..
        } => expr_references(operand, name) + expr_references(pattern, name),
        Expr::Case {
            operand,
            whens,
            otherwise,
            ..
        } => {
            operand.map_or(0, |o| expr_references(o, name))
                + whens
                    .iter()
                    .map(|(c, r)| expr_references(c, name) + expr_references(r, name))
                    .sum::<usize>()
                + otherwise.map_or(0, |o| expr_references(o, name))
        }
        Expr::Array(items) => items.iter().map(|x| expr_references(x, name)).sum(),
        Expr::Subscript { base, index } => {
            expr_references(base, name) + expr_references(index, name)
        }
        Expr::Slice { base, lower, upper } => {
            expr_references(base, name)
                + lower.map_or(0, |expression| expr_references(expression, name))
                + upper.map_or(0, |expression| expr_references(expression, name))
        }
        Expr::Field { base, .. } => expr_references(base, name),
        Expr::AnyAll { operand, array, .. } => {
            expr_references(operand, name) + expr_references(array, name)
        }
        _ => 0,
    }
}

fn frame_bound_references(bound: crate::sql::ast::FrameBound<'_>, name: &str) -> usize {
    match bound {
        crate::sql::ast::FrameBound::Preceding(expression)
        | crate::sql::ast::FrameBound::Following(expression) => expr_references(expression, name),
        crate::sql::ast::FrameBound::UnboundedPreceding
        | crate::sql::ast::FrameBound::CurrentRow
        | crate::sql::ast::FrameBound::UnboundedFollowing => 0,
    }
}

fn select_mentions_column(select: &Select<'_>, qualifier: &str, name: &str) -> bool {
    select.items.iter().any(|item| match item {
        SelectItem::Expr { expression, .. } | SelectItem::RecordStar(expression) => {
            expr_mentions_column(expression, qualifier, name)
        }
        SelectItem::Wildcard | SelectItem::TableWildcard(_) => false,
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

fn expr_mentions_column(expression: &Expr<'_>, qualifier: &str, name: &str) -> bool {
    let child = |expression| expr_mentions_column(expression, qualifier, name);
    match expression {
        Expr::Column {
            qualifier: written,
            name: written_name,
        } => *written_name == name && written.is_none_or(|written| written == qualifier),
        Expr::Subquery(select) | Expr::Exists(select) | Expr::ArraySubquery(select) => {
            select_mentions_column(select, qualifier, name)
        }
        Expr::InSubquery {
            operand, select, ..
        }
        | Expr::QuantifiedSubquery {
            operand, select, ..
        } => child(operand) || select_mentions_column(select, qualifier, name),
        Expr::Unary { operand, .. }
        | Expr::Cast { operand, .. }
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
        Expr::Between {
            operand, low, high, ..
        } => child(operand) || child(low) || child(high),
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

fn expression_contains_volatile(expression: &Expr<'_>, storage: &Storage, txid: u32) -> bool {
    if expression.contains_volatile_function().is_some()
        || matches!(expression, Expr::Call { name, args, .. }
            if storage.has_volatile_function_routine_candidate(name, args.len(), txid))
    {
        return true;
    }
    let nested = match expression {
        Expr::Subquery(select) | Expr::Exists(select) | Expr::ArraySubquery(select) => {
            select_contains_volatile(select, storage, txid)
        }
        Expr::InSubquery {
            operand, select, ..
        }
        | Expr::QuantifiedSubquery {
            operand, select, ..
        } => {
            expression_contains_volatile(operand, storage, txid)
                || select_contains_volatile(select, storage, txid)
        }
        _ => false,
    };
    if nested {
        return true;
    }
    let mut found = false;
    super::walk_children(expression, &mut |child| {
        found |= expression_contains_volatile(child, storage, txid);
        Ok(())
    })
    .expect("expression visitor cannot fail");
    found
}

fn select_contains_volatile(select: &Select<'_>, storage: &Storage, txid: u32) -> bool {
    let expression_has_it =
        |expression: &Expr<'_>| expression_contains_volatile(expression, storage, txid);
    if select.items.iter().any(|item| match item {
        SelectItem::Expr { expression, .. } | SelectItem::RecordStar(expression) => {
            expression_has_it(expression)
        }
        SelectItem::Wildcard | SelectItem::TableWildcard(_) => false,
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
                .is_some_and(|query| select_contains_volatile(query, storage, txid))
                || table.func_args.is_some_and(|arguments| {
                    arguments.iter().any(|argument| expression_has_it(argument))
                })
                || table.rows_from.is_some_and(|tables| {
                    tables.iter().any(|table| {
                        table.func_args.is_some_and(|arguments| {
                            arguments.iter().any(|argument| expression_has_it(argument))
                        }) || table
                            .subquery
                            .is_some_and(|query| select_contains_volatile(query, storage, txid))
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
        .is_some_and(|tree| set_tree_contains_volatile(tree, storage, txid))
        || select
            .with
            .iter()
            .any(|cte| select_contains_volatile(cte.query, storage, txid))
}

fn set_tree_contains_volatile(tree: &SetTree<'_>, storage: &Storage, txid: u32) -> bool {
    match tree {
        SetTree::Select(select) => select_contains_volatile(select, storage, txid),
        SetTree::Op { left, right, .. } => {
            set_tree_contains_volatile(left, storage, txid)
                || set_tree_contains_volatile(right, storage, txid)
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
    }) = q.set_body
    else {
        return Err(sql_err!(
            crate::sql::eval::sqlstate::INVALID_RECURSION,
            "recursive query \"{}\" does not have the form non-recursive-term UNION [ALL] recursive-term",
            name
        ));
    };
    if !q.order_by.is_empty() || q.limit.is_some() || q.offset.is_some() {
        return Err(sql_err!(
            sqlstate::FEATURE_NOT_SUPPORTED,
            "ORDER BY/LIMIT in a recursive query is not implemented"
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
                    crate::sql::exec::infer_type_catalog(value, None, storage, txid)?.0;
                let default_type =
                    crate::sql::exec::infer_type_catalog(default, None, storage, txid)?.0;
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
            authorization_role: None,
            qualifier: None,
            recursive_state: None,
            execution: None,
        },
        arena,
    )?;
    let mut columns = [ColDesc::new("", 0, 0); MAX_PROJ];
    describe_set_body(storage, rewritten, txid, &mut columns, arena)?;
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
    let mut values = [Datum::Null; crate::sql::array::MAX_ELEMENTS];
    let mut count = 0usize;
    if let Some(Datum::Array {
        element: ArrElem::Record,
        raw,
    }) = parent
    {
        count = crate::sql::array::len(raw);
        if count >= values.len() {
            return Err(sql_err!(
                sqlstate::PROGRAM_LIMIT_EXCEEDED,
                "recursive path exceeds the array limit"
            ));
        }
        for (index, value) in values.iter_mut().enumerate().take(count) {
            *value = crate::sql::array::get_record(raw, index, arena)?.unwrap_or(Datum::Null);
        }
    }
    values[count] = key;
    Ok(Datum::Array {
        element: ArrElem::Record,
        raw: crate::sql::array::build(&values[..=count], arena)?,
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
    for index in 0..crate::sql::array::len(raw) {
        let member = crate::sql::array::get_record(raw, index, arena)?.unwrap_or(Datum::Null);
        if crate::sql::eval::membership_eq(&member, key)? == Some(true) {
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
                        value: Datum::Null,
                    })
                    .map_err(|_| arena_full())?;
                fields[0] = RecordField {
                    name: "*DEPTH*",
                    type_oid: crate::sql::types::oid::INT8,
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
        if crate::sql::eval::membership_eq(&mark, &cycle.mark_value)? != Some(true) {
            output[count] = row;
            count += 1;
        }
    }
    Ok(&output[..count])
}

fn recursive_table_qualifier<'a>(select: &'a Select<'a>, name: &str) -> Option<&'a str> {
    let matches = |table: &'a TableRef<'a>| {
        (table.schema.is_none()
            && table.subquery.is_none()
            && !table.is_function_source()
            && table.table == name)
            .then_some(table.alias.unwrap_or(table.table))
    };
    let from = select.from.as_ref()?;
    matches(&from.base).or_else(|| from.joins.iter().find_map(|join| matches(&join.table)))
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
            authorization_role: None,
            qualifier: None,
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
            "select list is too wide"
        ));
    }
    let mut items = [SelectItem::Wildcard; MAX_PROJ];
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
    let ncols = describe_set_body(storage, base_tree, txid, &mut described, arena)?;
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
        Ok(if sorted {
            left.cmp(right)
        } else {
            core::cmp::Ordering::Equal
        })
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
    )?;
    storage
        .with_block_store(|blocks| sorter.finish(blocks, &mut compare))
        .expect("recursive work table has a block store")
}

fn external_decorate_recursive_run(
    storage: &Storage,
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
        Ok(if sorted {
            left.cmp(right)
        } else {
            core::cmp::Ordering::Equal
        })
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
        Ok(if sorted {
            left.cmp(right)
        } else {
            core::cmp::Ordering::Equal
        })
    };
    storage
        .with_block_store(|blocks| reader.start(blocks, input))
        .expect("recursive work table has a block store")?;
    while let Some(row) = reader.row() {
        let mark = arena.mark();
        let cycle_mark = crate::sql::exec::decode_projected_col_record(row, cycle.mark, arena)?;
        if crate::sql::eval::membership_eq(&cycle_mark, &cycle.mark_value)? != Some(true) {
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
    run: Option<crate::sql::external::ExternalRun>,
) -> Result<Option<crate::sql::external::ExternalRun>, SqlError> {
    let Some(run) = run else {
        return Ok(None);
    };
    let mut reader = storage.external_run_reader()?;
    let mut sorter = storage.external_sorter()?;
    sorter.reset();
    let mut compare = |left: &[u8], right: &[u8]| Ok(left.cmp(right));
    storage
        .with_block_store(|blocks| reader.start(blocks, run))
        .expect("recursive work table has a block store")?;
    while reader.row().is_some() {
        let keep = {
            let context = reader.context().expect("checked");
            context.previous != Some(context.row)
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
    let mut compare = |left: &[u8], right: &[u8]| Ok(left.cmp(right));
    storage
        .with_block_store(|blocks| candidate_reader.start(blocks, candidates))
        .expect("recursive work table has a block store")?;
    storage
        .with_block_store(|blocks| seen_reader.start(blocks, seen))
        .expect("recursive work table has a block store")?;
    while let Some(candidate) = candidate_reader.row() {
        while seen_reader
            .row()
            .is_some_and(|seen_row| seen_row < candidate)
        {
            storage
                .with_block_store(|blocks| seen_reader.advance(blocks))
                .expect("recursive work table has a block store")?;
        }
        if seen_reader.row() != Some(candidate) {
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
    left: Option<crate::sql::external::ExternalRun>,
    right: Option<crate::sql::external::ExternalRun>,
    sorted: bool,
) -> Result<Option<crate::sql::external::ExternalRun>, SqlError> {
    let mut sorter = storage.external_sorter()?;
    sorter.reset();
    let mut compare = |left: &[u8], right: &[u8]| {
        Ok(if sorted {
            left.cmp(right)
        } else {
            core::cmp::Ordering::Equal
        })
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
    let ncols = describe_set_body(storage, base_tree, txid, &mut described, arena)?;
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

    if storage.spill_attached() {
        let base_raw = external_recursive_tree(
            base_tree, storage, txid, arena, params, sequences, !union_all,
        )?;
        let base = if decorated {
            external_decorate_recursive_run(
                storage, base_raw, decoration, true, !union_all, false, arena,
            )?
        } else {
            base_raw
        };
        let mut all = if union_all {
            base
        } else {
            external_unique_run(storage, base)?
        };
        let mut working = all;
        let working_source = arena.alloc_atomic_usize(0).map_err(|_| arena_full())?;
        let working_cte = arena
            .alloc(MaterializedCte {
                column_names: base_column_names,
                column_types: base_column_types,
                column_collations: base_column_collations,
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
            authorization_role: None,
            qualifier: None,
            recursive_state: None,
            execution: outer.execution,
        };
        let step_tree = subst_set_tree(recursive_tree, context, arena)?;
        let mut step_desc = [ColDesc::new("", 0, 0); MAX_PROJ];
        let stepn = describe_set_body(storage, step_tree, txid, &mut step_desc, arena)?;
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
                let unique = external_unique_run(storage, candidates)?;
                external_recursive_difference(storage, unique, all)?
            };
            if fresh.is_none_or(|run| run.rows() == 0) {
                unsafe { arena.rewind_to(iteration_mark) };
                break;
            }
            let next_working =
                external_recursive_noncycle_run(storage, fresh, decoration, !union_all, arena)?;
            all = external_recursive_union(storage, all, fresh, !union_all)?;
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
                source: crate::sql::ast::MaterializedCteSource::External(all),
            })
            .map_err(|_| arena_full())?);
    }

    // Base rows; UNION (without ALL) deduplicates them among themselves.
    // Projected-row encoding is order-preserving-for-equality, so byte equality
    // is row equality.
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
            if !deduped[..kept].contains(&r) {
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
        authorization_role: None,
        qualifier: None,
        recursive_state: None,
        execution: outer.execution,
    };
    let step_tree = subst_set_tree(recursive_tree, context, arena)?;
    let mut step_desc = [ColDesc::new("", 0, 0); MAX_PROJ];
    let stepn = describe_set_body(storage, step_tree, txid, &mut step_desc, arena)?;
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
                if !all_rows.contains(&r) && !kept_rows[..kept].contains(&r) {
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
            source: crate::sql::ast::MaterializedCteSource::Inline(all_rows),
        })
        .map_err(|_| arena_full())?)
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
            select,
            context.storage,
            context.txid,
            arena,
            execution.params,
            &[],
            &[],
            context.ctes,
            context.materialized,
            execution.sequences,
            context.depth,
            context.path,
            context.dependencies,
            context.authorization_role,
            |name| select_references(select, name),
            |inner| subst_select_body(select, inner, arena),
        );
    }

    let mut resolved = [("", select, &[] as &'a [&'a str]); crate::sql::parser::MAX_CTES];
    let mut resolved_count = 0usize;
    for (index, cte) in select.with.iter().enumerate() {
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
        let mut scoped = [("", select, &[] as &'a [&'a str]); MAX_VISIBLE_CTES];
        let mut scoped_count = 0usize;
        for binding in &resolved[..resolved_count] {
            if scoped_count == scoped.len() {
                return Err(too_many_visible_ctes());
            }
            scoped[scoped_count] = *binding;
            scoped_count += 1;
        }
        for binding in context.ctes {
            if select.with.iter().any(|local| local.name == binding.0) {
                continue;
            }
            if scoped_count == scoped.len() {
                return Err(too_many_visible_ctes());
            }
            scoped[scoped_count] = *binding;
            scoped_count += 1;
        }
        let child = Subst {
            ctes: &scoped[..scoped_count],
            ..context
        };
        let query = if cte.recursive && select_references(cte.query, cte.name) > 0 {
            let (base, _, _) = recursive_parts(cte.query, cte.name)?;
            subst_select(wrap_set_tree(base, arena)?, child, arena)?
        } else {
            subst_select(cte.query, child, arena)?
        };
        resolved[resolved_count] = (cte.name, query, cte.columns);
        resolved_count += 1;
    }
    let mut scoped = [("", select, &[] as &'a [&'a str]); MAX_VISIBLE_CTES];
    let mut scoped_count = 0usize;
    for binding in &resolved[..resolved_count] {
        if scoped_count == scoped.len() {
            return Err(too_many_visible_ctes());
        }
        scoped[scoped_count] = *binding;
        scoped_count += 1;
    }
    for binding in context.ctes {
        if select.with.iter().any(|local| local.name == binding.0) {
            continue;
        }
        if scoped_count == scoped.len() {
            return Err(too_many_visible_ctes());
        }
        scoped[scoped_count] = *binding;
        scoped_count += 1;
    }
    subst_select_body(
        select,
        Subst {
            ctes: &scoped[..scoped_count],
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
    let mut order = [OrderBy {
        expression: &Expr::Null,
        descending: false,
        nulls_first: false,
    }; crate::sql::parser::MAX_LIST];
    if s.order_by.len() > crate::sql::parser::MAX_LIST {
        return Err(sql_err!(
            sqlstate::TOO_MANY_ARGUMENTS,
            "ORDER BY list too long"
        ));
    }
    for (i, ob) in s.order_by.iter().enumerate() {
        order[i] = OrderBy {
            expression: subst_expr(ob.expression, context, arena)?,
            ..*ob
        };
    }
    let order_by = arena
        .alloc_slice_copy(&order[..s.order_by.len()])
        .map_err(|_| arena_full())?;
    let set_body = match s.set_body {
        Some(tree) => Some(subst_set_tree(tree, context, arena)?),
        None => None,
    };
    let new = Select {
        items,
        distinct: s.distinct,
        distinct_on,
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

fn from_shadows_qualifier(from: &FromClause<'_>, qualifier: &str) -> bool {
    let shadows = |table: &TableRef<'_>| table.alias.unwrap_or(table.table) == qualifier;
    shadows(&from.base) || from.joins.iter().any(|join| shadows(&join.table))
}

fn subst_select_items<'a>(
    source: &'a [SelectItem<'a>],
    context: Subst<'_, 'a, '_, '_>,
    arena: &'a Arena,
) -> Result<&'a [SelectItem<'a>], SqlError> {
    let mut items = [SelectItem::Wildcard; MAX_PROJ];
    if source.len() > MAX_PROJ {
        return Err(sql_err!(sqlstate::TOO_MANY_COLUMNS, "select list too wide"));
    }
    for (i, item) in source.iter().enumerate() {
        items[i] = match item {
            SelectItem::Wildcard => SelectItem::Wildcard,
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
    }
    arena
        .alloc_slice_copy(&items[..source.len()])
        .map(|items| &*items)
        .map_err(|_| arena_full())
}

fn subst_assignments<'a>(
    source: &'a [(&'a str, &'a Expr<'a>)],
    context: Subst<'_, 'a, '_, '_>,
    arena: &'a Arena,
) -> Result<&'a [(&'a str, &'a Expr<'a>)], SqlError> {
    if source.len() > crate::sql::parser::MAX_LIST {
        return Err(sql_err!(
            sqlstate::TOO_MANY_ARGUMENTS,
            "assignment list too long"
        ));
    }
    let mut assignments = [("", &Expr::Null); crate::sql::parser::MAX_LIST];
    for (index, (column, expression)) in source.iter().enumerate() {
        assignments[index] = (column, subst_expr(expression, context, arena)?);
    }
    arena
        .alloc_slice_copy(&assignments[..source.len()])
        .map(|assignments| &*assignments)
        .map_err(|_| arena_full())
}

fn subst_on_conflict_targets<'a>(
    source: &'a [OnConflictTarget<'a>],
    context: Subst<'_, 'a, '_, '_>,
    arena: &'a Arena,
) -> Result<&'a [OnConflictTarget<'a>], SqlError> {
    let mut targets = [OnConflictTarget {
        column: None,
        expression: &Expr::Null,
        expression_text: "",
    }; crate::sql::parser::MAX_LIST];
    for (index, target) in source.iter().enumerate() {
        targets[index] = OnConflictTarget {
            column: target.column,
            expression: subst_expr(target.expression, context, arena)?,
            expression_text: target.expression_text,
        };
    }
    arena
        .alloc_slice_copy(&targets[..source.len()])
        .map(|targets| &*targets)
        .map_err(|_| arena_full())
}

fn subst_insert<'a>(
    statement: &'a Insert<'a>,
    context: Subst<'_, 'a, '_, '_>,
    arena: &'a Arena,
) -> Result<Insert<'a>, SqlError> {
    if statement.rows.len() > crate::sql::parser::MAX_LIST {
        return Err(sql_err!(
            sqlstate::TOO_MANY_ARGUMENTS,
            "VALUES list too long"
        ));
    }
    let mut rows: [&[&Expr]; crate::sql::parser::MAX_LIST] = [&[]; crate::sql::parser::MAX_LIST];
    for (index, row) in statement.rows.iter().enumerate() {
        rows[index] = subst_expr_slice(row, context, arena)?;
    }
    let rows = arena
        .alloc_slice_copy(&rows[..statement.rows.len()])
        .map_err(|_| arena_full())?;
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
        table: statement.table,
        columns: statement.columns,
        rows,
        select,
        on_conflict,
        returning: subst_select_items(statement.returning, context, arena)?,
        overriding: statement.overriding,
    })
}

fn subst_update<'a>(
    statement: &'a Update<'a>,
    context: Subst<'_, 'a, '_, '_>,
    arena: &'a Arena,
) -> Result<Update<'a>, SqlError> {
    Ok(Update {
        table: statement.table,
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
        returning: subst_select_items(statement.returning, context, arena)?,
    })
}

fn subst_delete<'a>(
    statement: &'a Delete<'a>,
    context: Subst<'_, 'a, '_, '_>,
    arena: &'a Arena,
) -> Result<Delete<'a>, SqlError> {
    Ok(Delete {
        table: statement.table,
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
        returning: subst_select_items(statement.returning, context, arena)?,
    })
}

fn subst_merge<'a>(
    statement: &'a Merge<'a>,
    context: Subst<'_, 'a, '_, '_>,
    arena: &'a Arena,
) -> Result<Merge<'a>, SqlError> {
    if statement.whens.len() > crate::sql::parser::MAX_LIST {
        return Err(sql_err!(
            sqlstate::TOO_MANY_ARGUMENTS,
            "MERGE action list too long"
        ));
    }
    let mut whens = [MergeWhen {
        matched: false,
        cond: None,
        action: MergeAction::DoNothing,
    }; crate::sql::parser::MAX_LIST];
    for (index, when) in statement.whens.iter().enumerate() {
        let action = match when.action {
            MergeAction::Update(assignments) => {
                MergeAction::Update(subst_assignments(assignments, context, arena)?)
            }
            MergeAction::Delete => MergeAction::Delete,
            MergeAction::Insert {
                columns,
                values,
                default_values,
            } => MergeAction::Insert {
                columns,
                values: subst_expr_slice(values, context, arena)?,
                default_values,
            },
            MergeAction::DoNothing => MergeAction::DoNothing,
        };
        whens[index] = MergeWhen {
            matched: when.matched,
            cond: opt_subst(when.cond, context, arena)?,
            action,
        };
    }
    Ok(Merge {
        target: statement.target,
        target_alias: statement.target_alias,
        source: subst_tableref(&statement.source, context, arena)?,
        on: subst_expr(statement.on, context, arena)?,
        whens: arena
            .alloc_slice_copy(&whens[..statement.whens.len()])
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
        } => SetTree::Op {
            operator: *operator,
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
    let dummy = Join {
        table: f.base,
        kind: JoinKind::Inner,
        on: None,
        using: None,
        natural: false,
    };
    let mut joins = [dummy; MAX_JOIN_TABLES - 1];
    if f.joins.len() > joins.len() {
        return Err(sql_err!(sqlstate::TOO_MANY_ARGUMENTS, "too many joins"));
    }
    for (i, j) in f.joins.iter().enumerate() {
        joins[i] = Join {
            table: subst_tableref(&j.table, context, arena)?,
            kind: j.kind,
            on: opt_subst(j.on, context, arena)?,
            using: j.using,
            natural: j.natural,
        };
    }
    let joins = arena
        .alloc_slice_copy(&joins[..f.joins.len()])
        .map_err(|_| arena_full())?;
    Ok(FromClause { base, joins })
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
        let mut rewritten = [*t; crate::sql::parser::MAX_LIST];
        for (slot, function) in rewritten.iter_mut().zip(functions) {
            *slot = subst_tableref(function, context, arena)?;
        }
        return Ok(TableRef {
            rows_from: Some(
                arena
                    .alloc_slice_copy(&rewritten[..functions.len()])
                    .map_err(|_| arena_full())?,
            ),
            ..*t
        });
    }
    if let Some(arguments) = t.func_args {
        let Some(dependency) = stored_routine_dependency(t.schema.unwrap_or(""), t.table, context)
        else {
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
            rows_from: None,
            col_alias: t.col_alias,
            inheritance: t.inheritance,
            sample: t.sample,
            cte: Some(m),
            with_ordinality: false,
            lateral: false,
            authorization_role: None,
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
            rows_from: None,
            col_alias: renames,
            inheritance: t.inheritance,
            sample: t.sample,
            cte: None,
            with_ordinality: false,
            lateral: false,
            authorization_role: None,
        });
    }
    // A name resolving to a view (not shadowed by a CTE, and not out-resolved
    // by a table earlier in the path) expands to a derived table over the
    // view's stored SELECT, recursively expanded under the view creator's
    // search path.
    let captured = context.dependencies.and_then(|dependencies| {
        dependencies.entries().iter().find_map(|dependency| {
            if dependency.referenced_schema.as_str() != t.schema.unwrap_or("")
                || dependency.referenced_name.as_str() != t.table
            {
                return None;
            }
            match dependency.class {
                crate::storage::DependencyClass::Table => Some(
                    crate::storage::ResolvedRelation::Table(dependency.slot as usize),
                ),
                crate::storage::DependencyClass::View => Some(
                    crate::storage::ResolvedRelation::View(dependency.slot as usize),
                ),
                _ => None,
            }
        })
    });
    let resolved = captured.or_else(|| match context.path {
        Some(p) => context
            .storage
            .resolve_relation_under(&p, t.schema, t.table, context.txid),
        None => context
            .storage
            .resolve_relation(t.schema, t.table, context.txid),
    });
    if let Some(crate::storage::ResolvedRelation::View(slot)) = resolved {
        if context.depth >= MAX_VIEW_DEPTH {
            return Err(sql_err!(
                sqlstate::FEATURE_NOT_SUPPORTED,
                "view \"{}\" nests too deeply (or references itself)",
                t.table
            ));
        }
        let view = context.storage.view(slot);
        let requester = match context.authorization_role {
            Some(role) => role as usize,
            None => context
                .storage
                .current_role_slot(context.txid)
                .ok_or_else(|| {
                    sql_err!(
                        sqlstate::INSUFFICIENT_PRIVILEGE,
                        "current role is not present in the role catalog"
                    )
                })?,
        };
        context
            .storage
            .require_schema_usage_as(view.schema.as_str(), requester, context.txid)?;
        let view_object = crate::storage::AccessObject {
            class: crate::storage::AccessClass::View,
            slot: slot as u16,
        };
        if !context.storage.has_object_privilege(
            view_object,
            requester,
            crate::storage::PrivilegeSet::SELECT,
            context.txid,
        ) {
            return Err(sql_err!(
                sqlstate::INSUFFICIENT_PRIVILEGE,
                "permission denied for view {}",
                view.name.as_str()
            ));
        }
        let authorization_role = match view.security {
            crate::storage::ViewSecurity::Definer => {
                Some(context.storage.object_owner(view_object, context.txid) as u16)
            }
            crate::storage::ViewSecurity::Invoker => context.authorization_role,
        };
        let view_sql = arena
            .alloc_str(view.sql.as_str())
            .map_err(|_| arena_full())?;
        let user = crate::sql::eval::funcs::system::session_user_owned();
        let view_path =
            context
                .storage
                .compute_path(view.creation_path.as_str(), user.as_str(), context.txid);
        let vsel = crate::sql::parser::parse_view_select(view_sql, arena)?;
        // The view body has its own scope: no outer CTEs, deeper view depth,
        // and the creator's path for its own references.
        let dependencies = context.storage.view_dependencies(slot);
        let expanded = if let Some(execution) = context.execution {
            with_exec_context(
                vsel.with,
                vsel,
                context.storage,
                context.txid,
                arena,
                execution.params,
                &[],
                &[],
                &[],
                &[],
                execution.sequences,
                context.depth + 1,
                Some(view_path),
                Some(dependencies),
                authorization_role,
                |name| select_references(vsel, name),
                |inner| subst_select_body(vsel, inner, arena),
            )?
        } else {
            expand_ctes_with_path(
                vsel,
                context.storage,
                context.txid,
                Some(view_path),
                Some(dependencies),
                context.depth + 1,
                authorization_role,
                arena,
            )?
        };
        return Ok(TableRef {
            schema: None,
            table: "",
            alias: Some(t.alias.unwrap_or(t.table)),
            subquery: Some(expanded),
            func_args: None,
            func_argument_names: &[],
            func_variadic: false,
            rows_from: None,
            col_alias: None,
            inheritance: t.inheritance,
            sample: t.sample,
            cte: None,
            with_ordinality: false,
            lateral: false,
            authorization_role: None,
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
            authorization_role: context.authorization_role,
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
    if context.qualifier.is_none() && !xs.iter().any(|x| expr_has_subquery(x)) {
        return Ok(xs);
    }
    let mut tmp = [&Expr::Null; crate::sql::parser::MAX_LIST];
    if xs.len() > tmp.len() {
        return Err(sql_err!(
            sqlstate::TOO_MANY_ARGUMENTS,
            "expression list too long"
        ));
    }
    for (i, x) in xs.iter().enumerate() {
        tmp[i] = subst_expr(x, context, arena)?;
    }
    Ok(&*arena
        .alloc_slice_copy(&tmp[..xs.len()])
        .map_err(|_| arena_full())?)
}

/// True if `e` contains a subquery anywhere (so it needs rebuilding when CTEs
/// are substituted). Leaves and subquery-free trees are returned unchanged.
fn expr_has_subquery(e: &Expr) -> bool {
    match e {
        Expr::Subquery(_)
        | Expr::InSubquery { .. }
        | Expr::QuantifiedSubquery { .. }
        | Expr::Exists(_)
        | Expr::ArraySubquery(_) => true,
        Expr::Unary { operand, .. }
        | Expr::Cast { operand, .. }
        | Expr::Collate { operand, .. }
        | Expr::IsNull { operand, .. } => expr_has_subquery(operand),
        Expr::Binary { left, right, .. } => expr_has_subquery(left) || expr_has_subquery(right),
        Expr::Call {
            args,
            order_by,
            over,
            filter,
            ..
        } => {
            args.iter().any(|a| expr_has_subquery(a))
                || order_by.iter().any(|o| expr_has_subquery(o.expression))
                || filter.is_some_and(expr_has_subquery)
                || over.is_some_and(|window| {
                    window
                        .partition_by
                        .iter()
                        .any(|expression| expr_has_subquery(expression))
                        || window
                            .order_by
                            .iter()
                            .any(|order| expr_has_subquery(order.expression))
                        || window.frame.is_some_and(|frame| {
                            frame_bound_has_subquery(frame.start)
                                || frame_bound_has_subquery(frame.end)
                        })
                })
        }
        Expr::InList { operand, list, .. } => {
            expr_has_subquery(operand) || list.iter().any(|a| expr_has_subquery(a))
        }
        Expr::Between {
            operand, low, high, ..
        } => expr_has_subquery(operand) || expr_has_subquery(low) || expr_has_subquery(high),
        Expr::Like {
            operand,
            pattern,
            escape,
            ..
        } => {
            expr_has_subquery(operand)
                || expr_has_subquery(pattern)
                || escape.is_some_and(expr_has_subquery)
        }
        Expr::Match {
            operand, pattern, ..
        } => expr_has_subquery(operand) || expr_has_subquery(pattern),
        Expr::Case {
            operand,
            whens,
            otherwise,
            ..
        } => {
            operand.is_some_and(expr_has_subquery)
                || whens
                    .iter()
                    .any(|(c, r)| expr_has_subquery(c) || expr_has_subquery(r))
                || otherwise.is_some_and(expr_has_subquery)
        }
        Expr::Array(items) => items.iter().any(|item| expr_has_subquery(item)),
        Expr::Subscript { base, index } => expr_has_subquery(base) || expr_has_subquery(index),
        Expr::Slice { base, lower, upper } => {
            expr_has_subquery(base)
                || lower.is_some_and(expr_has_subquery)
                || upper.is_some_and(expr_has_subquery)
        }
        Expr::Field { base, .. } => expr_has_subquery(base),
        Expr::AnyAll { operand, array, .. } => {
            expr_has_subquery(operand) || expr_has_subquery(array)
        }
        _ => false,
    }
}

fn frame_bound_has_subquery(bound: crate::sql::ast::FrameBound<'_>) -> bool {
    match bound {
        crate::sql::ast::FrameBound::Preceding(expression)
        | crate::sql::ast::FrameBound::Following(expression) => expr_has_subquery(expression),
        _ => false,
    }
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

fn rewrite_stored_type_name<'a>(
    type_name: &'a str,
    context: Subst<'_, 'a, '_, '_>,
    arena: &'a Arena,
) -> Result<&'a str, SqlError> {
    let Some(dependencies) = context.dependencies else {
        return Ok(type_name);
    };
    let (bare, array) = type_name
        .strip_suffix("[]")
        .map_or((type_name, false), |bare| (bare, true));
    let (referenced_schema, referenced_name) = bare.split_once('.').unwrap_or(("", bare));
    let Some(dependency) = dependencies.entries().iter().find(|dependency| {
        matches!(
            dependency.class,
            crate::storage::DependencyClass::Domain
                | crate::storage::DependencyClass::Enum
                | crate::storage::DependencyClass::Composite
        ) && dependency.referenced_schema.as_str() == referenced_schema
            && dependency.referenced_name.as_str() == referenced_name
    }) else {
        return Ok(type_name);
    };
    let (schema, name) = match dependency.class {
        crate::storage::DependencyClass::Domain => {
            let definition = context
                .storage
                .domain_for(dependency.slot as usize, context.txid);
            (definition.schema, definition.name)
        }
        crate::storage::DependencyClass::Enum => {
            let definition = context
                .storage
                .enum_for(dependency.slot as usize, context.txid);
            (definition.schema, definition.name)
        }
        crate::storage::DependencyClass::Composite => {
            let definition = context
                .storage
                .composite_for(dependency.slot as usize, context.txid);
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

fn stored_routine_dependency<'a>(
    schema: &str,
    name: &str,
    context: Subst<'_, 'a, '_, '_>,
) -> Option<crate::storage::StoredQueryDependency> {
    context
        .dependencies?
        .entries()
        .iter()
        .copied()
        .find(|dependency| {
            dependency.class == crate::storage::DependencyClass::Routine
                && dependency.referenced_schema.as_str() == schema
                && dependency.referenced_name.as_str() == name
        })
}

fn rewrite_stored_routine_name<'a>(
    name: &'a str,
    context: Subst<'_, 'a, '_, '_>,
    arena: &'a Arena,
) -> Result<&'a str, SqlError> {
    let (schema, bare) = name.split_once('.').unwrap_or(("", name));
    let Some(dependency) = stored_routine_dependency(schema, bare, context) else {
        return Ok(name);
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
    arena
        .alloc_str(qualified.as_str())
        .map_err(|_| arena_full())
}

fn subst_expr<'a>(
    e: &'a Expr<'a>,
    context: Subst<'_, 'a, '_, '_>,
    arena: &'a Arena,
) -> Result<&'a Expr<'a>, SqlError> {
    if context.qualifier.is_none()
        && context.recursive_state.is_none()
        && context.dependencies.is_none()
        && !expr_has_subquery(e)
    {
        return Ok(e);
    }
    let rebuilt = match e {
        Expr::Subquery(s) => Expr::Subquery(subst_select(s, context, arena)?),
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
        Expr::Unary { operator, operand } => Expr::Unary {
            operator: *operator,
            operand: subst_expr(operand, context, arena)?,
        },
        Expr::Binary {
            operator,
            left,
            right,
        } => Expr::Binary {
            operator: *operator,
            left: subst_expr(left, context, arena)?,
            right: subst_expr(right, context, arena)?,
        },
        Expr::Cast {
            operand,
            type_name,
            type_mod,
        } => Expr::Cast {
            operand: subst_expr(operand, context, arena)?,
            type_name: rewrite_stored_type_name(type_name, context, arena)?,
            type_mod: *type_mod,
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
        } => {
            let name = rewrite_stored_routine_name(name, context, arena)?;
            let mut ob = [OrderBy {
                expression: &Expr::Null,
                descending: false,
                nulls_first: false,
            }; crate::sql::parser::MAX_LIST];
            if order_by.len() > ob.len() {
                return Err(sql_err!(
                    sqlstate::TOO_MANY_ARGUMENTS,
                    "aggregate ORDER BY list too long"
                ));
            }
            for (i, o) in order_by.iter().enumerate() {
                ob[i] = OrderBy {
                    expression: subst_expr(o.expression, context, arena)?,
                    ..*o
                };
            }
            let order_by = arena
                .alloc_slice_copy(&ob[..order_by.len()])
                .map_err(|_| arena_full())?;
            let over = match over {
                None => None,
                Some(w) => {
                    let mut ob2 = [OrderBy {
                        expression: &Expr::Null,
                        descending: false,
                        nulls_first: false,
                    }; crate::sql::parser::MAX_LIST];
                    if w.order_by.len() > ob2.len() {
                        return Err(sql_err!(
                            sqlstate::TOO_MANY_ARGUMENTS,
                            "window ORDER BY list too long"
                        ));
                    }
                    for (i, o) in w.order_by.iter().enumerate() {
                        ob2[i] = OrderBy {
                            expression: subst_expr(o.expression, context, arena)?,
                            ..*o
                        };
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
                        order_by: arena
                            .alloc_slice_copy(&ob2[..w.order_by.len()])
                            .map_err(|_| arena_full())?,
                        frame,
                    };
                    Some(&*arena.alloc(spec).map_err(|_| arena_full())?)
                }
            };
            let filter = match filter {
                None => None,
                Some(f) => Some(subst_expr(f, context, arena)?),
            };
            Expr::Call {
                name,
                args: subst_expr_slice(args, context, arena)?,
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
        Expr::Between {
            operand,
            low,
            high,
            negated,
        } => Expr::Between {
            operand: subst_expr(operand, context, arena)?,
            low: subst_expr(low, context, arena)?,
            high: subst_expr(high, context, arena)?,
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
            synthetic,
        } => {
            let operand = opt_subst(*operand, context, arena)?;
            let mut ws = [(&Expr::Null, &Expr::Null); crate::sql::parser::MAX_LIST];
            if whens.len() > ws.len() {
                return Err(sql_err!(
                    sqlstate::TOO_MANY_ARGUMENTS,
                    "CASE has too many WHEN branches"
                ));
            }
            for (i, (c, r)) in whens.iter().enumerate() {
                ws[i] = (
                    subst_expr(c, context, arena)?,
                    subst_expr(r, context, arena)?,
                );
            }
            let whens = arena
                .alloc_slice_copy(&ws[..whens.len()])
                .map_err(|_| arena_full())?;
            Expr::Case {
                operand,
                whens,
                otherwise: opt_subst(*otherwise, context, arena)?,
                synthetic: *synthetic,
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
                    name,
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
                name,
            },
            _ => Expr::SchemaColumn {
                schema,
                table,
                name,
            },
        },
        // Leaves never reach here (guarded by expr_has_subquery above).
        other => *other,
    };
    Ok(&*arena.alloc(rebuilt).map_err(|_| arena_full())?)
}
