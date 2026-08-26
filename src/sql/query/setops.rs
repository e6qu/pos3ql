//! Set-operation queries: UNION / INTERSECT / EXCEPT.
//!
//! Each SELECT leaf is materialized to self-describing encoded rows coerced to
//! the columns' common type; the operators combine those multisets by sorted
//! merge; then a trailing ORDER BY / LIMIT / OFFSET applies to the whole result.
//! `describe_set_body` and `materialize_set_body` are the shared entry points
//! the derived-table, subquery, and INSERT-source paths reuse.

use crate::mem::arena::Arena;
use crate::pg::respond::Responder;
use crate::sql::ast::{Collation, Expr, OrderBy, Select, SelectItem, SetOp, SetQuery, SetTree};
use crate::sql::eval::{SequenceAccess, SqlError, compare_datums_collated, sqlstate};
use crate::sql::exec::{self, MAX_PROJ};
use crate::sql::external::ExternalRun;
use crate::sql::types::{ColDesc, ColType, CollationDerivation, Datum};
use crate::storage::Storage;
use crate::{sql_err, stack_format};

use super::{
    Outcome, QueryScope, arena_full, describe_scope_items, expand_set_tree_exec, infer_scope_type,
    select_into_rows, select_into_rows_recycling, sql_fail, sql_ok,
};

const MAX_SET_LEAVES: usize = 32;
const MAX_SET_NODES: usize = MAX_SET_LEAVES * 2 - 1;

struct DrySequence<'a>(&'a dyn SequenceAccess);

#[derive(Clone, Copy)]
struct ExternalSetContext<'a> {
    storage: &'a Storage,
    txid: u32,
    arena: &'a Arena,
    params: &'a [Datum<'a>],
    sequences: Option<&'a dyn SequenceAccess>,
}

impl SequenceAccess for DrySequence<'_> {
    fn nextval(&self, name: &str) -> Result<i64, SqlError> {
        self.0.dry_nextval(name)
    }
    fn currval(&self, name: &str) -> Result<i64, SqlError> {
        self.0.dry_currval(name)
    }
    fn lastval(&self) -> Result<i64, SqlError> {
        self.0.dry_lastval()
    }
    fn setval(&self, name: &str, value: i64, is_called: bool) -> Result<i64, SqlError> {
        self.0.dry_setval(name, value, is_called)
    }
    fn dry_nextval(&self, name: &str) -> Result<i64, SqlError> {
        self.0.dry_nextval(name)
    }
    fn dry_currval(&self, name: &str) -> Result<i64, SqlError> {
        self.0.dry_currval(name)
    }
    fn dry_lastval(&self) -> Result<i64, SqlError> {
        self.0.dry_lastval()
    }
    fn dry_setval(&self, name: &str, value: i64, is_called: bool) -> Result<i64, SqlError> {
        self.0.dry_setval(name, value, is_called)
    }
}

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
    sequences: Option<&dyn SequenceAccess>,
    responder: &mut Responder,
) -> Outcome {
    // A row-locking clause can never apply to a set operation, matching
    // PostgreSQL's 0A000.
    if let Some(clause) = q.locking.first() {
        return sql_fail(sql_err!(
            crate::sql::eval::sqlstate::FEATURE_NOT_SUPPORTED,
            "{} is not allowed with UNION/INTERSECT/EXCEPT",
            clause.strength.keyword()
        ));
    }
    // WITH CTEs and view references expand across the whole tree first.
    let body = match expand_set_tree_exec(q.with, q.body, storage, txid, arena, params, sequences) {
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
        let Some((ctype, _)) = exec::catalog_column_type(storage, txid, col.type_oid) else {
            return sql_fail(sql_err!(
                crate::sql::eval::sqlstate::FEATURE_NOT_SUPPORTED,
                "set-operation column {} type (oid {}) is not supported",
                c + 1,
                col.type_oid
            ));
        };
        target[c] = ctype;
    }

    if storage.spill_attached() {
        return external_set_query(
            storage,
            txid,
            body,
            q,
            arena,
            params,
            sequences,
            &target[..n_cols],
            &columns[..n_cols],
            responder,
        );
    }

    // Materialize and combine the tree.
    let collations: [Collation; MAX_PROJ] = core::array::from_fn(|index| columns[index].collation);
    let rows = match eval_set_tree(
        body,
        storage,
        txid,
        arena,
        params,
        sequences,
        &target[..n_cols],
        &collations[..n_cols],
    ) {
        Ok(r) => r,
        Err(e) => return sql_fail(e),
    };

    // ORDER BY (by output column position or name), then LIMIT/OFFSET.
    if let Err(e) = sort_set_rows(storage, arena, rows, q.order_by, &columns[..n_cols]) {
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
    let start = (offset as usize).min(rows.len());
    let mut end = offset.saturating_add(limit).min(rows.len() as u64) as usize;
    // FETCH FIRST ... WITH TIES: extend past the limit while rows tie with the
    // last on the ORDER BY output columns (a set-operation ORDER BY names an
    // output column, so ties compare those columns directly).
    if q.with_ties && limit > 0 && end < rows.len() && end > start {
        let boundary = rows[end - 1];
        while end < rows.len() {
            match set_rows_tie(storage, boundary, rows[end], q.order_by, &columns[..n_cols]) {
                Ok(true) => end += 1,
                Ok(false) => break,
                Err(error) => return sql_fail(error),
            }
        }
    }
    let mut emitted = 0u64;
    for row in &rows[start..end] {
        let mut out = [Datum::Null; MAX_PROJ];
        for (c, slot) in out[..n_cols].iter_mut().enumerate() {
            *slot = match exec::decode_projected_col_record(row, c, arena) {
                Ok(value) => value,
                Err(error) => return sql_fail(error),
            };
        }
        match super::emit_data_row(storage, txid, arena, responder, &out[..n_cols]) {
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

/// Streams a complete set-operation query into an internal consumer. This is
/// the same semantic boundary as wire output, so function programs, derived
/// consumers, and protocol responses cannot drift in their treatment of
/// CTEs, ordering, limits, or composite values.
pub(crate) fn set_query_into_rows<'a>(
    storage: &'a Storage,
    txid: u32,
    query: &'a SetQuery<'a>,
    arena: &'a Arena,
    params: &[Datum<'a>],
    sequences: Option<&dyn SequenceAccess>,
    emit: &mut dyn for<'row> FnMut(&[Datum<'row>]) -> Result<(), SqlError>,
) -> Result<(), SqlError> {
    if let Some(clause) = query.locking.first() {
        return Err(sql_err!(
            crate::sql::eval::sqlstate::FEATURE_NOT_SUPPORTED,
            "{} is not allowed with UNION/INTERSECT/EXCEPT",
            clause.strength.keyword()
        ));
    }
    let body = expand_set_tree_exec(
        query.with, query.body, storage, txid, arena, params, sequences,
    )?;
    if storage.spill_attached() {
        return external_set_body_into(
            storage,
            txid,
            body,
            query.order_by,
            query.limit,
            query.offset,
            query.with_ties,
            arena,
            params,
            sequences,
            emit,
        );
    }
    let mut columns = [ColDesc::new("", 0, 0); MAX_PROJ];
    let column_count = describe_set_body(storage, body, txid, &mut columns, arena)?;
    let mut target = [ColType::Bool; MAX_PROJ];
    for (column, desc) in columns[..column_count].iter().enumerate() {
        target[column] = exec::catalog_column_type(storage, txid, desc.type_oid)
            .map(|(ctype, _)| ctype)
            .ok_or_else(|| {
                sql_err!(
                    crate::sql::eval::sqlstate::FEATURE_NOT_SUPPORTED,
                    "set-operation column {} type (oid {}) is not supported",
                    column + 1,
                    desc.type_oid
                )
            })?;
    }
    let collations: [Collation; MAX_PROJ] = core::array::from_fn(|index| columns[index].collation);
    let rows = eval_set_tree(
        body,
        storage,
        txid,
        arena,
        params,
        sequences,
        &target[..column_count],
        &collations[..column_count],
    )?;
    sort_set_rows(
        storage,
        arena,
        rows,
        query.order_by,
        &columns[..column_count],
    )?;
    let limit = exec::eval_limit_pub(query.limit, arena, params)?;
    let offset = exec::eval_offset_pub(query.offset, arena, params)?;
    let start = (offset as usize).min(rows.len());
    let mut end = offset.saturating_add(limit).min(rows.len() as u64) as usize;
    if query.with_ties && limit > 0 && end < rows.len() && end > start {
        let boundary = rows[end - 1];
        while end < rows.len()
            && set_rows_tie(
                storage,
                boundary,
                rows[end],
                query.order_by,
                &columns[..column_count],
            )?
        {
            end += 1;
        }
    }
    for row in &rows[start..end] {
        let mut values = [Datum::Null; MAX_PROJ];
        for (column, value) in values.iter_mut().enumerate().take(column_count) {
            *value = exec::decode_projected_col_record(row, column, arena)?;
        }
        emit(&values[..column_count])?;
    }
    Ok(())
}

fn byte_order(left: &[u8], right: &[u8]) -> Result<core::cmp::Ordering, SqlError> {
    Ok(left.cmp(right))
}

fn insertion_order(_left: &[u8], _right: &[u8]) -> Result<core::cmp::Ordering, SqlError> {
    Ok(core::cmp::Ordering::Equal)
}

fn resolve_set_order(
    order_by: &[OrderBy],
    columns: &[ColDesc],
    keys: &mut [(usize, bool, bool); MAX_PROJ],
) -> Result<usize, SqlError> {
    let mut count = 0usize;
    for order in order_by {
        let index = match order.expression {
            Expr::Int(position) if *position >= 1 && (*position as usize) <= columns.len() => {
                (*position as usize) - 1
            }
            Expr::Column {
                name,
                qualifier: None,
            } => columns
                .iter()
                .position(|column| column.name == *name)
                .ok_or_else(|| {
                    sql_err!(
                        sqlstate::UNDEFINED_COLUMN,
                        "ORDER BY column \"{}\" does not exist in the set-operation result",
                        name
                    )
                })?,
            _ => {
                return Err(sql_err!(
                    sqlstate::FEATURE_NOT_SUPPORTED,
                    "ORDER BY on a set operation must name an output column or its position"
                ));
            }
        };
        if columns[index].collation_derivation == CollationDerivation::Indeterminate {
            return Err(sql_err!(
                sqlstate::INDETERMINATE_COLLATION,
                "could not determine which collation to use for string comparison"
            ));
        }
        keys[count] = (index, order.descending, order.nulls_first);
        count += 1;
    }
    Ok(count)
}

fn compare_set_order(
    storage: &Storage,
    columns: &[ColDesc],
    left: &[u8],
    right: &[u8],
    keys: &[(usize, bool, bool)],
) -> Result<core::cmp::Ordering, SqlError> {
    for &(index, descending, nulls_first) in keys {
        let left_value = exec::decode_projected_pub(left, index);
        let right_value = exec::decode_projected_pub(right, index);
        let ordering = match (left_value.is_null(), right_value.is_null()) {
            (true, true) => core::cmp::Ordering::Equal,
            (true, false) if nulls_first => core::cmp::Ordering::Less,
            (true, false) => core::cmp::Ordering::Greater,
            (false, true) if nulls_first => core::cmp::Ordering::Greater,
            (false, true) => core::cmp::Ordering::Less,
            (false, false) => {
                let ordering = compare_datums_collated(
                    storage,
                    columns[index].collation,
                    &left_value,
                    &right_value,
                )?;
                if descending {
                    ordering.reverse()
                } else {
                    ordering
                }
            }
        };
        if !ordering.is_eq() {
            return Ok(ordering);
        }
    }
    Ok(core::cmp::Ordering::Equal)
}

fn push_run(
    storage: &Storage,
    reader: &mut crate::sql::external::ExternalRunReader,
    run: ExternalRun,
    sorter: &mut crate::sql::external::ExternalSorter,
    compare: &mut impl FnMut(&[u8], &[u8]) -> Result<core::cmp::Ordering, SqlError>,
) -> Result<(), SqlError> {
    storage
        .with_block_store(|blocks| reader.start(blocks, run))
        .expect("spill-attached block store")?;
    while let Some(row) = reader.row() {
        storage
            .with_block_store(|blocks| sorter.push_encoded(blocks, row, compare))
            .expect("spill-attached block store")?;
        storage
            .with_block_store(|blocks| reader.advance(blocks))
            .expect("spill-attached block store")?;
    }
    Ok(())
}

fn external_set_leaf<'a>(
    select: &'a Select<'a>,
    context: ExternalSetContext<'a>,
    target: &[ColType],
    sorted: bool,
) -> Result<Option<ExternalRun>, SqlError> {
    let mut sorter = context.storage.external_sorter()?;
    sorter.reset();
    let mut compare = if sorted {
        byte_order as fn(&[u8], &[u8]) -> Result<_, _>
    } else {
        insertion_order as fn(&[u8], &[u8]) -> Result<_, _>
    };
    select_into_rows_recycling(
        context.storage,
        context.txid,
        select,
        context.arena,
        context.params,
        None,
        context.sequences,
        &mut |values| {
            if values.len() != target.len() {
                return Err(sql_err!(
                    sqlstate::SYNTAX_ERROR,
                    "each UNION query must have the same number of columns"
                ));
            }
            let mut coerced = [Datum::Null; MAX_PROJ];
            for column in 0..target.len() {
                coerced[column] = coerce_set_value(values[column], target[column], context.arena)?;
            }
            context
                .storage
                .with_block_store(|blocks| {
                    sorter.push_projected_by(
                        blocks,
                        target.len(),
                        |column| coerced[column],
                        &mut compare,
                    )
                })
                .expect("spill-attached block store")
        },
    )?;
    context
        .storage
        .with_block_store(|blocks| sorter.finish(blocks, &mut compare))
        .expect("spill-attached block store")
}

fn external_set_tree<'a>(
    tree: &'a SetTree<'a>,
    context: ExternalSetContext<'a>,
    target: &[ColType],
    sorted: bool,
) -> Result<Option<ExternalRun>, SqlError> {
    let SetTree::Op {
        operator,
        all,
        left,
        right,
    } = tree
    else {
        let SetTree::Select(select) = tree else {
            unreachable!()
        };
        return external_set_leaf(select, context, target, sorted);
    };
    let merge_inputs = !(*operator == SetOp::Union && *all);
    let left_run = external_set_tree(left, context, target, sorted || merge_inputs)?;
    let right_run = external_set_tree(right, context, target, sorted || merge_inputs)?;

    let mut sorter = context.storage.external_sorter()?;
    sorter.reset();
    let mut output_compare = if sorted || merge_inputs {
        byte_order as fn(&[u8], &[u8]) -> Result<_, _>
    } else {
        insertion_order as fn(&[u8], &[u8]) -> Result<_, _>
    };
    if *operator == SetOp::Union && *all {
        let mut reader = context.storage.external_run_reader()?;
        if let Some(run) = left_run {
            push_run(
                context.storage,
                &mut reader,
                run,
                &mut sorter,
                &mut output_compare,
            )?;
        }
        if let Some(run) = right_run {
            push_run(
                context.storage,
                &mut reader,
                run,
                &mut sorter,
                &mut output_compare,
            )?;
        }
    } else if let Some(left_run) = left_run {
        let mut left_reader = context.storage.external_run_reader()?;
        let mut right_reader = context.storage.external_run_reader()?;
        context
            .storage
            .with_block_store(|blocks| left_reader.start(blocks, left_run))
            .expect("spill-attached block store")?;
        if let Some(right_run) = right_run {
            context
                .storage
                .with_block_store(|blocks| right_reader.start(blocks, right_run))
                .expect("spill-attached block store")?;
        }
        while left_reader.row().is_some() {
            let left_length = left_reader.stage_current().expect("current left row");
            let mut left_count = 0u64;
            while left_reader.row() == Some(left_reader.output(left_length)) {
                left_count += 1;
                context
                    .storage
                    .with_block_store(|blocks| left_reader.advance(blocks))
                    .expect("spill-attached block store")?;
            }
            let left_row = left_reader.output(left_length);
            while right_reader.row().is_some_and(|row| row < left_row) {
                let right_length = right_reader.stage_current().expect("current right row");
                if *operator == SetOp::Union {
                    context
                        .storage
                        .with_block_store(|blocks| {
                            sorter.push_encoded(
                                blocks,
                                right_reader.output(right_length),
                                &mut output_compare,
                            )
                        })
                        .expect("spill-attached block store")?;
                }
                while right_reader.row() == Some(right_reader.output(right_length)) {
                    context
                        .storage
                        .with_block_store(|blocks| right_reader.advance(blocks))
                        .expect("spill-attached block store")?;
                }
            }
            let mut right_count = 0u64;
            while right_reader.row() == Some(left_row) {
                right_count += 1;
                context
                    .storage
                    .with_block_store(|blocks| right_reader.advance(blocks))
                    .expect("spill-attached block store")?;
            }
            let copies = match (*operator, *all) {
                (SetOp::Union, false) => 1,
                (SetOp::Intersect, true) => left_count.min(right_count),
                (SetOp::Intersect, false) => u64::from(right_count > 0),
                (SetOp::Except, true) => left_count.saturating_sub(right_count),
                (SetOp::Except, false) => u64::from(right_count == 0),
                _ => unreachable!(),
            };
            for _ in 0..copies {
                context
                    .storage
                    .with_block_store(|blocks| {
                        sorter.push_encoded(blocks, left_row, &mut output_compare)
                    })
                    .expect("spill-attached block store")?;
            }
        }
        if *operator == SetOp::Union && !*all {
            while right_reader.row().is_some() {
                let right_length = right_reader.stage_current().expect("current right row");
                context
                    .storage
                    .with_block_store(|blocks| {
                        sorter.push_encoded(
                            blocks,
                            right_reader.output(right_length),
                            &mut output_compare,
                        )
                    })
                    .expect("spill-attached block store")?;
                while right_reader.row() == Some(right_reader.output(right_length)) {
                    context
                        .storage
                        .with_block_store(|blocks| right_reader.advance(blocks))
                        .expect("spill-attached block store")?;
                }
            }
        }
    } else if *operator == SetOp::Union && !*all {
        let mut reader = context.storage.external_run_reader()?;
        if let Some(run) = right_run {
            context
                .storage
                .with_block_store(|blocks| reader.start(blocks, run))
                .expect("spill-attached block store")?;
            while reader.row().is_some() {
                let right_length = reader.stage_current().expect("current right row");
                context
                    .storage
                    .with_block_store(|blocks| {
                        sorter.push_encoded(
                            blocks,
                            reader.output(right_length),
                            &mut output_compare,
                        )
                    })
                    .expect("spill-attached block store")?;
                while reader.row() == Some(reader.output(right_length)) {
                    context
                        .storage
                        .with_block_store(|blocks| reader.advance(blocks))
                        .expect("spill-attached block store")?;
                }
            }
        }
    }
    context
        .storage
        .with_block_store(|blocks| sorter.finish(blocks, &mut output_compare))
        .expect("spill-attached block store")
}

/// Streams a set-operation body from immutable provider-neutral runs. This is
/// the retention-free row-source seam used by INSERT/CTAS and derived
/// consumers; decoded values borrow only the reader's current row.
#[expect(
    clippy::too_many_arguments,
    reason = "set-operation execution plumbing"
)]
pub(crate) fn external_set_body_into<'a>(
    storage: &'a Storage,
    txid: u32,
    tree: &'a SetTree<'a>,
    order_by: &'a [OrderBy<'a>],
    limit: Option<&'a Expr<'a>>,
    offset: Option<&'a Expr<'a>>,
    with_ties: bool,
    arena: &'a Arena,
    params: &[Datum<'a>],
    sequences: Option<&dyn SequenceAccess>,
    emit: &mut dyn for<'row> FnMut(&[Datum<'row>]) -> Result<(), SqlError>,
) -> Result<(), SqlError> {
    let mut columns = [ColDesc::new("", 0, 0); MAX_PROJ];
    let column_count = describe_set_body(storage, tree, txid, &mut columns, arena)?;
    let mut target = [ColType::Bool; MAX_PROJ];
    for column in 0..column_count {
        target[column] = exec::catalog_column_type(storage, txid, columns[column].type_oid)
            .map(|(ctype, _)| ctype)
            .ok_or_else(|| {
                sql_err!(
                    crate::sql::eval::sqlstate::FEATURE_NOT_SUPPORTED,
                    "set-operation column {} type (oid {}) is not supported",
                    column + 1,
                    columns[column].type_oid
                )
            })?;
    }
    let Some(run) = external_set_tree(
        tree,
        ExternalSetContext {
            storage,
            txid,
            arena,
            params,
            sequences,
        },
        &target[..column_count],
        false,
    )?
    else {
        return Ok(());
    };
    let limit = exec::eval_limit_pub(limit, arena, params)?;
    let offset = exec::eval_offset_pub(offset, arena, params)?;
    // A trailing ORDER BY needs one final provider-neutral sort. Set-operation
    // output without ORDER BY retains the multiset merge order.
    let run = if order_by.is_empty() {
        run
    } else {
        let mut keys = [(0usize, false, false); MAX_PROJ];
        let key_count = resolve_set_order(order_by, &columns[..column_count], &mut keys)?;
        let mut reader = storage.external_run_reader()?;
        let mut sorter = storage.external_sorter()?;
        sorter.reset();
        let mut compare = |left: &[u8], right: &[u8]| {
            compare_set_order(
                storage,
                &columns[..column_count],
                left,
                right,
                &keys[..key_count],
            )
        };
        push_run(storage, &mut reader, run, &mut sorter, &mut compare)?;
        storage
            .with_block_store(|blocks| sorter.finish(blocks, &mut compare))
            .expect("spill-attached block store")?
            .unwrap_or(run)
    };
    let mut reader = storage.external_run_reader()?;
    storage
        .with_block_store(|blocks| reader.start(blocks, run))
        .expect("spill-attached block store")?;
    let window = offset.saturating_add(limit);
    let mut logical = 0u64;
    let mut boundary_len = 0usize;
    while let Some(context) = reader.context() {
        let tied = with_ties
            && limit > 0
            && logical >= window
            && boundary_len > 0
            && set_rows_tie(
                storage,
                &context.boundary[..boundary_len],
                context.row,
                order_by,
                &columns[..column_count],
            )?;
        if logical >= window && !tied {
            break;
        }
        if logical >= offset {
            let mut values = [Datum::Null; MAX_PROJ];
            for (column, value) in values.iter_mut().enumerate().take(column_count) {
                *value = exec::decode_projected_col_record(context.row, column, arena)?;
            }
            emit(&values[..column_count])?;
        }
        if with_ties && limit > 0 && logical + 1 == window {
            context.boundary[..context.row.len()].copy_from_slice(context.row);
            boundary_len = context.row.len();
        }
        logical += 1;
        storage
            .with_block_store(|blocks| reader.advance(blocks))
            .expect("spill-attached block store")?;
    }
    Ok(())
}

#[expect(clippy::too_many_arguments, reason = "set-query execution plumbing")]
fn external_set_query<'a>(
    storage: &'a Storage,
    txid: u32,
    tree: &'a SetTree<'a>,
    query: &'a SetQuery<'a>,
    arena: &'a Arena,
    params: &[Datum<'a>],
    sequences: Option<&dyn SequenceAccess>,
    target: &[ColType],
    columns: &[ColDesc<'a>],
    responder: &mut Responder,
) -> Outcome {
    let run = match external_set_tree(
        tree,
        ExternalSetContext {
            storage,
            txid,
            arena,
            params,
            sequences,
        },
        target,
        false,
    ) {
        Ok(run) => run,
        Err(error) => return sql_fail(error),
    };
    let limit = match exec::eval_limit_pub(query.limit, arena, params) {
        Ok(limit) => limit,
        Err(error) => return sql_fail(error),
    };
    let offset = match exec::eval_offset_pub(query.offset, arena, params) {
        Ok(offset) => offset,
        Err(error) => return sql_fail(error),
    };
    responder.row_description(columns)?;
    let Some(run) = run else {
        responder.command_complete("SELECT 0")?;
        return sql_ok();
    };
    // A trailing ORDER BY needs one final provider-neutral sort. Set-operation
    // output without ORDER BY retains UNION ALL's left-then-right order.
    let run = if query.order_by.is_empty() {
        run
    } else {
        let mut keys = [(0usize, false, false); MAX_PROJ];
        let key_count = match resolve_set_order(query.order_by, columns, &mut keys) {
            Ok(count) => count,
            Err(error) => return sql_fail(error),
        };
        let mut reader = match storage.external_run_reader() {
            Ok(reader) => reader,
            Err(error) => return sql_fail(error),
        };
        let mut sorter = match storage.external_sorter() {
            Ok(sorter) => sorter,
            Err(error) => return sql_fail(error),
        };
        sorter.reset();
        let mut compare = |left: &[u8], right: &[u8]| {
            compare_set_order(storage, columns, left, right, &keys[..key_count])
        };
        if let Err(error) = push_run(storage, &mut reader, run, &mut sorter, &mut compare) {
            return sql_fail(error);
        }
        match storage
            .with_block_store(|blocks| sorter.finish(blocks, &mut compare))
            .expect("spill-attached block store")
        {
            Ok(Some(run)) => run,
            Ok(None) => unreachable!("sorting a non-empty run stays non-empty"),
            Err(error) => return sql_fail(error),
        }
    };
    let mut reader = match storage.external_run_reader() {
        Ok(reader) => reader,
        Err(error) => return sql_fail(error),
    };
    if let Err(error) = storage
        .with_block_store(|blocks| reader.start(blocks, run))
        .expect("spill-attached block store")
    {
        return sql_fail(error);
    }
    let mut logical = 0u64;
    let mut emitted = 0u64;
    let window = offset.saturating_add(limit);
    let mut boundary_len = 0usize;
    while reader.row().is_some() {
        let keep_scanning = {
            let context = reader.context().expect("current external set row");
            let tied = if query.with_ties && limit > 0 && logical >= window && boundary_len > 0 {
                match set_rows_tie(
                    storage,
                    &context.boundary[..boundary_len],
                    context.row,
                    query.order_by,
                    columns,
                ) {
                    Ok(value) => value,
                    Err(error) => return sql_fail(error),
                }
            } else {
                false
            };
            if logical >= window && !tied {
                false
            } else {
                if logical >= offset {
                    let mut values = [Datum::Null; MAX_PROJ];
                    for (column, value) in values.iter_mut().enumerate().take(target.len()) {
                        *value = match exec::decode_projected_col_record(context.row, column, arena)
                        {
                            Ok(value) => value,
                            Err(error) => return sql_fail(error),
                        };
                    }
                    match super::emit_data_row(
                        storage,
                        txid,
                        arena,
                        responder,
                        &values[..target.len()],
                    ) {
                        Ok(Ok(())) => {}
                        Ok(Err(error)) => return sql_fail(error),
                        Err(wire) => return Err(wire),
                    }
                    emitted += 1;
                }
                if query.with_ties && limit > 0 && logical + 1 == window {
                    context.boundary[..context.row.len()].copy_from_slice(context.row);
                    boundary_len = context.row.len();
                }
                logical += 1;
                true
            }
        };
        if !keep_scanning {
            break;
        }
        if let Err(error) = storage
            .with_block_store(|blocks| reader.advance(blocks))
            .expect("spill-attached block store")
        {
            return sql_fail(error);
        }
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
                return Err(sql_err!(
                    sqlstate::PROGRAM_LIMIT_EXCEEDED,
                    "too many set-operation branches"
                ));
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
                    // A cast is a typed boundary even when catalog-free
                    // inference cannot name its user-defined target. Treating
                    // such a cast as UNKNOWN makes identical composite
                    // branches collapse to text.
                    if matches!(expression, Expr::Cast { .. }) {
                        return false;
                    }
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
                return false;
            }
        }
    }
    false
}

/// Column descriptions of a set-operation leaf (FROM-less or table-backed).
fn describe_leaf<'a>(
    storage: &'a Storage,
    s: &'a Select<'a>,
    txid: u32,
    columns: &mut [ColDesc<'a>],
    arena: &'a Arena,
) -> Result<usize, SqlError> {
    match &s.from {
        None => super::describe_catalog_items(s.items, None, storage, txid, columns),
        Some(from) => {
            let scope = QueryScope::resolve_schema(storage, from, txid, arena)?;
            describe_scope_items(s.items, &scope, None, storage, txid, arena, columns)
        }
    }
}

#[derive(Clone, Copy)]
struct SetColumnCollation {
    value: Collation,
    derivation: CollationDerivation,
}

impl SetColumnCollation {
    const NONE: Self = Self {
        value: Collation::None,
        derivation: CollationDerivation::None,
    };
}

fn merge_set_collations(
    left: SetColumnCollation,
    right: SetColumnCollation,
    allow_indeterminate: bool,
) -> Result<SetColumnCollation, SqlError> {
    use CollationDerivation::{Explicit, Implicit, Indeterminate, None};

    let merged = match (left.derivation, right.derivation) {
        (None, _) => right,
        (_, None) => left,
        (Explicit, Explicit) if left.value != right.value => {
            return Err(sql_err!(
                sqlstate::COLLATION_MISMATCH,
                "collation mismatch between \"{}\" and \"{}\"",
                left.value.name(),
                right.value.name()
            ));
        }
        (Explicit, _) => left,
        (_, Explicit) => right,
        (Indeterminate, Indeterminate) => SetColumnCollation {
            value: Collation::None,
            derivation: Indeterminate,
        },
        (Indeterminate, _) => right,
        (_, Indeterminate) => left,
        (Implicit, Implicit)
            if left.value != Collation::Default
                && right.value != Collation::Default
                && left.value != right.value =>
        {
            SetColumnCollation {
                value: Collation::None,
                derivation: Indeterminate,
            }
        }
        (Implicit, Implicit) if left.value == Collation::Default => right,
        (Implicit, Implicit) => left,
    };
    if merged.derivation == Indeterminate && !allow_indeterminate {
        return Err(sql_err!(
            sqlstate::COLLATION_MISMATCH,
            "collation mismatch between implicit collations \"{}\" and \"{}\"",
            left.value.name(),
            right.value.name()
        ));
    }
    Ok(merged)
}

fn describe_set_collations(
    tree: &SetTree<'_>,
    column_count: usize,
    leaves: &[[SetColumnCollation; MAX_PROJ]; MAX_SET_LEAVES],
    next_leaf: &mut usize,
    workspace: &mut [[SetColumnCollation; MAX_PROJ]; MAX_SET_NODES],
    next_node: &mut usize,
) -> Result<usize, SqlError> {
    let output = *next_node;
    *next_node += 1;
    if output == workspace.len() {
        return Err(sql_err!(
            sqlstate::PROGRAM_LIMIT_EXCEEDED,
            "too many set-operation branches"
        ));
    }
    match tree {
        SetTree::Select(_) => {
            workspace[output][..column_count].copy_from_slice(&leaves[*next_leaf][..column_count]);
            *next_leaf += 1;
        }
        SetTree::Op {
            operator,
            all,
            left,
            right,
        } => {
            let left = describe_set_collations(
                left,
                column_count,
                leaves,
                next_leaf,
                workspace,
                next_node,
            )?;
            let right = describe_set_collations(
                right,
                column_count,
                leaves,
                next_leaf,
                workspace,
                next_node,
            )?;
            let allow_indeterminate = *operator == SetOp::Union && *all;
            let left = workspace[left];
            let right = workspace[right];
            for ((result, left), right) in workspace[output][..column_count]
                .iter_mut()
                .zip(left)
                .zip(right)
            {
                *result = merge_set_collations(left, right, allow_indeterminate)?;
                if result.derivation == CollationDerivation::Explicit {
                    result.derivation = CollationDerivation::Implicit;
                }
            }
        }
    }
    Ok(output)
}

/// A set operation keeps the first leaf's output identity. Record output is
/// no exception: carry its registered shape with that description so a derived
/// set source remains a typed composite instead of an anonymous record.
fn register_first_leaf_record_shapes(
    storage: &Storage,
    statement: &Select,
    txid: u32,
    arena: &Arena,
    columns: &mut [ColDesc],
    count: usize,
) -> Result<(), SqlError> {
    let scope = statement
        .from
        .as_ref()
        .map(|from| QueryScope::resolve_schema(storage, from, txid, arena))
        .transpose()?;
    let scope = scope.as_ref();
    let mut slot = 0usize;
    for item in statement.items {
        match item {
            SelectItem::Wildcard => slot += scope.map_or(0, QueryScope::star_columns),
            SelectItem::TableWildcard(name) => {
                slot += scope
                    .and_then(|scope| {
                        scope
                            .table_index(name)
                            .ok()
                            .map(|table| scope.defs[table].expect("resolved").n_columns)
                    })
                    .unwrap_or(0);
            }
            SelectItem::RecordStar(base) => {
                slot += scope.map_or(0, |scope| super::record_star_width(base, scope));
            }
            SelectItem::Expr { expression, .. } => {
                if slot < count && columns[slot].type_oid == crate::sql::types::oid::RECORD {
                    let handle = match scope {
                        Some(scope) => crate::sql::exec::register_shape_for(
                            expression,
                            &super::CatalogScopeCols {
                                scope,
                                outer_scope: None,
                                storage,
                                txid,
                            },
                        ),
                        None => crate::sql::exec::register_shape_for(
                            expression,
                            &crate::sql::exec::CatalogCols {
                                definition: None,
                                alias: None,
                                storage,
                                txid,
                            },
                        ),
                    };
                    if let Some(handle) = handle {
                        columns[slot].type_mod = handle;
                    }
                }
                slot += 1;
            }
        }
    }
    Ok(())
}

/// The common type of two set-operation columns: equal types, the numeric
/// tower, or (else) an error signalled by None.
pub(crate) fn coerce_set_value<'a>(
    value: Datum<'a>,
    target: ColType,
    arena: &'a Arena,
) -> Result<Datum<'a>, SqlError> {
    match (value, target) {
        (value @ Datum::Composite { slot, .. }, ColType::Composite(expected))
            if slot == expected =>
        {
            Ok(value)
        }
        (value @ Datum::CompositeText { slot, .. }, ColType::Composite(expected))
            if slot == expected =>
        {
            Ok(value)
        }
        (value, target) => crate::sql::eval::cast_to(value, target, arena),
    }
}

pub(crate) fn unify_set_type(a: ColType, b: ColType) -> Option<ColType> {
    if a == b {
        return Some(a);
    }
    // Catalog object aliases are OID domains. Set operations erase that
    // alias, exactly as PostgreSQL does when an OID catalog column meets a
    // `regclass`/`regproc`-style expression.
    if (a == ColType::Oid && b.is_reg_object()) || (b == ColType::Oid && a.is_reg_object()) {
        return Some(ColType::Oid);
    }
    // The full numeric tower — omitting int2 or real here left `smallint UNION
    // integer` and `real UNION double precision` failing to unify.
    let numeric = |t| {
        matches!(
            t,
            ColType::Int2
                | ColType::Int4
                | ColType::Int8
                | ColType::Float4
                | ColType::Float8
                | ColType::Numeric
        )
    };
    if numeric(a) && numeric(b) {
        return Some(exec::unify_numeric_tower(a, b));
    }
    None
}

/// Materializes a set tree to self-describing rows, coercing every leaf's rows
/// to the columns' common `target` types so the combining operators can match
/// rows by their encoded bytes.
#[expect(
    clippy::too_many_arguments,
    reason = "set tree evaluation carries statement context"
)]
fn eval_set_tree<'a>(
    tree: &'a SetTree<'a>,
    storage: &'a Storage,
    txid: u32,
    arena: &'a Arena,
    params: &[Datum<'a>],
    sequences: Option<&dyn SequenceAccess>,
    target: &[ColType],
    collations: &[Collation],
) -> Result<&'a mut [&'a [u8]], SqlError> {
    match tree {
        SetTree::Select(s) => eval_set_leaf(s, storage, txid, arena, params, sequences, target),
        SetTree::Op {
            operator,
            all,
            left,
            right,
        } => {
            let l = eval_set_tree(
                left, storage, txid, arena, params, sequences, target, collations,
            )?;
            let r = eval_set_tree(
                right, storage, txid, arena, params, sequences, target, collations,
            )?;
            combine_sets(storage, collations, *operator, *all, l, r, arena)
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
    let leaf0 = leaves[0].expect(">=1 leaf");
    let n_cols = describe_leaf(storage, leaf0, txid, columns, arena)?;
    register_first_leaf_record_shapes(storage, leaf0, txid, arena, columns, n_cols)?;
    let mut leaf_collations = [[SetColumnCollation::NONE; MAX_PROJ]; MAX_SET_LEAVES];
    // `None` = still undetermined (an untyped NULL / UNKNOWN column adopts the
    // type of the other branches, as PostgreSQL resolves an unknown literal).
    let mut target: [Option<ColType>; MAX_PROJ] = [None; MAX_PROJ];
    for (c, col) in columns[..n_cols].iter().enumerate() {
        let unknown = leaf_col_unknown(storage, leaf0, c, txid, arena);
        if !unknown {
            leaf_collations[0][c] = SetColumnCollation {
                value: col.collation,
                derivation: col.collation_derivation,
            };
        }
        target[c] = if unknown {
            None
        } else {
            Some(
                exec::catalog_column_type(storage, txid, col.type_oid)
                    .map(|(ctype, _)| ctype)
                    .ok_or_else(|| {
                        sql_err!(
                            sqlstate::FEATURE_NOT_SUPPORTED,
                            "set-operation column {} type (oid {}) is not supported",
                            c + 1,
                            col.type_oid
                        )
                    })?,
            )
        };
    }
    for (leaf_index, leaf) in leaves[1..n_leaves].iter().enumerate() {
        let mut lc = [ColDesc::new("", 0, 0); MAX_PROJ];
        let ln = describe_leaf(storage, leaf.expect("leaf"), txid, &mut lc, arena)?;
        if ln != n_cols {
            return Err(sql_err!(
                sqlstate::SYNTAX_ERROR,
                "each UNION query must have the same number of columns"
            ));
        }
        let leaf_ref = leaf.expect("leaf");
        for c in 0..n_cols {
            if leaf_col_unknown(storage, leaf_ref, c, txid, arena) {
                continue; // an untyped NULL column adopts the running type
            }
            leaf_collations[leaf_index + 1][c] = SetColumnCollation {
                value: lc[c].collation,
                derivation: lc[c].collation_derivation,
            };
            let lt = exec::catalog_column_type(storage, txid, lc[c].type_oid)
                .map(|(ctype, _)| ctype)
                .ok_or_else(|| {
                    sql_err!(
                        sqlstate::FEATURE_NOT_SUPPORTED,
                        "set-operation column {} type (oid {}) is not supported",
                        c + 1,
                        lc[c].type_oid
                    )
                })?;
            match target[c] {
                None => target[c] = Some(lt),
                Some(existing) => match unify_set_type(existing, lt) {
                    Some(t) => target[c] = Some(t),
                    None => {
                        return Err(sql_err!(
                            sqlstate::DATATYPE_MISMATCH,
                            "UNION types {} and {} cannot be matched",
                            existing.name(),
                            lt.name()
                        ));
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
    let mut collation_workspace = [[SetColumnCollation::NONE; MAX_PROJ]; MAX_SET_NODES];
    let mut next_collation_leaf = 0;
    let mut next_collation_node = 0;
    let collation_root = describe_set_collations(
        tree,
        n_cols,
        &leaf_collations,
        &mut next_collation_leaf,
        &mut collation_workspace,
        &mut next_collation_node,
    )?;
    for (column, collation) in columns[..n_cols]
        .iter_mut()
        .zip(&collation_workspace[collation_root])
    {
        column.collation = collation.value;
        column.collation_derivation = collation.derivation;
    }
    Ok(n_cols)
}

/// The result of materializing a set-operation body: the combined encoded rows,
/// the unified per-column types, and the column count.
type MaterializedSet<'a> = (&'a [&'a [u8]], &'a [ColType], usize);

/// Materializes a set-operation body to combined encoded rows plus the unified
/// column types, ready to decode. Shared by subquery and INSERT-source paths.
pub(crate) fn materialize_set_body<'a>(
    storage: &Storage,
    txid: u32,
    tree: &'a SetTree<'a>,
    arena: &'a Arena,
    params: &[Datum<'a>],
    sequences: Option<&dyn SequenceAccess>,
) -> Result<MaterializedSet<'a>, SqlError> {
    let materialized = materialize_set_body_tied(storage, txid, tree, arena, params, sequences)?;
    // `materialize_set_body_tied` ties all inputs to one lifetime because the
    // row executor may temporarily decode datums borrowed from storage. None
    // of those datums escape: every output row is projected-encoded into
    // `arena`, and the target type slice is allocated there too. Express that
    // provenance at this choke point so callers may release the catalog borrow
    // before mutating storage.
    Ok(unsafe { core::mem::transmute::<MaterializedSet<'_>, MaterializedSet<'a>>(materialized) })
}

fn materialize_set_body_tied<'a>(
    storage: &'a Storage,
    txid: u32,
    tree: &'a SetTree<'a>,
    arena: &'a Arena,
    params: &[Datum<'a>],
    sequences: Option<&dyn SequenceAccess>,
) -> Result<MaterializedSet<'a>, SqlError> {
    let mut columns = [ColDesc::new("", 0, 0); MAX_PROJ];
    let n = describe_set_body(storage, tree, txid, &mut columns, arena)?;
    let mut tgt = [ColType::Bool; MAX_PROJ];
    for c in 0..n {
        tgt[c] = exec::catalog_column_type(storage, txid, columns[c].type_oid)
            .map(|(ctype, _)| ctype)
            .ok_or_else(|| {
                sql_err!(
                    sqlstate::FEATURE_NOT_SUPPORTED,
                    "set-operation column {} type (oid {}) is not supported",
                    c + 1,
                    columns[c].type_oid
                )
            })?;
    }
    let target = arena
        .alloc_slice_copy(&tgt[..n])
        .map_err(|_| arena_full())?;
    let mut collations = [Collation::None; MAX_PROJ];
    for (index, column) in columns[..n].iter().enumerate() {
        collations[index] = column.collation;
    }
    let rows = eval_set_tree(
        tree,
        storage,
        txid,
        arena,
        params,
        sequences,
        target,
        &collations[..n],
    )?;
    Ok((rows, target, n))
}

fn eval_set_leaf<'a>(
    s: &'a Select<'a>,
    storage: &'a Storage,
    txid: u32,
    arena: &'a Arena,
    params: &[Datum<'a>],
    sequences: Option<&dyn SequenceAccess>,
    target: &[ColType],
) -> Result<&'a mut [&'a [u8]], SqlError> {
    // Pass 1: count the rows. Pass 2: coerce to the target types and encode.
    let mut count = 0usize;
    let dry_sequences = sequences.map(DrySequence);
    let dry_mark = arena.mark();
    select_into_rows(
        storage,
        txid,
        s,
        arena,
        params,
        None,
        dry_sequences
            .as_ref()
            .map(|sequence| sequence as &dyn SequenceAccess),
        &mut |_| {
            count += 1;
            Ok(())
        },
    )?;
    // The count pass cannot retain datums and uses a non-mutating sequence
    // adapter. Recycle its executor scopes and row scratch before the real
    // pass; otherwise every set leaf permanently consumes the work arena
    // twice, and recursive terms compound that cost per iteration.
    unsafe { arena.rewind_to(dry_mark) };
    let empty: &[u8] = &[];
    let rows = arena
        .alloc_slice_with(count, |_| empty)
        .map_err(|_| arena_full())?;
    let n = target.len();
    let mut at = 0usize;
    select_into_rows(
        storage,
        txid,
        s,
        arena,
        params,
        None,
        sequences,
        &mut |vals| {
            if vals.len() != n {
                return Err(sql_err!(
                    sqlstate::SYNTAX_ERROR,
                    "each UNION query must have the same number of columns"
                ));
            }
            let mut coerced = [Datum::Null; MAX_PROJ];
            for c in 0..n {
                coerced[c] = coerce_set_value(vals[c], target[c], arena)?;
            }
            rows[at] = exec::encode_projected_pub(&coerced[..n], arena)?;
            at += 1;
            Ok(())
        },
    )?;
    Ok(rows)
}

/// Combines two encoded-row multisets. Both inputs are sorted here (set ops are
/// unordered until the final ORDER BY), then merged by equal runs.
fn combine_sets<'a>(
    storage: &Storage,
    collations: &[Collation],
    operator: SetOp,
    all: bool,
    l: &'a mut [&'a [u8]],
    r: &'a mut [&'a [u8]],
    arena: &'a Arena,
) -> Result<&'a mut [&'a [u8]], SqlError> {
    // UNION ALL preserves order (left rows then right, as scanned); only the
    // distinct set operations sort to merge/dedup.
    if !(operator == SetOp::Union && all) {
        let mut error = None;
        crate::mem::arena::stable_sort_via(arena, l, |left, right| {
            match compare_set_rows(storage, collations, left, right) {
                Ok(ordering) => ordering,
                Err(value) => {
                    error = Some(value);
                    core::cmp::Ordering::Equal
                }
            }
        })
        .map_err(|_| arena_full())?;
        if let Some(error) = error {
            return Err(error);
        }
        crate::mem::arena::stable_sort_via(arena, r, |left, right| {
            match compare_set_rows(storage, collations, left, right) {
                Ok(ordering) => ordering,
                Err(value) => {
                    error = Some(value);
                    core::cmp::Ordering::Equal
                }
            }
        })
        .map_err(|_| arena_full())?;
        if let Some(error) = error {
            return Err(error);
        }
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
                let take_l = j >= r.len()
                    || (i < l.len() && compare_set_rows(storage, collations, l[i], r[j])?.is_le());
                let row = if take_l {
                    i += 1;
                    l[i - 1]
                } else {
                    j += 1;
                    r[j - 1]
                };
                let distinct = match last {
                    Some(prior) => !compare_set_rows(storage, collations, prior, row)?.is_eq(),
                    None => true,
                };
                if distinct {
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
                while i < l.len() && compare_set_rows(storage, collations, l[i], row)?.is_eq() {
                    cl += 1;
                    i += 1;
                }
                // Advance r past smaller values, then count the matching run.
                while j < r.len() && compare_set_rows(storage, collations, r[j], row)?.is_lt() {
                    j += 1;
                }
                let mut chained_row = 0;
                while j < r.len() && compare_set_rows(storage, collations, r[j], row)?.is_eq() {
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

fn compare_set_rows(
    storage: &Storage,
    collations: &[Collation],
    left: &[u8],
    right: &[u8],
) -> Result<core::cmp::Ordering, SqlError> {
    for (index, &collation) in collations.iter().enumerate() {
        let left_value = exec::decode_projected_pub(left, index);
        let right_value = exec::decode_projected_pub(right, index);
        let ordering = match (left_value.is_null(), right_value.is_null()) {
            (true, true) => core::cmp::Ordering::Equal,
            (true, false) => core::cmp::Ordering::Less,
            (false, true) => core::cmp::Ordering::Greater,
            (false, false) => {
                compare_datums_collated(storage, collation, &left_value, &right_value)?
            }
        };
        if !ordering.is_eq() {
            return Ok(ordering);
        }
    }
    Ok(core::cmp::Ordering::Equal)
}

/// Sorts combined set-operation rows by the trailing ORDER BY, which may
/// reference an output column by 1-based position or by name (from the first
/// leaf). Other ORDER BY expressions over a set operation are unsupported.
/// Whether two set-operation output rows tie on every ORDER BY column (the
/// `WITH TIES` peer test). The ORDER BY has already been validated by
/// [`sort_set_rows`], so an unresolvable key conservatively counts as no tie.
pub(crate) fn set_rows_tie(
    storage: &Storage,
    a: &[u8],
    b: &[u8],
    order_by: &[OrderBy],
    columns: &[ColDesc],
) -> Result<bool, SqlError> {
    for ob in order_by {
        let index = match ob.expression {
            Expr::Int(n) if *n >= 1 && (*n as usize) <= columns.len() => (*n as usize) - 1,
            Expr::Column {
                name,
                qualifier: None,
            } => match columns.iter().position(|c| c.name == *name) {
                Some(i) => i,
                None => return Ok(false),
            },
            _ => return Ok(false),
        };
        let va = exec::decode_projected_pub(a, index);
        let vb = exec::decode_projected_pub(b, index);
        let equal = match (va.is_null(), vb.is_null()) {
            (true, true) => true,
            (false, false) => {
                compare_datums_collated(storage, columns[index].collation, &va, &vb)?.is_eq()
            }
            _ => false,
        };
        if !equal {
            return Ok(false);
        }
    }
    Ok(true)
}

pub(crate) fn sort_set_rows(
    storage: &Storage,
    arena: &Arena,
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
            Expr::Column {
                name,
                qualifier: None,
            } => match columns.iter().position(|c| c.name == *name) {
                Some(i) => i,
                None => {
                    return Err(sql_err!(
                        sqlstate::UNDEFINED_COLUMN,
                        "ORDER BY column \"{}\" does not exist in the set-operation result",
                        name
                    ));
                }
            },
            _ => {
                return Err(sql_err!(
                    sqlstate::FEATURE_NOT_SUPPORTED,
                    "ORDER BY on a set operation must name an output column or its position"
                ));
            }
        };
        if columns[index].collation_derivation == CollationDerivation::Indeterminate {
            return Err(sql_err!(
                sqlstate::INDETERMINATE_COLLATION,
                "could not determine which collation to use for string comparison"
            ));
        }
        keys[nk] = (index, ob.descending, ob.nulls_first);
        nk += 1;
    }
    let keys = &keys[..nk];
    let mut err: Option<SqlError> = None;
    crate::mem::arena::stable_sort_via(arena, rows, |a, b| {
        if err.is_some() {
            return core::cmp::Ordering::Equal;
        }
        for &(index, descending, nulls_first) in keys {
            let va = exec::decode_projected_pub(a, index);
            let vb = exec::decode_projected_pub(b, index);
            let ord = match (va.is_null(), vb.is_null()) {
                (true, true) => core::cmp::Ordering::Equal,
                (true, false) => {
                    if nulls_first {
                        core::cmp::Ordering::Less
                    } else {
                        core::cmp::Ordering::Greater
                    }
                }
                (false, true) => {
                    if nulls_first {
                        core::cmp::Ordering::Greater
                    } else {
                        core::cmp::Ordering::Less
                    }
                }
                (false, false) => {
                    match compare_datums_collated(storage, columns[index].collation, &va, &vb) {
                        Ok(o) => {
                            if descending {
                                o.reverse()
                            } else {
                                o
                            }
                        }
                        Err(e) => {
                            err = Some(e);
                            core::cmp::Ordering::Equal
                        }
                    }
                }
            };
            if ord != core::cmp::Ordering::Equal {
                return ord;
            }
        }
        core::cmp::Ordering::Equal
    })
    .map_err(|_| arena_full())?;
    match err {
        Some(e) => Err(e),
        None => Ok(()),
    }
}
