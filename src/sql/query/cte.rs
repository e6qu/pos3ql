//! `WITH` expansion and the AST substitution it rests on.
//!
//! A non-recursive CTE is inlined as a derived table wherever its name is used;
//! a recursive one is materialized by fixpoint iteration and bound to its rows.
//! Views expand the same way, from their stored text. Both rest on one thing: a
//! substituting walk that rebuilds a statement's expressions and FROM items in
//! the arena with each reference replaced, which is the rest of this module.

use crate::mem::arena::Arena;
use crate::sql::ast::{
    Cte, Expr, FromClause, Join, JoinKind, MaterializedCte, OrderBy, Select, SelectItem, SetOp,
    SetQuery, SetTree, TableRef,
};
use crate::sql::eval::{sqlstate, SqlError};
use crate::sql::exec::MAX_PROJ;
use crate::sql::types::{ColDesc, Datum};
use crate::sql_err;
use crate::storage::Storage;

use super::setops::{describe_set_body, materialize_set_body};
use super::{arena_full, check_timeout, MAX_JOIN_TABLES};

/// Expands a statement's `WITH` list (and any view reference) for the
/// describe path, which needs the shape but not the rows.
pub fn expand_ctes<'a>(
    sel: &'a Select<'a>,
    storage: &'a Storage,
    txid: u32,
    arena: &'a Arena,
) -> Result<&'a Select<'a>, SqlError> {
    // Fast path: nothing to rewrite (no CTEs anywhere and no views defined).
    if sel.with.is_empty() && !storage.has_any_view() {
        return Ok(sel);
    }
    if sel.with.len() > crate::sql::parser::MAX_CTES {
        return Err(sql_err!(sqlstate::TOO_MANY_ARGUMENTS, "too many WITH entries"));
    }
    // Resolve CTEs left-to-right so a CTE can reference earlier ones.
    let mut resolved: [(&'a str, &'a Select<'a>, &'a [&'a str]); crate::sql::parser::MAX_CTES] =
        [("", sel, &[]); crate::sql::parser::MAX_CTES];
    let mut n = 0;
    for cte in sel.with {
        if resolved[..n].iter().any(|(name, _, _)| *name == cte.name) {
            return Err(sql_err!(sqlstate::DUPLICATE_ALIAS, "WITH query name \"{}\" specified more than once", cte.name));
        }
        let context = Subst { ctes: &resolved[..n], materialized: &[], storage, txid, depth: 0 };
        // A self-referencing recursive CTE cannot be inlined; this schema-only
        // path (Describe / view validation) binds its non-recursive term,
        // which carries the CTE's column shape. Execution goes through
        // `expand_ctes_exec`, which materializes the fixpoint.
        let q = if cte.recursive && select_references(cte.query, cte.name) > 0 {
            let (base, _, _) = recursive_parts(cte.query, cte.name)?;
            let wrapped = wrap_set_tree(base, arena)?;
            subst_select(wrapped, context, arena)?
        } else {
            subst_select(cte.query, context, arena)?
        };
        resolved[n] = (cte.name, q, cte.columns);
        n += 1;
    }
    // Substitute the body against all CTEs (the WITH list is dropped by
    // subst_select, which never copies it) and expand any view references.
    let context = Subst { ctes: &resolved[..n], materialized: &[], storage, txid, depth: 0 };
    subst_select(sel, context, arena)
}

/// Like [`expand_ctes`], but for execution: a self-referencing recursive CTE is
/// materialized to a fixpoint (base term, then the recursive term iterated with
/// the CTE name bound to the previous iteration's rows) and its references
/// resolve to the finished row set.
pub fn expand_ctes_exec<'a>(
    sel: &'a Select<'a>,
    storage: &'a Storage,
    txid: u32,
    arena: &'a Arena,
    params: &[Datum<'a>],
) -> Result<&'a Select<'a>, SqlError> {
    if sel.with.is_empty() && !storage.has_any_view() {
        return Ok(sel);
    }
    if sel.with.len() > crate::sql::parser::MAX_CTES {
        return Err(sql_err!(sqlstate::TOO_MANY_ARGUMENTS, "too many WITH entries"));
    }
    let mut resolved: [(&'a str, &'a Select<'a>, &'a [&'a str]); crate::sql::parser::MAX_CTES] =
        [("", sel, &[]); crate::sql::parser::MAX_CTES];
    let mut n = 0;
    let mut materialized: [(&'a str, &'a MaterializedCte<'a>); crate::sql::parser::MAX_CTES] =
        [("", &EMPTY_CTE); crate::sql::parser::MAX_CTES];
    let mut nm = 0;
    for cte in sel.with {
        if resolved[..n].iter().any(|(name, _, _)| *name == cte.name)
            || materialized[..nm].iter().any(|(name, _)| *name == cte.name)
        {
            return Err(sql_err!(sqlstate::DUPLICATE_ALIAS, "WITH query name \"{}\" specified more than once", cte.name));
        }
        let context = Subst {
            ctes: &resolved[..n],
            materialized: &materialized[..nm],
            storage,
            txid,
            depth: 0,
        };
        if cte.recursive && select_references(cte.query, cte.name) > 0 {
            let m = materialize_recursive(cte, context, storage, txid, arena, params)?;
            materialized[nm] = (cte.name, m);
            nm += 1;
        } else {
            let q = subst_select(cte.query, context, arena)?;
            resolved[n] = (cte.name, q, cte.columns);
            n += 1;
        }
    }
    let context = Subst {
        ctes: &resolved[..n],
        materialized: &materialized[..nm],
        storage,
        txid,
        depth: 0,
    };
    subst_select(sel, context, arena)
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
pub(super) fn expand_set_tree<'a>(
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
) -> Result<&'a SetTree<'a>, SqlError> {
    if with.is_empty() && !storage.has_any_view() {
        return Ok(tree);
    }
    let wrapper = wrap_set_tree_with(with, tree, arena)?;
    let expanded = expand_ctes_exec(wrapper, storage, txid, arena, params)?;
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
        grouping_sets: &[],
        having: None,
        order_by: &[],
        limit: None,
        offset: None,
        with,
        set_body: Some(tree),
    };
    Ok(&*arena.alloc(sel).map_err(|_| arena_full())?)
}

static EMPTY_CTE: MaterializedCte<'static> =
    MaterializedCte { column_names: &[], column_types: &[], rows: &[] };

type CteBindings<'a> = [(&'a str, &'a Select<'a>, &'a [&'a str])];

/// Threaded through the FROM-reference rewrite: CTE bindings in scope (query
/// plus optional column-rename list), materialized recursive CTEs, storage (to
/// resolve view names), and the current view-expansion depth (a cycle /
/// runaway-nesting guard).
#[derive(Clone, Copy)]
struct Subst<'c, 'a> {
    ctes: &'c CteBindings<'a>,
    materialized: &'c [(&'a str, &'a MaterializedCte<'a>)],
    storage: &'a Storage,
    /// The requesting transaction, for catalog visibility (a view another
    /// transaction created but has not committed is invisible here).
    txid: u32,
    depth: u32,
}

const MAX_VIEW_DEPTH: u32 = 12;

/// Number of references to the unqualified table name `name` anywhere in a
/// select — FROM items (recursing into derived-table subqueries), the set-op
/// body, and expression subqueries.
fn select_references(s: &Select, name: &str) -> usize {
    if let Some(tree) = s.set_body {
        return set_tree_references(tree, name);
    }
    let mut count = 0usize;
    if let Some(f) = &s.from {
        count += tref_references(&f.base, name);
        for j in f.joins {
            count += tref_references(&j.table, name);
            if let Some(on) = j.on {
                count += expr_references(on, name);
            }
        }
    }
    for it in s.items {
        if let SelectItem::Expr { expression, .. } = it {
            count += expr_references(expression, name);
        }
    }
    if let Some(w) = s.where_clause {
        count += expr_references(w, name);
    }
    if let Some(h) = s.having {
        count += expr_references(h, name);
    }
    for g in s.group_by {
        count += expr_references(g, name);
    }
    count
}

fn tref_references(t: &TableRef, name: &str) -> usize {
    if let Some(sub) = t.subquery {
        return select_references(sub, name);
    }
    usize::from(t.schema.is_none() && t.func_args.is_none() && t.table == name)
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
        Expr::InSubquery { operand, select, .. } => {
            expr_references(operand, name) + select_references(select, name)
        }
        Expr::Unary { operand, .. } | Expr::Cast { operand, .. } | Expr::IsNull { operand, .. } => {
            expr_references(operand, name)
        }
        Expr::Binary { left, right, .. } => {
            expr_references(left, name) + expr_references(right, name)
        }
        Expr::Call { args, .. } => args.iter().map(|a| expr_references(a, name)).sum(),
        Expr::InList { operand, list, .. } => {
            expr_references(operand, name)
                + list.iter().map(|x| expr_references(x, name)).sum::<usize>()
        }
        Expr::Between { operand, low, high, .. } => {
            expr_references(operand, name)
                + expr_references(low, name)
                + expr_references(high, name)
        }
        Expr::Like { operand, pattern, .. } | Expr::Match { operand, pattern, .. } => {
            expr_references(operand, name) + expr_references(pattern, name)
        }
        Expr::Case { operand, whens, otherwise, .. } => {
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
        Expr::Field { base, .. } => expr_references(base, name),
        Expr::AnyAll { operand, array, .. } => {
            expr_references(operand, name) + expr_references(array, name)
        }
        _ => 0,
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
                && t.func_args.is_none()
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
    let Some(&SetTree::Op { operator: SetOp::Union, all, left, right }) = q.set_body else {
        return Err(sql_err!(
            crate::sql::eval::sqlstate::INVALID_RECURSION,
            "recursive query \"{}\" does not have the form non-recursive-term UNION [ALL] recursive-term",
            name
        ));
    };
    if !q.order_by.is_empty() || q.limit.is_some() || q.offset.is_some() {
        return Err(sql_err!(
            sqlstate::FEATURE_NOT_SUPPORTED,
            "ORDER BY/LIMIT in a recursive query body is not supported"
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
        grouping_sets: &[],
        having: None,
        order_by: &[],
        limit: None,
        offset: None,
        with: &[],
        set_body: Some(tree),
    };
    Ok(&*arena.alloc(sel).map_err(|_| arena_full())?)
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
    outer: Subst<'_, 'a>,
    storage: &'a Storage,
    txid: u32,
    arena: &'a Arena,
    params: &[Datum<'a>],
) -> Result<&'a MaterializedCte<'a>, SqlError> {
    let (base_tree, recursive_tree, union_all) = recursive_parts(cte.query, cte.name)?;
    // References to earlier CTEs inline now; the self-reference stays a bare
    // table name (it is not in `outer`'s bindings) for per-iteration binding.
    let base_tree = subst_set_tree(base_tree, outer, arena)?;
    let recursive_tree = subst_set_tree(recursive_tree, outer, arena)?;
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
    let column_names: &'a [&'a str] = {
        let mut names: [&str; MAX_PROJ] = [""; MAX_PROJ];
        for (i, slot) in names.iter_mut().enumerate().take(ncols) {
            *slot = cte.columns.get(i).copied().unwrap_or(described[i].name);
        }
        arena.alloc_slice_copy(&names[..ncols]).map_err(|_| arena_full())?
    };
    let column_types: &'a [(i32, i16)] = {
        let mut types = [(0i32, 0i16); MAX_PROJ];
        for (i, slot) in types.iter_mut().enumerate().take(ncols) {
            *slot = (described[i].type_oid, described[i].typlen);
        }
        arena.alloc_slice_copy(&types[..ncols]).map_err(|_| arena_full())?
    };

    // Base rows; UNION (without ALL) deduplicates them among themselves.
    // Projected-row encoding is order-preserving-for-equality, so byte equality
    // is row equality.
    let (base_rows, _, _) = materialize_set_body(storage, txid, base_tree, arena, params)?;
    const EMPTY: &[u8] = &[];
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

    while !working.is_empty() {
        check_timeout()?;
        // Bind the CTE name to the previous iteration's rows and evaluate the
        // recursive term.
        let working_cte = arena
            .alloc(MaterializedCte { column_names, column_types, rows: working })
            .map_err(|_| arena_full())?;
        let binding = [(cte.name, &*working_cte)];
        let context = Subst { ctes: &[], materialized: &binding, storage, txid: outer.txid, depth: 0 };
        let step_tree = subst_set_tree(recursive_tree, context, arena)?;
        // The recursive term's column types must agree with the non-recursive
        // term's (PostgreSQL unifies them; a mismatch is a loud error).
        let mut step_desc = [ColDesc::new("", 0, 0); MAX_PROJ];
        let stepn = describe_set_body(storage, step_tree, txid, &mut step_desc, arena)?;
        if stepn != ncols {
            return Err(sql_err!(
                sqlstate::SYNTAX_ERROR,
                "each UNION query must have the same number of columns"
            ));
        }
        for c in 0..ncols {
            if step_desc[c].type_oid != column_types[c].0 {
                return Err(sql_err!(
                    sqlstate::DATATYPE_MISMATCH,
                    "recursive query \"{}\" column {} has type {} in non-recursive term but type {} overall",
                    cte.name,
                    c + 1,
                    column_types[c].0,
                    step_desc[c].type_oid
                ));
            }
        }
        let (step_rows, _, _) = materialize_set_body(storage, txid, step_tree, arena, params)?;
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
        working = fresh;
    }

    Ok(&*arena
        .alloc(MaterializedCte { column_names, column_types, rows: all_rows })
        .map_err(|_| arena_full())?)
}

fn subst_select<'a>(
    s: &'a Select<'a>,
    context: Subst<'_, 'a>,
    arena: &'a Arena,
) -> Result<&'a Select<'a>, SqlError> {
    let from = match &s.from {
        Some(f) => Some(subst_from(f, context, arena)?),
        None => None,
    };
    let mut items = [SelectItem::Wildcard; MAX_PROJ];
    if s.items.len() > MAX_PROJ {
        return Err(sql_err!(sqlstate::TOO_MANY_COLUMNS, "select list too wide"));
    }
    for (i, it) in s.items.iter().enumerate() {
        items[i] = match it {
            SelectItem::Wildcard => SelectItem::Wildcard,
            SelectItem::TableWildcard(q) => SelectItem::TableWildcard(q),
            SelectItem::RecordStar(base) => {
                SelectItem::RecordStar(subst_expr(base, context, arena)?)
            }
            SelectItem::Expr { expression, alias } => SelectItem::Expr {
                expression: subst_expr(expression, context, arena)?,
                alias: *alias,
            },
        };
    }
    let items = arena.alloc_slice_copy(&items[..s.items.len()]).map_err(|_| arena_full())?;
    let group_by = subst_expr_slice(s.group_by, context, arena)?;
    // Grouping-set bitmasks index into `group_by`; substitution preserves the
    // column order and count, so they carry over unchanged.
    let grouping_sets = arena.alloc_slice_copy(s.grouping_sets).map_err(|_| arena_full())?;
    let mut order = [OrderBy { expression: &Expr::Null, descending: false, nulls_first: false };
        crate::sql::parser::MAX_LIST];
    if s.order_by.len() > crate::sql::parser::MAX_LIST {
        return Err(sql_err!(sqlstate::TOO_MANY_ARGUMENTS, "ORDER BY list too long"));
    }
    for (i, ob) in s.order_by.iter().enumerate() {
        order[i] = OrderBy { expression: subst_expr(ob.expression, context, arena)?, ..*ob };
    }
    let order_by = arena.alloc_slice_copy(&order[..s.order_by.len()]).map_err(|_| arena_full())?;
    let set_body = match s.set_body {
        Some(tree) => Some(subst_set_tree(tree, context, arena)?),
        None => None,
    };
    let new = Select {
        items,
        distinct: s.distinct,
        distinct_on: s.distinct_on,
        from,
        where_clause: opt_subst(s.where_clause, context, arena)?,
        group_by,
        grouping_sets,
        having: opt_subst(s.having, context, arena)?,
        order_by,
        limit: opt_subst(s.limit, context, arena)?,
        offset: opt_subst(s.offset, context, arena)?,
        with: &[],
        set_body,
    };
    Ok(&*arena.alloc(new).map_err(|_| arena_full())?)
}

/// Substitutes parameters through every leaf SELECT of a set-operation tree,
/// mirroring [`subst_select`] for a set-operator subquery body.
fn subst_set_tree<'a>(
    tree: &'a SetTree<'a>,
    context: Subst<'_, 'a>,
    arena: &'a Arena,
) -> Result<&'a SetTree<'a>, SqlError> {
    let out = match tree {
        SetTree::Select(s) => SetTree::Select(subst_select(s, context, arena)?),
        SetTree::Op { operator, all, left, right } => SetTree::Op {
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
    context: Subst<'_, 'a>,
    arena: &'a Arena,
) -> Result<FromClause<'a>, SqlError> {
    let base = subst_tableref(&f.base, context, arena)?;
    let dummy =
        Join { table: f.base, kind: JoinKind::Inner, on: None, using_columns: None, natural: false };
    let mut joins = [dummy; MAX_JOIN_TABLES - 1];
    if f.joins.len() > joins.len() {
        return Err(sql_err!(sqlstate::TOO_MANY_ARGUMENTS, "too many joins"));
    }
    for (i, j) in f.joins.iter().enumerate() {
        joins[i] = Join {
            table: subst_tableref(&j.table, context, arena)?,
            kind: j.kind,
            on: opt_subst(j.on, context, arena)?,
            using_columns: j.using_columns,
            natural: j.natural,
        };
    }
    let joins = arena.alloc_slice_copy(&joins[..f.joins.len()]).map_err(|_| arena_full())?;
    Ok(FromClause { base, joins })
}

fn subst_tableref<'a>(
    t: &TableRef<'a>,
    context: Subst<'_, 'a>,
    arena: &'a Arena,
) -> Result<TableRef<'a>, SqlError> {
    if let Some(sub) = t.subquery {
        return Ok(TableRef {
            subquery: Some(subst_select(sub, context, arena)?),
            ..*t
        });
    }
    // An unqualified name matching a materialized (recursive) CTE resolves to
    // its precomputed row set.
    if t.schema.is_none()
        && t.func_args.is_none()
        && let Some((_, m)) = context.materialized.iter().find(|(name, _)| *name == t.table)
    {
        return Ok(TableRef {
            schema: None,
            table: t.table,
            alias: Some(t.alias.unwrap_or(t.table)),
            subquery: None,
            func_args: None,
            col_alias: t.col_alias,
            cte: Some(m),
            with_ordinality: false,
        });
    }
    // An unqualified name matching a CTE becomes a derived table over the
    // (already-substituted) CTE query, exposed under its alias or CTE name.
    // The CTE's own column-rename list applies unless the reference carries an
    // explicit one (`FROM t AS x(c1, ...)`).
    if t.schema.is_none()
        && let Some((_, q, columns)) = context.ctes.iter().find(|(name, _, _)| *name == t.table)
    {
        let renames = t
            .col_alias
            .or(if columns.is_empty() { None } else { Some(columns) });
        return Ok(TableRef {
            schema: None,
            table: "",
            alias: Some(t.alias.unwrap_or(t.table)),
            subquery: Some(q),
            func_args: None,
            col_alias: renames,
            cte: None,
            with_ordinality: false,
        });
    }
    // A name matching a view (and not shadowed by a CTE or table) expands to a
    // derived table over the view's stored SELECT, recursively expanded.
    if t.schema.is_none()
        && context.storage.find_table(t.table).is_none()
        && let Some(view_sql) = context.storage.find_view(t.table, context.txid)
    {
        if context.depth >= MAX_VIEW_DEPTH {
            return Err(sql_err!(
                sqlstate::FEATURE_NOT_SUPPORTED,
                "view \"{}\" nests too deeply (or references itself)",
                t.table
            ));
        }
        let vsel = crate::sql::parser::parse_view_select(view_sql, arena)?;
        // The view body has its own scope: no outer CTEs, deeper view depth.
        let inner = Subst {
            ctes: &[],
            materialized: &[],
            storage: context.storage,
            txid: context.txid,
            depth: context.depth + 1,
        };
        let expanded = subst_select(vsel, inner, arena)?;
        return Ok(TableRef {
            schema: None,
            table: "",
            alias: Some(t.alias.unwrap_or(t.table)),
            subquery: Some(expanded),
            func_args: None,
            col_alias: None,
            cte: None,
            with_ordinality: false,
        });
    }
    Ok(*t)
}

fn opt_subst<'a>(
    e: Option<&'a Expr<'a>>,
    context: Subst<'_, 'a>,
    arena: &'a Arena,
) -> Result<Option<&'a Expr<'a>>, SqlError> {
    match e {
        Some(x) => Ok(Some(subst_expr(x, context, arena)?)),
        None => Ok(None),
    }
}

fn subst_expr_slice<'a>(
    xs: &'a [&'a Expr<'a>],
    context: Subst<'_, 'a>,
    arena: &'a Arena,
) -> Result<&'a [&'a Expr<'a>], SqlError> {
    if !xs.iter().any(|x| expr_has_subquery(x)) {
        return Ok(xs);
    }
    let mut tmp = [&Expr::Null; crate::sql::parser::MAX_LIST];
    if xs.len() > tmp.len() {
        return Err(sql_err!(sqlstate::TOO_MANY_ARGUMENTS, "expression list too long"));
    }
    for (i, x) in xs.iter().enumerate() {
        tmp[i] = subst_expr(x, context, arena)?;
    }
    Ok(&*arena.alloc_slice_copy(&tmp[..xs.len()]).map_err(|_| arena_full())?)
}

/// True if `e` contains a subquery anywhere (so it needs rebuilding when CTEs
/// are substituted). Leaves and subquery-free trees are returned unchanged.
fn expr_has_subquery(e: &Expr) -> bool {
    match e {
        Expr::Subquery(_) | Expr::InSubquery { .. } | Expr::Exists(_)
        | Expr::ArraySubquery(_) => true,
        Expr::Unary { operand, .. }
        | Expr::Cast { operand, .. }
        | Expr::IsNull { operand, .. } => expr_has_subquery(operand),
        Expr::Binary { left, right, .. } => expr_has_subquery(left) || expr_has_subquery(right),
        Expr::Call { args, order_by, .. } => {
            args.iter().any(|a| expr_has_subquery(a))
                || order_by.iter().any(|o| expr_has_subquery(o.expression))
        }
        Expr::InList { operand, list, .. } => {
            expr_has_subquery(operand) || list.iter().any(|a| expr_has_subquery(a))
        }
        Expr::Between { operand, low, high, .. } => {
            expr_has_subquery(operand) || expr_has_subquery(low) || expr_has_subquery(high)
        }
        Expr::Like { operand, pattern, .. } | Expr::Match { operand, pattern, .. } => {
            expr_has_subquery(operand) || expr_has_subquery(pattern)
        }
        Expr::Case { operand, whens, otherwise, .. } => {
            operand.is_some_and(expr_has_subquery)
                || whens.iter().any(|(c, r)| expr_has_subquery(c) || expr_has_subquery(r))
                || otherwise.is_some_and(expr_has_subquery)
        }
        _ => false,
    }
}

fn subst_expr<'a>(
    e: &'a Expr<'a>,
    context: Subst<'_, 'a>,
    arena: &'a Arena,
) -> Result<&'a Expr<'a>, SqlError> {
    if !expr_has_subquery(e) {
        return Ok(e);
    }
    let rebuilt = match e {
        Expr::Subquery(s) => Expr::Subquery(subst_select(s, context, arena)?),
        Expr::ArraySubquery(s) => Expr::ArraySubquery(subst_select(s, context, arena)?),
        Expr::Exists(s) => Expr::Exists(subst_select(s, context, arena)?),
        Expr::InSubquery { operand, select, negated } => Expr::InSubquery {
            operand: subst_expr(operand, context, arena)?,
            select: subst_select(select, context, arena)?,
            negated: *negated,
        },
        Expr::Unary { operator, operand } => Expr::Unary {
            operator: *operator,
            operand: subst_expr(operand, context, arena)?,
        },
        Expr::Binary { operator, left, right } => Expr::Binary {
            operator: *operator,
            left: subst_expr(left, context, arena)?,
            right: subst_expr(right, context, arena)?,
        },
        Expr::Cast { operand, type_name, type_mod } => Expr::Cast {
            operand: subst_expr(operand, context, arena)?,
            type_name,
            type_mod: *type_mod,
        },
        Expr::IsNull { operand, negated } => Expr::IsNull {
            operand: subst_expr(operand, context, arena)?,
            negated: *negated,
        },
        Expr::Call { name, args, star, distinct, order_by, over, filter } => {
            let mut ob = [OrderBy { expression: &Expr::Null, descending: false, nulls_first: false };
                crate::sql::parser::MAX_LIST];
            if order_by.len() > ob.len() {
                return Err(sql_err!(sqlstate::TOO_MANY_ARGUMENTS, "aggregate ORDER BY list too long"));
            }
            for (i, o) in order_by.iter().enumerate() {
                ob[i] = OrderBy { expression: subst_expr(o.expression, context, arena)?, ..*o };
            }
            let order_by = arena
                .alloc_slice_copy(&ob[..order_by.len()])
                .map_err(|_| arena_full())?;
            let over = match over {
                None => None,
                Some(w) => {
                    let mut ob2 = [OrderBy { expression: &Expr::Null, descending: false, nulls_first: false };
                        crate::sql::parser::MAX_LIST];
                    for (i, o) in w.order_by.iter().enumerate() {
                        ob2[i] = OrderBy { expression: subst_expr(o.expression, context, arena)?, ..*o };
                    }
                    let spec = crate::sql::ast::WindowSpec {
                        partition_by: subst_expr_slice(w.partition_by, context, arena)?,
                        order_by: arena.alloc_slice_copy(&ob2[..w.order_by.len()]).map_err(|_| arena_full())?,
                        frame: w.frame,
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
                star: *star,
                distinct: *distinct,
                order_by,
                over,
                filter,
            }
        }
        Expr::InList { operand, list, negated } => Expr::InList {
            operand: subst_expr(operand, context, arena)?,
            list: subst_expr_slice(list, context, arena)?,
            negated: *negated,
        },
        Expr::Between { operand, low, high, negated } => Expr::Between {
            operand: subst_expr(operand, context, arena)?,
            low: subst_expr(low, context, arena)?,
            high: subst_expr(high, context, arena)?,
            negated: *negated,
        },
        Expr::Like { operand, pattern, negated, case_insensitive, escape } => Expr::Like {
            operand: subst_expr(operand, context, arena)?,
            pattern: subst_expr(pattern, context, arena)?,
            negated: *negated,
            case_insensitive: *case_insensitive,
            escape: opt_subst(*escape, context, arena)?,
        },
        Expr::Match { operand, pattern, negated, case_insensitive } => Expr::Match {
            operand: subst_expr(operand, context, arena)?,
            pattern: subst_expr(pattern, context, arena)?,
            negated: *negated,
            case_insensitive: *case_insensitive,
        },
        Expr::Case { operand, whens, otherwise, synthetic } => {
            let operand = opt_subst(*operand, context, arena)?;
            let mut ws = [(&Expr::Null, &Expr::Null); crate::sql::parser::MAX_LIST];
            if whens.len() > ws.len() {
                return Err(sql_err!(sqlstate::TOO_MANY_ARGUMENTS, "CASE has too many WHEN branches"));
            }
            for (i, (c, r)) in whens.iter().enumerate() {
                ws[i] = (subst_expr(c, context, arena)?, subst_expr(r, context, arena)?);
            }
            let whens = arena.alloc_slice_copy(&ws[..whens.len()]).map_err(|_| arena_full())?;
            Expr::Case {
                operand,
                whens,
                otherwise: opt_subst(*otherwise, context, arena)?,
                synthetic: *synthetic,
            }
        }
        // Leaves never reach here (guarded by expr_has_subquery above).
        other => *other,
    };
    Ok(&*arena.alloc(rebuilt).map_err(|_| arena_full())?)
}
