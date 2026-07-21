//! Set-operation queries: UNION / INTERSECT / EXCEPT.
//!
//! Each SELECT leaf is materialized to self-describing encoded rows coerced to
//! the columns' common type; the operators combine those multisets by sorted
//! merge; then a trailing ORDER BY / LIMIT / OFFSET applies to the whole result.
//! `describe_set_body` and `materialize_set_body` are the shared entry points
//! the derived-table, subquery, and INSERT-source paths reuse.

use crate::mem::arena::Arena;
use crate::pg::respond::Responder;
use crate::pg::wire::WireFull;
use crate::sql::ast::{Expr, OrderBy, Select, SelectItem, SetOp, SetQuery, SetTree};
use crate::sql::eval::{compare_datums, sqlstate, SqlError};
use crate::sql::exec::{self, MAX_PROJ};
use crate::sql::types::{ColDesc, ColType, Datum};
use crate::storage::Storage;
use crate::{sql_err, stack_format};

use super::{
    arena_full, describe_scope_items, expand_set_tree_exec, infer_scope_type, select_into_rows,
    sql_fail, sql_ok, Outcome, QueryScope,
};

const MAX_SET_LEAVES: usize = 32;

/// Executes a set-operation query (UNION / INTERSECT / EXCEPT). Each SELECT
/// leaf is materialized to self-describing rows coerced to the columns' common
/// type; the operators combine those multisets; then the trailing ORDER BY /
/// LIMIT / OFFSET apply to the whole result. Grouped/DISTINCT/aggregate leaves
/// are rejected loudly (they flow through `select_into_rows`).
pub fn set_query<'a>(
    storage: &'a Storage,
    txid: u32,
    q: &'a SetQuery<'a>,
    arena: &'a Arena,
    params: &[Datum<'a>],
    responder: &mut Responder,
) -> Outcome {
    // WITH CTEs and view references expand across the whole tree first.
    let body = match expand_set_tree_exec(q.with, q.body, storage, txid, arena, params) {
        Ok(b) => b,
        Err(e) => return sql_fail(e),
    };
    // Column names + types from the first leaf, unified across every leaf.
    let mut columns = [ColDesc::new("", 0, 0); MAX_PROJ];
    let n_cols = match describe_set_body(storage, body, txid, &mut columns, arena) {
        Ok(n) => n,
        Err(e) => return sql_fail(e),
    };
    let mut target = [ColType::Bool; MAX_PROJ];
    for (c, col) in columns[..n_cols].iter().enumerate() {
        target[c] = exec::coltype_of_oid(col.type_oid).unwrap_or(ColType::Text);
    }

    // Materialize and combine the tree.
    let rows = match eval_set_tree(body, storage, txid, arena, params, &target[..n_cols]) {
        Ok(r) => r,
        Err(e) => return sql_fail(e),
    };

    // ORDER BY (by output column position or name), then LIMIT/OFFSET.
    if let Err(e) = sort_set_rows(rows, q.order_by, &columns[..n_cols]) {
        return sql_fail(e);
    }
    let limit = match exec::eval_limit_pub(q.limit, arena, params) {
        Ok(l) => l,
        Err(e) => return sql_fail(e),
    };
    let offset = match exec::eval_offset_pub(q.offset, arena, params) {
        Ok(o) => o,
        Err(e) => return sql_fail(e),
    };

    responder.row_description(&columns[..n_cols])?;
    let mut emitted = 0u64;
    for (i, row) in rows.iter().enumerate() {
        if (i as u64) < offset {
            continue;
        }
        if emitted >= limit {
            break;
        }
        let mut out = [Datum::Null; MAX_PROJ];
        for (c, slot) in out[..n_cols].iter_mut().enumerate() {
            *slot = exec::decode_projected_pub(row, c);
        }
        if responder.data_row(&out[..n_cols]).is_err() {
            return Err(WireFull);
        }
        emitted += 1;
    }
    let tag = stack_format!(48, "SELECT {}", emitted);
    responder.command_complete(tag.as_str())?;
    sql_ok()
}

/// Walks a set tree collecting its SELECT leaves left-to-right.
fn collect_set_leaves<'a>(
    tree: &'a SetTree<'a>,
    out: &mut [Option<&'a Select<'a>>; MAX_SET_LEAVES],
    n: &mut usize,
) -> Result<(), SqlError> {
    match tree {
        SetTree::Select(s) => {
            if *n == MAX_SET_LEAVES {
                return Err(sql_err!("54000", "too many set-operation branches"));
            }
            out[*n] = Some(s);
            *n += 1;
            Ok(())
        }
        SetTree::Op { left, right, .. } => {
            collect_set_leaves(left, out, n)?;
            collect_set_leaves(right, out, n)
        }
    }
}

/// Column descriptions of a set-operation leaf (FROM-less or table-backed).
/// Whether a set-operation leaf's `c`-th output column is an untyped UNKNOWN
/// (a bare NULL or parameter), which the describe path coerces to text but a
/// set operation should let adopt another branch's type.
fn leaf_col_unknown<'a>(
    storage: &'a Storage,
    s: &'a Select<'a>,
    c: usize,
    txid: u32,
    arena: &'a Arena,
) -> bool {
    if s.set_body.is_some() {
        return false;
    }
    // Find the c-th expression item (wildcards expand to typed columns, never
    // unknown, so they only advance the index).
    let mut idx = 0usize;
    for item in s.items {
        match item {
            SelectItem::Expr { expression, .. } => {
                if idx == c {
                    let raw = match &s.from {
                        None => exec::infer_type_pub(expression, None).map(|t| t.0),
                        Some(f) => QueryScope::resolve_schema(storage, f, txid, arena)
                            .and_then(|sc| infer_scope_type(expression, &sc).map(|t| t.0)),
                    };
                    // infer_scope_type already coerces UNKNOWN→TEXT, so only the
                    // FROM-less path (raw infer) can report UNKNOWN.
                    return matches!(raw, Ok(crate::sql::types::oid::UNKNOWN));
                }
                idx += 1;
            }
            SelectItem::Wildcard | SelectItem::TableWildcard(_) | SelectItem::RecordStar(_) => {
                return false
            }
        }
    }
    false
}

fn describe_leaf<'a>(
    storage: &'a Storage,
    s: &'a Select<'a>,
    txid: u32,
    columns: &mut [ColDesc<'a>],
    arena: &'a Arena,
) -> Result<usize, SqlError> {
    match &s.from {
        None => exec::describe_items(s.items, None, columns),
        Some(from) => {
            let scope = QueryScope::resolve_schema(storage, from, txid, arena)?;
            describe_scope_items(s.items, &scope, columns)
        }
    }
}

/// The common type of two set-operation columns: equal types, the numeric
/// tower, or (else) an error signalled by None.
fn unify_set_type(a: ColType, b: ColType) -> Option<ColType> {
    if a == b {
        return Some(a);
    }
    let numeric = |t| matches!(t, ColType::Int4 | ColType::Int8 | ColType::Float8 | ColType::Numeric);
    if numeric(a) && numeric(b) {
        return Some(exec::unify_numeric_tower(a, b));
    }
    None
}

/// Materializes a set tree to self-describing rows, coercing every leaf's rows
/// to the columns' common `target` types so the combining operators can match
/// rows by their encoded bytes.
fn eval_set_tree<'a>(
    tree: &'a SetTree<'a>,
    storage: &'a Storage,
    txid: u32,
    arena: &'a Arena,
    params: &[Datum<'a>],
    target: &[ColType],
) -> Result<&'a mut [&'a [u8]], SqlError> {
    match tree {
        SetTree::Select(s) => eval_set_leaf(s, storage, txid, arena, params, target),
        SetTree::Op { operator, all, left, right } => {
            let l = eval_set_tree(left, storage, txid, arena, params, target)?;
            let r = eval_set_tree(right, storage, txid, arena, params, target)?;
            combine_sets(*operator, *all, l, r, arena)
        }
    }
}

/// Describes a set-operation body: column names/types come from the first leaf,
/// then each column's type is unified across every leaf (same count required).
/// On success `columns[..n]` carries the final unified OIDs/lengths. Shared by the
/// derived-table, subquery, and INSERT-source paths.
pub(crate) fn describe_set_body<'a>(
    storage: &'a Storage,
    tree: &'a SetTree<'a>,
    txid: u32,
    columns: &mut [ColDesc<'a>],
    arena: &'a Arena,
) -> Result<usize, SqlError> {
    let mut leaves: [Option<&Select>; MAX_SET_LEAVES] = [None; MAX_SET_LEAVES];
    let mut n_leaves = 0;
    collect_set_leaves(tree, &mut leaves, &mut n_leaves)?;
    let n_cols = describe_leaf(storage, leaves[0].expect(">=1 leaf"), txid, columns, arena)?;
    // `None` = still undetermined (an untyped NULL / UNKNOWN column adopts the
    // type of the other branches, as PostgreSQL resolves an unknown literal).
    let mut target: [Option<ColType>; MAX_PROJ] = [None; MAX_PROJ];
    let leaf0 = leaves[0].expect(">=1 leaf");
    for (c, col) in columns[..n_cols].iter().enumerate() {
        target[c] = if leaf_col_unknown(storage, leaf0, c, txid, arena) {
            None
        } else {
            exec::coltype_of_oid(col.type_oid)
        };
    }
    for leaf in leaves[1..n_leaves].iter() {
        let mut lc = [ColDesc::new("", 0, 0); MAX_PROJ];
        let ln = describe_leaf(storage, leaf.expect("leaf"), txid, &mut lc, arena)?;
        if ln != n_cols {
            return Err(sql_err!(
                "42601",
                "each UNION query must have the same number of columns"
            ));
        }
        let leaf_ref = leaf.expect("leaf");
        for c in 0..n_cols {
            if leaf_col_unknown(storage, leaf_ref, c, txid, arena) {
                continue; // an untyped NULL column adopts the running type
            }
            let lt = exec::coltype_of_oid(lc[c].type_oid).unwrap_or(ColType::Text);
            match target[c] {
                None => target[c] = Some(lt),
                Some(existing) => match unify_set_type(existing, lt) {
                    Some(t) => target[c] = Some(t),
                    None => {
                        return Err(sql_err!(
                            "42804",
                            "UNION types {} and {} cannot be matched",
                            existing.name(),
                            lt.name()
                        ))
                    }
                },
            }
        }
    }
    // A column that stayed unknown across every branch (all NULL) is text.
    let target: [ColType; MAX_PROJ] = core::array::from_fn(|c| target[c].unwrap_or(ColType::Text));
    for (c, col) in columns[..n_cols].iter_mut().enumerate() {
        col.type_oid = target[c].oid();
        col.typlen = target[c].typlen();
    }
    Ok(n_cols)
}

/// The result of materializing a set-operation body: the combined encoded rows,
/// the unified per-column types, and the column count.
type MaterializedSet<'a> = (&'a [&'a [u8]], &'a [ColType], usize);

/// Materializes a set-operation body to combined encoded rows plus the unified
/// column types, ready to decode. Shared by subquery and INSERT-source paths.
pub(crate) fn materialize_set_body<'a>(
    storage: &'a Storage,
    txid: u32,
    tree: &'a SetTree<'a>,
    arena: &'a Arena,
    params: &[Datum<'a>],
) -> Result<MaterializedSet<'a>, SqlError> {
    let mut columns = [ColDesc::new("", 0, 0); MAX_PROJ];
    let n = describe_set_body(storage, tree, txid, &mut columns, arena)?;
    let mut tgt = [ColType::Bool; MAX_PROJ];
    for c in 0..n {
        tgt[c] = exec::coltype_of_oid(columns[c].type_oid).unwrap_or(ColType::Text);
    }
    let target = arena.alloc_slice_copy(&tgt[..n]).map_err(|_| arena_full())?;
    let rows = eval_set_tree(tree, storage, txid, arena, params, target)?;
    Ok((rows, target, n))
}

fn eval_set_leaf<'a>(
    s: &'a Select<'a>,
    storage: &'a Storage,
    txid: u32,
    arena: &'a Arena,
    params: &[Datum<'a>],
    target: &[ColType],
) -> Result<&'a mut [&'a [u8]], SqlError> {
    // Pass 1: count the rows. Pass 2: coerce to the target types and encode.
    let mut count = 0usize;
    select_into_rows(storage, txid, s, arena, params, None, &mut |_| {
        count += 1;
        Ok(())
    })?;
    let empty: &[u8] = &[];
    let rows = arena.alloc_slice_with(count, |_| empty).map_err(|_| arena_full())?;
    let n = target.len();
    let mut at = 0usize;
    select_into_rows(storage, txid, s, arena, params, None, &mut |vals| {
        if vals.len() != n {
            return Err(sql_err!(
                "42601",
                "each UNION query must have the same number of columns"
            ));
        }
        let mut coerced = [Datum::Null; MAX_PROJ];
        for c in 0..n {
            coerced[c] = crate::sql::eval::cast_to(vals[c], target[c], arena)?;
        }
        rows[at] = exec::encode_projected_pub(&coerced[..n], arena)?;
        at += 1;
        Ok(())
    })?;
    Ok(rows)
}

/// Combines two encoded-row multisets. Both inputs are sorted here (set ops are
/// unordered until the final ORDER BY), then merged by equal runs.
fn combine_sets<'a>(
    operator: SetOp,
    all: bool,
    l: &'a mut [&'a [u8]],
    r: &'a mut [&'a [u8]],
    arena: &'a Arena,
) -> Result<&'a mut [&'a [u8]], SqlError> {
    // UNION ALL preserves order (left rows then right, as scanned); only the
    // distinct set operations sort to merge/dedup.
    if !(operator == SetOp::Union && all) {
        l.sort_unstable();
        r.sort_unstable();
    }
    let empty: &[u8] = &[];
    let out = arena
        .alloc_slice_with(l.len() + r.len(), |_| empty)
        .map_err(|_| arena_full())?;
    let mut n = 0usize;
    let mut push = |row: &'a [u8], times: usize| {
        for _ in 0..times {
            out[n] = row;
            n += 1;
        }
    };
    match operator {
        SetOp::Union if all => {
            for &row in l.iter().chain(r.iter()) {
                push(row, 1);
            }
        }
        SetOp::Union => {
            // Distinct merge of two sorted runs.
            let (mut i, mut j) = (0, 0);
            let mut last: Option<&[u8]> = None;
            while i < l.len() || j < r.len() {
                let take_l = j >= r.len() || (i < l.len() && l[i] <= r[j]);
                let row = if take_l {
                    i += 1;
                    l[i - 1]
                } else {
                    j += 1;
                    r[j - 1]
                };
                if last != Some(row) {
                    push(row, 1);
                    last = Some(row);
                }
            }
        }
        SetOp::Intersect | SetOp::Except => {
            let (mut i, mut j) = (0, 0);
            while i < l.len() {
                // One equal run in l.
                let row = l[i];
                let mut cl = 0;
                while i < l.len() && l[i] == row {
                    cl += 1;
                    i += 1;
                }
                // Advance r past smaller values, then count the matching run.
                while j < r.len() && r[j] < row {
                    j += 1;
                }
                let mut chained_row = 0;
                while j < r.len() && r[j] == row {
                    chained_row += 1;
                    j += 1;
                }
                let times = match (operator, all) {
                    (SetOp::Intersect, true) => cl.min(chained_row),
                    (SetOp::Intersect, false) => usize::from(chained_row > 0),
                    (SetOp::Except, true) => cl.saturating_sub(chained_row),
                    (SetOp::Except, false) => usize::from(chained_row == 0),
                    _ => unreachable!(),
                };
                push(row, times);
            }
        }
    }
    Ok(&mut out[..n])
}

/// Sorts combined set-operation rows by the trailing ORDER BY, which may
/// reference an output column by 1-based position or by name (from the first
/// leaf). Other ORDER BY expressions over a set operation are unsupported.
fn sort_set_rows(
    rows: &mut [&[u8]],
    order_by: &[OrderBy],
    columns: &[ColDesc],
) -> Result<(), SqlError> {
    if order_by.is_empty() {
        return Ok(());
    }
    // Resolve each key to an output column index.
    let mut keys: [(usize, bool, bool); MAX_PROJ] = [(0, false, false); MAX_PROJ];
    let mut nk = 0;
    for ob in order_by {
        let index = match ob.expression {
            Expr::Int(n) if *n >= 1 && (*n as usize) <= columns.len() => (*n as usize) - 1,
            Expr::Column { name, qualifier: None } => {
                match columns.iter().position(|c| c.name == *name) {
                    Some(i) => i,
                    None => {
                        return Err(sql_err!(
                            sqlstate::UNDEFINED_COLUMN,
                            "ORDER BY column \"{}\" does not exist in the set-operation result",
                            name
                        ))
                    }
                }
            }
            _ => {
                return Err(sql_err!(
                    sqlstate::FEATURE_NOT_SUPPORTED,
                    "ORDER BY on a set operation must name an output column or its position"
                ))
            }
        };
        keys[nk] = (index, ob.descending, ob.nulls_first);
        nk += 1;
    }
    let keys = &keys[..nk];
    let mut err: Option<SqlError> = None;
    rows.sort_by(|a, b| {
        if err.is_some() {
            return core::cmp::Ordering::Equal;
        }
        for &(index, descending, nulls_first) in keys {
            let va = exec::decode_projected_pub(a, index);
            let vb = exec::decode_projected_pub(b, index);
            let ord = match (va.is_null(), vb.is_null()) {
                (true, true) => core::cmp::Ordering::Equal,
                (true, false) => if nulls_first { core::cmp::Ordering::Less } else { core::cmp::Ordering::Greater },
                (false, true) => if nulls_first { core::cmp::Ordering::Greater } else { core::cmp::Ordering::Less },
                (false, false) => match compare_datums(&va, &vb) {
                    Ok(o) => if descending { o.reverse() } else { o },
                    Err(e) => {
                        err = Some(e);
                        core::cmp::Ordering::Equal
                    }
                },
            };
            if ord != core::cmp::Ordering::Equal {
                return ord;
            }
        }
        core::cmp::Ordering::Equal
    });
    match err {
        Some(e) => Err(e),
        None => Ok(()),
    }
}
