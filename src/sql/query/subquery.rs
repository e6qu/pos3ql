//! Subqueries: evaluating a nested SELECT where a scalar, a row, or a set is
//! expected.
//!
//! SQL scans prepare uncorrelated results once and correlated results per
//! outer row. Procedural expressions execute only the subqueries they reach,
//! preserving CASE and boolean short-circuit behavior. Outer columns and
//! variables are chained onto the inner scope; returned bytes are detached
//! before a procedural command can change their storage.

use crate::mem::arena::{Arena, ArenaList};
use crate::sql::ast::{BinaryOp, Collation, Expr, Select, SelectItem, SetTree};
use crate::sql::eval::{
    ColumnLookup, EvalHooks, SqlError, SubqueryList, SubqueryListProbe, SubqueryValues, eval_full,
    sqlstate,
};
use crate::sql::types::{ColType, Datum, RecordField};
use crate::sql_err;
use crate::stack_format;
use crate::storage::Storage;

use super::{
    CatalogScopeCols, Chained, QueryScope, SUBQUERY_DEPTH, ScopeCols, ScopeSchema, arena_full,
    cmp_key_rows, collect_aggs, collect_table_sample_expressions, fold_aggregates,
    fromless_aggregate_hooks, pax_column_demand, scan_source_with_pax_columns, select_into_rows,
    select_into_rows_recycling, where_passes,
};
use crate::sql::exec::MAX_PROJ;

pub(super) const fn empty_subquery_list<'a>() -> SubqueryList<'a> {
    SubqueryList {
        node: core::ptr::null(),
        values: &[],
        probe: None,
        saw_null: false,
        witness: Datum::Null,
        collations: &[],
    }
}

#[derive(Clone, Copy)]
struct ExternalSubqueryList {
    access: crate::storage::ExternalRunAccess,
    run: crate::sql::external::ExternalRun,
}

impl SubqueryListProbe for ExternalSubqueryList {
    fn is_empty(&self) -> bool {
        self.run.rows() == 0
    }

    fn probe<'a>(
        &self,
        value: Datum<'a>,
        columns: crate::sql::eval::QuantifiedColumns<'_>,
        catalog: Option<&dyn crate::sql::eval::CatalogAccess>,
        arena: &'a Arena,
    ) -> Result<(bool, bool), SqlError> {
        let mut reader = self.access.reader()?;
        self.access
            .with_blocks(|blocks| reader.start(blocks, self.run))?;
        let mut saw_unknown = false;
        while let Some(row) = reader.row() {
            crate::sql::query::check_timeout()?;
            let mark = arena.mark();
            let member = crate::sql::exec::decode_projected_col_record(row, 0, arena)?;
            if member.is_null() {
                saw_unknown = true;
            } else {
                match crate::sql::eval::quantified_comparison(
                    BinaryOp::Eq,
                    value,
                    member,
                    columns.row_constructor,
                    columns.collations,
                    catalog,
                    arena,
                )? {
                    Datum::Bool(true) => {
                        // SAFETY: `member` is dead after this branch; `value`
                        // was allocated below the mark by the caller.
                        unsafe { arena.rewind_to(mark) };
                        return Ok((true, saw_unknown));
                    }
                    Datum::Bool(false) => {}
                    Datum::Null => saw_unknown = true,
                    _ => unreachable!("equality returns boolean or NULL"),
                }
            }
            // SAFETY: the decoded member is consumed by the comparison above.
            unsafe { arena.rewind_to(mark) };
            self.access.with_blocks(|blocks| reader.advance(blocks))?;
        }
        Ok((false, saw_unknown))
    }

    fn quantify<'a>(
        &self,
        value: Datum<'a>,
        operator: BinaryOp,
        all: bool,
        columns: crate::sql::eval::QuantifiedColumns<'_>,
        catalog: Option<&dyn crate::sql::eval::CatalogAccess>,
        arena: &'a Arena,
    ) -> Result<Datum<'a>, SqlError> {
        let mut reader = self.access.reader()?;
        self.access
            .with_blocks(|blocks| reader.start(blocks, self.run))?;
        let mut saw_unknown = false;
        while let Some(row) = reader.row() {
            crate::sql::query::check_timeout()?;
            let mark = arena.mark();
            let member = crate::sql::exec::decode_projected_col_record(row, 0, arena)?;
            let left = crate::sql::eval::coerce_unknown(value, &member)?;
            let right = crate::sql::eval::coerce_unknown(member, &left)?;
            let result = crate::sql::eval::quantified_comparison(
                operator,
                left,
                right,
                columns.row_constructor,
                columns.collations,
                catalog,
                arena,
            )?;
            match result {
                Datum::Bool(true) if !all => {
                    // SAFETY: decoded values are dead after the comparison.
                    unsafe { arena.rewind_to(mark) };
                    return Ok(Datum::Bool(true));
                }
                Datum::Bool(false) if all => {
                    // SAFETY: decoded values are dead after the comparison.
                    unsafe { arena.rewind_to(mark) };
                    return Ok(Datum::Bool(false));
                }
                Datum::Null => saw_unknown = true,
                Datum::Bool(_) => {}
                _ => unreachable!("comparison operators return boolean or NULL"),
            }
            // SAFETY: decoded values are dead after the comparison.
            unsafe { arena.rewind_to(mark) };
            self.access.with_blocks(|blocks| reader.advance(blocks))?;
        }
        Ok(if saw_unknown {
            Datum::Null
        } else {
            Datum::Bool(all)
        })
    }
}

/// The type witness for a spooled subquery run's single result column. A
/// wildcard select item carries no expression to infer from, so its single
/// expanded column resolves through the scope — mirroring `run_subquery`'s
/// wildcard expansion, and giving the IN-operand coercion the true column
/// type even over an empty set. A set-operation body has no projection
/// expression at all, so its first column's type is described from the set
/// tree. The witness is part of coercion semantics, so every accepted result
/// type must resolve through the catalog before execution begins.
fn spooled_column_witness<'a>(
    select: &'a Select<'a>,
    storage: &'a Storage,
    txid: u32,
    scope: Option<&QueryScope<'a>>,
    arena: &'a Arena,
    item: &'a Expr<'a>,
    outer: Option<&dyn ColumnLookup<'a>>,
) -> Result<Datum<'a>, SqlError> {
    if let Some(tree) = select.set_body {
        let mut columns = [crate::sql::types::ColDesc::new("", 0, 0); MAX_PROJ];
        super::setops::describe_set_body(
            storage,
            tree,
            txid,
            &mut columns,
            arena,
            None,
            crate::sql::exec::DescriptionTarget::Sql,
            None,
        )?;
        if let Some((ColType::Composite(slot), _)) =
            crate::sql::exec::catalog_column_type(storage, txid, columns[0].type_oid)
        {
            return composite_slot_witness(
                storage,
                txid,
                crate::storage::TypeCreation::require(slot)?,
                arena,
            )
            .map(|(witness, _)| witness);
        }
        return catalog_type_witness(storage, txid, columns[0].type_oid);
    }
    match select.items.first() {
        Some(SelectItem::Wildcard(_)) => {
            let scope = scope.ok_or_else(|| {
                sql_err!(
                    sqlstate::SYNTAX_ERROR,
                    "SELECT * with no tables specified is not valid"
                )
            })?;
            if scope.star_columns() != 1 {
                return Err(sql_err!(
                    sqlstate::SYNTAX_ERROR,
                    "subquery must return only one column"
                ));
            }
            type_witness(scope.output_type(scope.star_entry(0)))
        }
        Some(SelectItem::TableWildcard(qualifier)) => {
            let scope = scope.ok_or_else(|| {
                sql_err!(
                    sqlstate::SYNTAX_ERROR,
                    "SELECT * with no tables specified is not valid"
                )
            })?;
            if scope.qualified_star_columns(qualifier)? != 1 {
                return Err(sql_err!(
                    sqlstate::SYNTAX_ERROR,
                    "subquery must return only one column"
                ));
            }
            type_witness(scope.output_type(scope.qualified_star_entry(qualifier, 0)?))
        }
        _ => {
            if let Some((witness, _)) = scope_record_witness(item, scope, storage, txid, arena)? {
                Ok(witness)
            } else {
                subquery_witness_with_outer(storage, txid, item, scope, outer, arena)
            }
        }
    }
}

fn composite_slot_witness<'a>(
    storage: &'a Storage,
    txid: u32,
    slot: crate::storage::TypeCreation,
    arena: &'a Arena,
) -> Result<(Datum<'a>, &'a [Collation]), SqlError> {
    let definition = storage.row_type_definition(slot, txid)?.ok_or_else(|| {
        sql_err!(
            sqlstate::UNDEFINED_OBJECT,
            "composite type no longer exists"
        )
    })?;
    let mut fields = [RecordField {
        name: "",
        type_oid: 0,
        value: Datum::Null,
    }; MAX_PROJ];
    let mut collations = [Collation::None; MAX_PROJ];
    let mut count = 0;
    for field in definition.active_fields() {
        fields[count] = RecordField {
            name: arena
                .alloc_str(field.name.as_str())
                .map_err(|_| arena_full())?,
            type_oid: storage
                .routine_type_oid(field.ctype, field.user_type, txid)
                .ok_or_else(|| {
                    sql_err!(
                        sqlstate::INTERNAL_ERROR,
                        "composite field type is absent from the catalog"
                    )
                })?,
            value: type_witness(field.ctype)?,
        };
        collations[count] = if field.ctype.is_collatable() {
            Collation::Default
        } else {
            Collation::None
        };
        count += 1;
    }
    let fields = arena
        .alloc_slice_copy(&fields[..count])
        .map_err(|_| arena_full())?;
    let collations = arena
        .alloc_slice_copy(&collations[..count])
        .map_err(|_| arena_full())?;
    Ok((
        Datum::Composite {
            slot,
            fields: &*fields,
        },
        &*collations,
    ))
}

fn scope_record_witness<'a>(
    expression: &'a Expr<'a>,
    scope: Option<&QueryScope<'a>>,
    storage: &'a Storage,
    txid: u32,
    arena: &'a Arena,
) -> Result<Option<(Datum<'a>, &'a [Collation])>, SqlError> {
    if let Expr::Cast { type_name, .. } = expression
        && let Some(slot) = storage.resolve_composite_slot(type_name, txid)
    {
        return composite_slot_witness(
            storage,
            txid,
            storage.composite_for(slot, txid).creation(),
            arena,
        )
        .map(Some);
    }
    let Some(scope) = scope else {
        return Ok(None);
    };
    let table_name = match expression {
        Expr::WholeRow(name) => Some(*name),
        Expr::Column {
            qualifier: None,
            name,
        } if scope.source(name).is_ok() => Some(*name),
        _ => None,
    };
    if let Some(table_name) = table_name {
        if scope.func_scalar_column(table_name).is_some() {
            return Ok(None);
        }
        let source = scope.source(table_name)?;
        let columns = scope.source_columns(source);
        let mut fields = [RecordField {
            name: "",
            type_oid: 0,
            value: Datum::Null,
        }; MAX_PROJ];
        let mut collations = [Collation::None; MAX_PROJ];
        for (index, column) in columns.iter().enumerate() {
            fields[index] = RecordField {
                name: scope.defs.column_name(source.table, source.first + index),
                type_oid: storage
                    .routine_type_oid(column.ctype, column.user_type, txid)
                    .ok_or_else(|| {
                        sql_err!(
                            sqlstate::INTERNAL_ERROR,
                            "whole-row field type is absent from the catalog"
                        )
                    })?,
                value: type_witness(column.ctype)?,
            };
            collations[index] = column.collation;
        }
        let fields = arena
            .alloc_slice_copy(&fields[..columns.len()])
            .map_err(|_| arena_full())?;
        let collations = arena
            .alloc_slice_copy(&collations[..columns.len()])
            .map_err(|_| arena_full())?;
        return Ok(Some((Datum::Record(&*fields), &*collations)));
    }
    Ok(None)
}

/// Spools a quantified subquery result into the provider-neutral run stack.
/// Probes stream it with bounded reader scratch.
#[allow(clippy::too_many_arguments)]
fn external_list_subquery<'a>(
    node: &Expr<'a>,
    select: &'a Select<'a>,
    storage: &'a Storage,
    txid: u32,
    arena: &'a Arena,
    params: &[Datum<'a>],
    depth: u32,
    row_arity: usize,
    row_witness: Option<Datum<'a>>,
    collations: &'a [Collation],
    outer: Option<&dyn ColumnLookup<'a>>,
    sequences: Option<&dyn crate::sql::eval::SequenceAccess>,
) -> Result<SubqueryList<'a>, SqlError> {
    if depth == 0 {
        return Err(sql_err!(
            sqlstate::STATEMENT_TOO_COMPLEX,
            "subqueries nested too deeply"
        ));
    }
    let mut sorter = storage.external_sorter()?;
    sorter.reset();
    let mut compare = |_left: &[u8], _right: &[u8]| Ok(core::cmp::Ordering::Equal);
    let item = match select.items.first() {
        Some(SelectItem::Expr { expression, .. }) => *expression,
        _ => &Expr::Null,
    };
    let own_scope = select
        .from
        .as_ref()
        .map(|from| QueryScope::resolve_schema(storage, from, txid, arena, outer))
        .transpose()?;
    let mut witness = if let Some(witness) = row_witness {
        witness
    } else {
        match spooled_column_witness(
            select,
            storage,
            txid,
            own_scope.as_ref(),
            arena,
            item,
            outer,
        ) {
            Ok(witness) => witness,
            // An outer row can make an otherwise valid correlated projection
            // impossible to infer before that row is available. `Null` is a
            // deferred witness, never a replacement type: its eventual value
            // is compared without coercion and a non-correlated failure stays
            // an error.
            Err(_) if outer.is_some() => Datum::Null,
            Err(error) => return Err(error),
        }
    };
    let assemble_record = row_arity > 1;
    let row_witness_fields = match row_witness {
        Some(Datum::Record(fields)) => Some(fields),
        _ => None,
    };
    if let Some(fields) = row_witness_fields {
        if fields.len() < row_arity {
            return Err(sql_err!(
                sqlstate::SYNTAX_ERROR,
                "subquery has too few columns"
            ));
        }
        if fields.len() > row_arity {
            return Err(sql_err!(
                sqlstate::SYNTAX_ERROR,
                "subquery has too many columns"
            ));
        }
    }
    let projected_columns = if assemble_record { row_arity } else { 1 };
    select_into_rows_recycling(
        storage,
        txid,
        select,
        arena,
        params,
        outer,
        sequences,
        &mut |values| {
            if values.len() != projected_columns {
                return Err(sql_err!(
                    sqlstate::SYNTAX_ERROR,
                    "subquery must return only one column"
                ));
            }
            let value = if !assemble_record {
                values[0]
            } else {
                let mut fields = [RecordField {
                    name: "",
                    type_oid: 0,
                    value: Datum::Null,
                }; MAX_PROJ];
                for (column, field) in fields.iter_mut().enumerate().take(row_arity) {
                    *field = RecordField {
                        name: "",
                        type_oid: row_witness_fields
                            .and_then(|fields| fields.get(column))
                            .map_or_else(|| values[column].type_oid(), |field| field.type_oid),
                        value: values[column],
                    };
                }
                let fields = arena
                    .alloc_slice_copy(&fields[..row_arity])
                    .map_err(|_| arena_full())?;
                Datum::Record(&*fields)
            };
            if row_arity == 1 && witness.is_null() && !value.is_null() {
                witness = catalog_type_witness(storage, txid, value.type_oid())?;
            }
            storage
                .with_block_store(|blocks| {
                    sorter.push_projected_by(blocks, 1, |_| value, &mut compare)
                })
                .expect("external subquery run has a block store")
        },
    )?;
    let run = storage
        .with_block_store(|blocks| sorter.finish(blocks, &mut compare))
        .expect("external subquery run has a block store")?;
    drop(sorter);
    let probe = match run {
        Some(run) => {
            let probe: &mut dyn SubqueryListProbe = arena
                .alloc(ExternalSubqueryList {
                    access: storage.external_run_access()?,
                    run,
                })
                .map_err(|_| arena_full())?;
            Some(core::ptr::NonNull::from(probe))
        }
        None => None,
    };
    Ok(SubqueryList {
        node: (node as *const Expr).cast(),
        values: &[],
        probe,
        saw_null: false,
        witness,
        collations,
    })
}

/// Evaluates a scalar subquery through the row callback seam. Only the first
/// row is retained and it is detached immediately; a second row raises the
/// cardinality error without first materializing the entire result set.
#[allow(clippy::too_many_arguments)]
fn streaming_scalar_subquery<'a>(
    select: &'a Select<'a>,
    storage: &'a Storage,
    txid: u32,
    arena: &'a Arena,
    params: &[Datum<'a>],
    depth: u32,
    outer: Option<&dyn ColumnLookup<'a>>,
    sequences: Option<&dyn crate::sql::eval::SequenceAccess>,
) -> Result<(Datum<'a>, Datum<'a>), SqlError> {
    if depth == 0 {
        return Err(sql_err!(
            sqlstate::STATEMENT_TOO_COMPLEX,
            "subqueries nested too deeply"
        ));
    }
    let item = match select.items.first() {
        Some(SelectItem::Expr { expression, .. }) => *expression,
        _ => &Expr::Null,
    };
    let own_scope = select
        .from
        .as_ref()
        .map(|from| QueryScope::resolve_schema(storage, from, txid, arena, outer))
        .transpose()?;
    let witness = spooled_column_witness(
        select,
        storage,
        txid,
        own_scope.as_ref(),
        arena,
        item,
        outer,
    )?;
    let mut sorter = storage.external_sorter()?;
    sorter.reset();
    let mut compare = |_left: &[u8], _right: &[u8]| Ok(core::cmp::Ordering::Equal);
    let mut count = 0usize;
    select_into_rows_recycling(
        storage,
        txid,
        select,
        arena,
        params,
        outer,
        sequences,
        &mut |values| {
            if values.len() != 1 {
                return Err(sql_err!(
                    sqlstate::SYNTAX_ERROR,
                    "subquery must return only one column"
                ));
            }
            if count > 0 {
                return Err(sql_err!(
                    sqlstate::CARDINALITY_VIOLATION,
                    "more than one row returned by a subquery used as an expression"
                ));
            }
            count += 1;
            // Scalar subqueries preserve the datum produced by their select.
            // Array constructors have a target element type and normalize each
            // member separately; applying that rule here can turn a runtime
            // aggregate result (for example numeric) into its static text
            // witness before an enclosing comparison sees it.
            let value = values[0];
            storage
                .with_block_store(|blocks| {
                    sorter.push_projected_by(blocks, 1, |_| value, &mut compare)
                })
                .expect("external scalar-subquery run has a block store")
        },
    )?;
    // Cardinality is checked before each push, so the only retained row always
    // fits the startup-sized chunk and never needs a temporary object.
    let value = match sorter.in_memory_rows(&mut compare)? {
        Some(0) => Datum::Null,
        Some(1) => {
            let encoded = arena
                .alloc_slice_copy(sorter.in_memory_row(0))
                .map_err(|_| arena_full())?;
            crate::sql::exec::decode_projected_col_record(encoded, 0, arena)?
        }
        Some(_) => unreachable!("scalar subquery cardinality was checked while streaming"),
        None => unreachable!("one scalar-subquery row cannot spill its startup-sized chunk"),
    };
    Ok((value, witness))
}

/// Spools an `ARRAY(subquery)` result into a provider-neutral run, then builds
/// the array datum from the streamed values. The intermediate collection (per-
/// row Datums, ORDER BY keys, sort indices) never touches the statement arena;
/// only the final array blob does.
#[allow(clippy::too_many_arguments)]
fn streaming_array_subquery<'a>(
    select: &'a Select<'a>,
    storage: &'a Storage,
    txid: u32,
    arena: &'a Arena,
    params: &[Datum<'a>],
    depth: u32,
    outer: Option<&dyn ColumnLookup<'a>>,
    sequences: Option<&dyn crate::sql::eval::SequenceAccess>,
) -> Result<(Datum<'a>, Datum<'a>), SqlError> {
    if depth == 0 {
        return Err(sql_err!(
            sqlstate::STATEMENT_TOO_COMPLEX,
            "subqueries nested too deeply"
        ));
    }
    let plan = array_subquery_plan(select, storage, txid, arena, outer)?;
    let mut sorter = storage.external_sorter()?;
    sorter.reset();
    let mut compare = |_left: &[u8], _right: &[u8]| Ok(core::cmp::Ordering::Equal);
    let mut count = 0usize;
    select_into_rows_recycling(
        storage,
        txid,
        select,
        arena,
        params,
        outer,
        sequences,
        &mut |values| {
            if values.len() != 1 {
                return Err(sql_err!(
                    sqlstate::SYNTAX_ERROR,
                    "subquery must return only one column"
                ));
            }
            count += 1;
            storage
                .with_block_store(|blocks| {
                    sorter.push_projected_by(blocks, 1, |_| values[0], &mut compare)
                })
                .expect("external array-subquery run has a block store")
        },
    )?;
    let run = storage
        .with_block_store(|blocks| sorter.finish(blocks, &mut compare))
        .expect("external array-subquery run has a block store")?;
    drop(sorter);
    let values: &'a [Datum<'a>] = match run {
        Some(run) => {
            let mut reader = storage.external_run_reader()?;
            storage
                .with_block_store(|blocks| reader.start(blocks, run))
                .expect("external array-subquery run has a block store")?;
            let buf = arena
                .alloc_slice_with(count, |_| Datum::Null)
                .map_err(|_| arena_full())?;
            for slot in buf.iter_mut() {
                let row = reader
                    .row()
                    .expect("external array-subquery row count is stable");
                let encoded = arena.alloc_slice_copy(row).map_err(|_| arena_full())?;
                *slot = crate::sql::exec::decode_projected_col_record(encoded, 0, arena)?;
                storage
                    .with_block_store(|blocks| reader.advance(blocks))
                    .expect("external array-subquery run has a block store")?;
            }
            buf
        }
        None => &[],
    };
    let v = build_array_scalar(values, plan, storage, txid, arena)?;
    Ok((v, v))
}

/// Walks an expression tree collecting subquery nodes, in discovery order and
/// deduplicated by node identity so each subquery keeps one stable position.
fn collect_subqueries<'a>(
    expression: &'a Expr<'a>,
    out: &mut ArenaList<'a, &'a Expr<'a>>,
) -> Result<(), SqlError> {
    if matches!(
        expression,
        Expr::Subquery(_)
            | Expr::RowSubquery { .. }
            | Expr::InSubquery { .. }
            | Expr::QuantifiedSubquery { .. }
            | Expr::Exists(_)
            | Expr::ArraySubquery(_)
    ) {
        if out.as_slice().iter().any(|&e| core::ptr::eq(e, expression)) {
            return Ok(());
        }
        out.push(expression).map_err(|_| arena_full())?;
        // The operand of IN (SELECT ..) may itself contain subqueries.
        if let Expr::InSubquery { operand, .. } | Expr::QuantifiedSubquery { operand, .. } =
            expression
        {
            collect_subqueries(operand, out)?;
        }
        return Ok(());
    }
    walk_children(expression, &mut |child| collect_subqueries(child, out))
}

/// PostgreSQL analyses each subquery level against its own FROM schema, so a
/// constant compared with a subquery's column converts to the column's type
/// before any row is read (`SELECT (SELECT a FROM t WHERE a = 'q')` fails on
/// an empty `t`).
pub(super) fn check_subquery_literals<'a>(
    expression: &'a Expr<'a>,
    outer: Option<&QueryScope<'a>>,
    storage: &Storage,
    txid: u32,
    arena: &'a Arena,
) -> Result<(), SqlError> {
    let mut nodes = ArenaList::new(arena);
    collect_subqueries(expression, &mut nodes)?;
    for node in nodes.as_slice() {
        let select = match node {
            Expr::Subquery(select) | Expr::ArraySubquery(select) | Expr::Exists(select) => select,
            Expr::RowSubquery { select, .. }
            | Expr::InSubquery { select, .. }
            | Expr::QuantifiedSubquery { select, .. } => select,
            _ => continue,
        };
        check_select_literals(select, outer, storage, txid, arena)?;
    }
    Ok(())
}

pub(super) fn check_select_literals<'a>(
    select: &'a Select<'a>,
    outer: Option<&QueryScope<'a>>,
    storage: &Storage,
    txid: u32,
    arena: &'a Arena,
) -> Result<(), SqlError> {
    if let Some(tree) = select.set_body {
        fn leaves<'t>(
            tree: &'t SetTree<'t>,
            visit: &mut dyn FnMut(&'t Select<'t>) -> Result<(), SqlError>,
        ) -> Result<(), SqlError> {
            match tree {
                SetTree::Select(select) => visit(select),
                SetTree::Op { left, right, .. } => {
                    leaves(left, visit)?;
                    leaves(right, visit)
                }
            }
        }
        return leaves(tree, &mut |leaf| {
            check_select_literals(leaf, outer, storage, txid, arena)
        });
    }
    let Some(from) = select.from.as_ref() else {
        for expression in select
            .items
            .iter()
            .filter_map(|item| match item {
                SelectItem::Expr { expression, .. } => Some(*expression),
                _ => None,
            })
            .chain(select.where_clause)
        {
            check_subquery_literals(expression, outer, storage, txid, arena)?;
        }
        return Ok(());
    };
    // A FROM clause that does not resolve is reported by the query's own
    // analysis when it runs, in PostgreSQL's order.
    let outer_columns = outer.map(super::ScopeSchema);
    let Ok(scope) = crate::sql::error_position::preserving_pending(|| {
        QueryScope::resolve_schema(
            storage,
            from,
            txid,
            arena,
            outer_columns
                .as_ref()
                .map(|columns| columns as &dyn ColumnLookup<'_>),
        )
    }) else {
        return Ok(());
    };
    let expressions = select
        .items
        .iter()
        .filter_map(|item| match item {
            SelectItem::Expr { expression, .. } => Some(*expression),
            _ => None,
        })
        .chain(select.where_clause)
        .chain(select.having)
        .chain(from.joins.iter().filter_map(|join| join.on));
    for expression in expressions {
        super::check_literal_operands(expression, &scope, arena)?;
        check_subquery_literals(expression, Some(&scope), storage, txid, arena)?;
    }
    Ok(())
}

/// Selects the already-classified correlated nodes that occur inside one
/// expression. Executors use this to evaluate WHERE's correlated subqueries
/// before the select-list ones, so rows rejected by ordinary join predicates
/// never materialize unused subquery results.
pub(super) fn correlated_in_expression<'a>(
    expression: Option<&'a Expr<'a>>,
    correlated: &[&'a Expr<'a>],
    arena: &'a Arena,
) -> Result<&'a [&'a Expr<'a>], SqlError> {
    let Some(expression) = expression else {
        return Ok(&[]);
    };
    let mut nodes = ArenaList::new(arena);
    collect_subqueries(expression, &mut nodes)?;
    let mut selected = ArenaList::new(arena);
    for &node in nodes.as_slice() {
        if correlated
            .iter()
            .any(|candidate| core::ptr::eq(*candidate, node))
        {
            selected.push(node).map_err(|_| arena_full())?;
        }
    }
    Ok(selected.as_slice())
}

/// The error-safe top-level WHERE conjuncts that do not depend on a correlated
/// subquery. They can run in the source scan; the caller still evaluates the
/// original predicate after it has supplied the correlated values.
pub(super) fn correlated_scan_conjuncts<'a>(
    predicate: Option<&'a Expr<'a>>,
    correlated: &[&'a Expr<'a>],
    arena: &'a Arena,
) -> Result<Option<&'a Expr<'a>>, SqlError> {
    let Some(predicate) = predicate else {
        return Ok(None);
    };
    if correlated.is_empty() {
        return Ok(Some(predicate));
    }
    fn contains(expression: &Expr, nodes: &[&Expr]) -> bool {
        if nodes.iter().any(|node| core::ptr::eq(*node, expression)) {
            return true;
        }
        let mut found = false;
        let _ = walk_children(expression, &mut |child| {
            if contains(child, nodes) {
                found = true;
            }
            Ok(())
        });
        found
    }

    let mut conjuncts = [predicate; super::plan::MAX_CONJUNCTS];
    let mut count = 0;
    if !super::plan::flatten_and(predicate, &mut conjuncts, &mut count) {
        return Ok(None);
    }
    let mut selected = None;
    for conjunct in &conjuncts[..count] {
        if contains(conjunct, correlated) || !super::plan::is_error_safe(conjunct) {
            continue;
        }
        selected = Some(match selected {
            None => *conjunct,
            Some(left) => arena
                .alloc(Expr::Binary {
                    source_position: crate::sql::ast::SourcePosition::UNKNOWN,
                    operator: BinaryOp::And,
                    left,
                    right: conjunct,
                })
                .map_err(|_| arena_full())?,
        });
    }
    Ok(selected)
}

pub(crate) fn walk_children<'a>(
    expression: &Expr<'a>,
    f: &mut dyn FnMut(&'a Expr<'a>) -> Result<(), SqlError>,
) -> Result<(), SqlError> {
    match expression {
        Expr::Unary { operand, .. }
        | Expr::Cast { operand, .. }
        | Expr::BoundDefault {
            expression: operand,
            ..
        }
        | Expr::Collate { operand, .. }
        | Expr::IsNull { operand, .. } => f(operand),
        Expr::Binary { left, right, .. } => {
            f(left)?;
            f(right)
        }
        Expr::Call {
            args,
            order_by,
            over,
            filter,
            ..
        } => {
            for a in *args {
                f(a)?;
            }
            for order in *order_by {
                f(order.expression)?;
            }
            if let Some(filter) = filter {
                f(filter)?;
            }
            if let Some(window) = over {
                for expression in window.partition_by {
                    f(expression)?;
                }
                for order in window.order_by {
                    f(order.expression)?;
                }
                if let Some(frame) = window.frame {
                    for bound in [frame.start, frame.end] {
                        match bound {
                            crate::sql::ast::FrameBound::Preceding(expression)
                            | crate::sql::ast::FrameBound::Following(expression) => f(expression)?,
                            crate::sql::ast::FrameBound::UnboundedPreceding
                            | crate::sql::ast::FrameBound::CurrentRow
                            | crate::sql::ast::FrameBound::UnboundedFollowing => {}
                        }
                    }
                }
            }
            Ok(())
        }
        Expr::InList { operand, list, .. } => {
            f(operand)?;
            for e in *list {
                f(e)?;
            }
            Ok(())
        }
        Expr::Like {
            operand,
            pattern,
            escape,
            ..
        } => {
            f(operand)?;
            f(pattern)?;
            if let Some(escape) = escape {
                f(escape)?;
            }
            Ok(())
        }
        Expr::Match {
            operand, pattern, ..
        } => {
            f(operand)?;
            f(pattern)
        }
        Expr::Case {
            operand,
            whens,
            otherwise,
            ..
        } => {
            if let Some(o) = operand {
                f(o)?;
            }
            for (c, r) in *whens {
                f(c)?;
                f(r)?;
            }
            if let Some(o) = otherwise {
                f(o)?;
            }
            Ok(())
        }
        Expr::InSubquery { operand, .. } | Expr::QuantifiedSubquery { operand, .. } => f(operand),
        // A quantified comparison's array side may be a collected subquery.
        Expr::AnyAll { operand, array, .. } => {
            f(operand)?;
            f(array)
        }
        Expr::Array(elements) => {
            for element in *elements {
                f(element)?;
            }
            Ok(())
        }
        Expr::Subscript { base, index } => {
            f(base)?;
            f(index)
        }
        Expr::Slice { base, lower, upper } => {
            f(base)?;
            if let Some(lower) = lower {
                f(lower)?;
            }
            if let Some(upper) = upper {
                f(upper)?;
            }
            Ok(())
        }
        Expr::Field { base, .. } | Expr::RecordFieldIndex { base, .. } => f(base),
        _ => Ok(()),
    }
}

struct SubqueryCollationLookup<'scope, 'definition, 'outer, 'row> {
    scope: Option<&'scope QueryScope<'definition>>,
    outer: Option<&'outer dyn ColumnLookup<'row>>,
}

impl<'a> ColumnLookup<'a> for SubqueryCollationLookup<'_, 'a, '_, 'a> {
    fn whole_row_field_metadata(
        &self,
        table: &str,
        index: usize,
        catalog: Option<&dyn crate::sql::eval::CatalogAccess>,
    ) -> Option<(crate::util::StackStr<64>, crate::sql::exec::StaticTypeMeta)> {
        let schema = self.scope.map(ScopeSchema);
        let empty = crate::sql::eval::NoColumns;
        let inner: &dyn ColumnLookup<'_> = match &schema {
            Some(schema) => schema,
            None => &empty,
        };
        Chained {
            inner,
            outer: self.outer,
        }
        .whole_row_field_metadata(table, index, catalog)
    }

    fn column_type_oid(&self, qualifier: Option<&str>, name: &str) -> Option<i32> {
        let schema = self.scope.map(ScopeSchema);
        let empty = crate::sql::eval::NoColumns;
        let inner: &dyn ColumnLookup<'_> = match &schema {
            Some(schema) => schema,
            None => &empty,
        };
        Chained {
            inner,
            outer: self.outer,
        }
        .column_type_oid(qualifier, name)
    }

    fn resolve_column_type(
        &self,
        qualifier: Option<&str>,
        name: &str,
    ) -> Result<ColType, SqlError> {
        let schema = self.scope.map(ScopeSchema);
        let empty = crate::sql::eval::NoColumns;
        let inner: &dyn ColumnLookup<'_> = match &schema {
            Some(schema) => schema,
            None => &empty,
        };
        Chained {
            inner,
            outer: self.outer,
        }
        .resolve_column_type(qualifier, name)
    }

    fn is_empty_scope(&self) -> bool {
        self.scope.is_none_or(|scope| scope.n == 0)
            && self.outer.is_none_or(|outer| outer.is_empty_scope())
    }

    fn whole_row_type(&self, table: &str) -> Option<ColType> {
        self.scope
            .and_then(|scope| scope.whole_row_type(table))
            .or_else(|| self.outer.and_then(|outer| outer.whole_row_type(table)))
    }

    fn lookup(&self, qualifier: Option<&str>, name: &str) -> Result<Datum<'a>, SqlError> {
        if self
            .scope
            .is_some_and(|scope| scope.find_column(qualifier, name).is_ok())
        {
            return Ok(Datum::Null);
        }
        self.outer
            .ok_or_else(|| crate::sql::exec::could_not_identify(name))?
            .lookup(qualifier, name)
    }

    fn col_type(&self, qualifier: Option<&str>, name: &str) -> Option<ColType> {
        self.scope
            .and_then(|scope| {
                scope
                    .find_column(qualifier, name)
                    .ok()
                    .map(|c| scope.output_type(c))
            })
            .or_else(|| self.outer.and_then(|outer| outer.col_type(qualifier, name)))
    }

    fn collation(&self, qualifier: Option<&str>, name: &str) -> Collation {
        self.scope
            .and_then(|scope| {
                scope
                    .find_column(qualifier, name)
                    .ok()
                    .map(|column| scope.output_collation(column))
            })
            .or_else(|| self.outer.map(|outer| outer.collation(qualifier, name)))
            .unwrap_or(Collation::None)
    }

    fn record_field_collation(&self, base: &Expr<'a>, field: &str) -> Collation {
        if let Some(scope) = self.scope {
            let collation =
                crate::sql::exec::record_field_metadata(base, field, &super::ScopeCols(scope))
                    .map_or(Collation::None, |meta| meta.collation);
            if collation != Collation::None {
                return collation;
            }
        }
        self.outer.map_or(Collation::None, |outer| {
            outer.record_field_collation(base, field)
        })
    }
}

fn subquery_collations<'a>(
    select: &'a Select<'a>,
    storage: &'a Storage,
    txid: u32,
    arena: &'a Arena,
    outer: Option<&dyn ColumnLookup<'a>>,
) -> Result<&'a [Collation], SqlError> {
    let mut output = [Collation::None; MAX_PROJ];
    if let Some(tree) = select.set_body {
        let mut columns = [crate::sql::types::ColDesc::new("", 0, 0); MAX_PROJ];
        let count = super::setops::describe_set_body(
            storage,
            tree,
            txid,
            &mut columns,
            arena,
            None,
            crate::sql::exec::DescriptionTarget::Sql,
            outer,
        )?;
        if count == 1
            && let Some((ColType::Composite(slot), _)) =
                crate::sql::exec::catalog_column_type(storage, txid, columns[0].type_oid)
        {
            return composite_slot_witness(
                storage,
                txid,
                crate::storage::TypeCreation::require(slot)?,
                arena,
            )
            .map(|(_, collations)| collations);
        }
        for (slot, column) in output.iter_mut().zip(&columns[..count]) {
            *slot = column.collation;
        }
        return arena
            .alloc_slice_copy(&output[..count])
            .map(|slice| &*slice)
            .map_err(|_| arena_full());
    }
    if select
        .items
        .iter()
        .any(|item| !matches!(item, SelectItem::Expr { .. }))
    {
        let mut columns = [crate::sql::types::ColDesc::new("", 0, 0); MAX_PROJ];
        let count = super::describe_select(
            select,
            storage,
            txid,
            arena,
            &mut columns,
            crate::sql::exec::DescriptionTarget::Sql,
            None,
        )?;
        for (slot, column) in output.iter_mut().zip(&columns[..count]) {
            *slot = column.collation;
        }
        return arena
            .alloc_slice_copy(&output[..count])
            .map(|slice| &*slice)
            .map_err(|_| arena_full());
    }
    let scope = select
        .from
        .as_ref()
        .map(|from| QueryScope::resolve_schema(storage, from, txid, arena, outer))
        .transpose()?;
    if let [SelectItem::Expr { expression, .. }] = select.items
        && let Some((_, collations)) =
            scope_record_witness(expression, scope.as_ref(), storage, txid, arena)?
    {
        return Ok(collations);
    }
    let lookup = SubqueryCollationLookup {
        scope: scope.as_ref(),
        outer,
    };
    let mut count = 0;
    for item in select.items {
        match item {
            SelectItem::Expr { expression, .. } => {
                let catalog = super::storage_catalog(storage, arena, txid);
                output[count] = crate::sql::eval::described_expression_collation(
                    expression,
                    &lookup,
                    Some(&catalog),
                )?
                .0;
                count += 1;
            }
            SelectItem::Wildcard(_) => {
                let scope = scope.as_ref().ok_or_else(|| {
                    sql_err!(
                        sqlstate::SYNTAX_ERROR,
                        "SELECT * with no tables specified is not valid"
                    )
                })?;
                for index in 0..scope.star_columns() {
                    output[count] = scope.output_collation(scope.star_entry(index));
                    count += 1;
                }
            }
            SelectItem::TableWildcard(qualifier) => {
                let scope = scope.as_ref().ok_or_else(|| {
                    sql_err!(
                        sqlstate::SYNTAX_ERROR,
                        "SELECT * with no tables specified is not valid"
                    )
                })?;
                for index in 0..scope.qualified_star_columns(qualifier)? {
                    output[count] =
                        scope.output_collation(scope.qualified_star_entry(qualifier, index)?);
                    count += 1;
                }
            }
            SelectItem::RecordStar(_) => unreachable!("expanded by catalog description above"),
        }
    }
    arena
        .alloc_slice_copy(&output[..count])
        .map(|slice| &*slice)
        .map_err(|_| arena_full())
}

fn row_subquery_witness<'a>(
    select: &'a Select<'a>,
    storage: &'a Storage,
    txid: u32,
    arena: &'a Arena,
    outer: Option<&dyn ColumnLookup<'a>>,
) -> Result<Datum<'a>, SqlError> {
    let mut fields = [RecordField {
        name: "",
        type_oid: 0,
        value: Datum::Null,
    }; MAX_PROJ];
    if let Some(tree) = select.set_body {
        let mut columns = [crate::sql::types::ColDesc::new("", 0, 0); MAX_PROJ];
        let count = super::setops::describe_set_body(
            storage,
            tree,
            txid,
            &mut columns,
            arena,
            None,
            crate::sql::exec::DescriptionTarget::Sql,
            None,
        )?;
        for (index, column) in columns[..count].iter().enumerate() {
            let value = catalog_type_witness(storage, txid, column.type_oid)?;
            fields[index] = RecordField {
                name: column.name,
                type_oid: column.type_oid,
                value,
            };
        }
        let fields = arena
            .alloc_slice_copy(&fields[..count])
            .map_err(|_| arena_full())?;
        return Ok(Datum::Record(&*fields));
    }
    if select
        .items
        .iter()
        .any(|item| !matches!(item, SelectItem::Expr { .. }))
    {
        let mut columns = [crate::sql::types::ColDesc::new("", 0, 0); MAX_PROJ];
        let count = super::describe_select(
            select,
            storage,
            txid,
            arena,
            &mut columns,
            crate::sql::exec::DescriptionTarget::Sql,
            None,
        )?;
        for (index, column) in columns[..count].iter().enumerate() {
            fields[index] = RecordField {
                name: column.name,
                type_oid: column.type_oid,
                value: catalog_type_witness(storage, txid, column.type_oid)?,
            };
        }
        let fields = arena
            .alloc_slice_copy(&fields[..count])
            .map_err(|_| arena_full())?;
        return Ok(Datum::Record(&*fields));
    }
    let scope = select
        .from
        .as_ref()
        .map(|from| QueryScope::resolve_schema(storage, from, txid, arena, outer))
        .transpose()?;
    for (index, item) in select.items.iter().enumerate() {
        let SelectItem::Expr { expression, alias } = item else {
            unreachable!("non-expression projections were described above")
        };
        let value =
            subquery_witness_with_outer(storage, txid, expression, scope.as_ref(), outer, arena)?;
        let generated = stack_format!(12, "f{}", index + 1);
        fields[index] = RecordField {
            name: match alias {
                Some(alias) => alias,
                None => arena
                    .alloc_str(generated.as_str())
                    .map_err(|_| arena_full())?,
            },
            type_oid: value.type_oid(),
            value,
        };
    }
    let fields = arena
        .alloc_slice_copy(&fields[..select.items.len()])
        .map_err(|_| arena_full())?;
    Ok(Datum::Record(&*fields))
}

/// Resolves a comparison subquery's row arity before execution.
fn row_projected<'a>(
    operand: &'a Expr<'a>,
    select: &'a Select<'a>,
    _arena: &'a Arena,
) -> Result<(&'a Select<'a>, usize), SqlError> {
    fn arity(operand: &Expr) -> usize {
        match operand {
            Expr::Call {
                kind: crate::sql::ast::CallKind::SqlSyntax,
                name,
                args,
                ..
            } if name.eq_ignore_ascii_case("row") => args.len(),
            // PostgreSQL retains RowExpr arity through an explicit cast when
            // checking the sublink's projected column count. A scalar cast
            // from text to a composite remains one value.
            Expr::Cast { operand, .. }
            | Expr::BoundDefault {
                expression: operand,
                ..
            } => arity(operand),
            _ => 1,
        }
    }
    let arity = arity(operand);
    if arity > 1 {
        return Ok((select, arity));
    }
    // A set-operation body carries its columns in the leaves, all of equal
    // arity (checked where the branches are combined), so the first one speaks
    // for the whole tree.
    let columns = match select.set_body {
        Some(tree) => set_leaf(tree).items.len(),
        None => select.items.len(),
    };
    if columns < arity {
        return Err(sql_err!(
            sqlstate::SYNTAX_ERROR,
            "subquery has too few columns"
        ));
    }
    if columns > arity {
        return Err(sql_err!(
            sqlstate::SYNTAX_ERROR,
            "subquery has too many columns"
        ));
    }
    Ok((select, arity))
}

/// The leftmost leaf of a set tree, which fixes the arity of the whole tree.
fn set_leaf<'a>(tree: &'a SetTree<'a>) -> &'a Select<'a> {
    match tree {
        SetTree::Select(select) => select,
        SetTree::Op { left, .. } => set_leaf(left),
    }
}

/// Pre-evaluates every (uncorrelated) subquery in the statement and stores
/// the results in the arena for hook-based lookup during evaluation.
#[allow(clippy::too_many_arguments)]
pub fn prepare_subqueries<'a>(
    exprs: &[Option<&'a Expr<'a>>],
    storage: &'a Storage,
    txid: u32,
    arena: &'a Arena,
    params: &[Datum<'a>],
    depth: u32,
    outer: Option<&dyn ColumnLookup<'a>>,
    sequences: Option<&dyn crate::sql::eval::SequenceAccess>,
) -> Result<SubqueryValues<'a, 'a>, SqlError> {
    let mut nodes = ArenaList::new(arena);
    for expression in exprs.iter().flatten() {
        collect_subqueries(expression, &mut nodes)?;
    }
    eval_subquery_nodes(
        nodes.as_slice(),
        storage,
        txid,
        arena,
        params,
        depth,
        outer,
        sequences,
        SubqueryTiming::WhereReached,
    )
}

/// Evaluates a set of already-collected subquery nodes (scalar, IN, or
/// EXISTS) into arena-backed [`SubqueryValues`] keyed by node identity.
/// EXISTS results are stored as boolean scalars.
#[allow(clippy::too_many_arguments)]
fn eval_subquery_nodes<'a>(
    nodes: &[&Expr<'a>],
    storage: &'a Storage,
    txid: u32,
    arena: &'a Arena,
    params: &[Datum<'a>],
    depth: u32,
    outer: Option<&dyn ColumnLookup<'a>>,
    sequences: Option<&dyn crate::sql::eval::SequenceAccess>,
    timing: SubqueryTiming,
) -> Result<SubqueryValues<'a, 'a>, SqlError> {
    let defers = |node: &Expr<'_>| {
        matches!(timing, SubqueryTiming::WhereReached) && subquery_has_effects(node, storage, txid)
    };
    let n_list_nodes = nodes
        .iter()
        .filter(|node| {
            matches!(
                node,
                Expr::InSubquery { .. } | Expr::QuantifiedSubquery { .. }
            ) && !defers(node)
        })
        .count();
    let n_deferred = nodes.iter().filter(|node| defers(node)).count();
    let scalars: &mut [(*const Expr<'a>, Datum<'a>, Datum<'a>)] = arena
        .alloc_slice_with(nodes.len() - n_list_nodes - n_deferred, |_| {
            (core::ptr::null(), Datum::Null, Datum::Null)
        })
        .map_err(|_| arena_full())?;
    let lists: &mut [SubqueryList<'a>] = arena
        .alloc_slice_with(n_list_nodes, |_| empty_subquery_list())
        .map_err(|_| arena_full())?;
    let deferred = if n_deferred == 0 {
        &mut []
    } else {
        let executor: &'a dyn super::super::eval::SubqueryExecution = arena
            .alloc(ProceduralSubqueries { storage, txid })
            .map_err(|_| arena_full())?;
        let mut entries = nodes.iter().filter(|node| defers(node));
        let mut failed = false;
        let deferred = arena
            .alloc_slice_with(n_deferred, |_| {
                let node = entries.next().copied();
                let cache = match arena.alloc(super::super::eval::DeferredSubqueryCache::default())
                {
                    Ok(cache) => {
                        let cache: &core::cell::Cell<_> = core::cell::Cell::from_mut(cache);
                        Some(cache)
                    }
                    Err(_) => {
                        failed = true;
                        None
                    }
                };
                super::super::eval::DeferredSubquery {
                    node: node.map_or(core::ptr::null(), |node| (node as *const Expr).cast()),
                    executor,
                    owner: arena,
                    cache,
                }
            })
            .map_err(|_| arena_full())?;
        if failed {
            return Err(arena_full());
        }
        deferred
    };
    let (mut n_scalars, mut n_lists) = (0, 0);
    for node in nodes {
        if defers(node) {
            continue;
        }
        let mut evaluate = || -> Result<(), SqlError> {
            match node {
                Expr::RowSubquery { select, arity } => {
                    let (values, _, witness) = run_row_subquery(
                        select,
                        storage,
                        txid,
                        arena,
                        params,
                        depth,
                        outer,
                        *arity as usize,
                        sequences,
                    )?;
                    if values.len() > 1 {
                        return Err(sql_err!(
                            crate::sql::eval::sqlstate::CARDINALITY_VIOLATION,
                            "more than one row returned by a subquery used as an expression"
                        ));
                    }
                    scalars[n_scalars] = (
                        *node as *const _,
                        values.first().copied().unwrap_or(Datum::Null),
                        witness,
                    );
                    n_scalars += 1;
                }
                Expr::Subquery(select) => {
                    if storage.spill_attached() {
                        let (value, witness) = streaming_scalar_subquery(
                            select, storage, txid, arena, params, depth, outer, sequences,
                        )?;
                        scalars[n_scalars] = (*node as *const _, value, witness);
                        n_scalars += 1;
                        return Ok(());
                    }
                    let (values, _, witness) = run_subquery(
                        select, storage, txid, arena, params, depth, outer, 1, sequences,
                    )?;
                    if values.len() > 1 {
                        return Err(sql_err!(
                            crate::sql::eval::sqlstate::CARDINALITY_VIOLATION,
                            "more than one row returned by a subquery used as an expression"
                        ));
                    }
                    let v = values.first().copied().unwrap_or(Datum::Null);
                    scalars[n_scalars] = (*node as *const _, v, witness);
                    n_scalars += 1;
                }
                Expr::Exists(select) => {
                    let found = subquery_exists(
                        select, storage, txid, arena, params, depth, outer, sequences,
                    )?;
                    scalars[n_scalars] =
                        (*node as *const _, Datum::Bool(found), Datum::Bool(false));
                    n_scalars += 1;
                }
                Expr::ArraySubquery(select) => {
                    if storage.spill_attached() {
                        let (v, witness) = streaming_array_subquery(
                            select, storage, txid, arena, params, depth, outer, sequences,
                        )?;
                        scalars[n_scalars] = (*node as *const _, v, witness);
                        n_scalars += 1;
                        return Ok(());
                    }
                    let plan = array_subquery_plan(select, storage, txid, arena, outer)?;
                    let (values, _, _) = run_subquery(
                        select, storage, txid, arena, params, depth, outer, 1, sequences,
                    )?;
                    let v = build_array_scalar(values, plan, storage, txid, arena)?;
                    scalars[n_scalars] = (*node as *const _, v, v);
                    n_scalars += 1;
                }
                Expr::InSubquery {
                    operand, select, ..
                }
                | Expr::QuantifiedSubquery {
                    operand, select, ..
                } => {
                    let original_select = *select;
                    let (select, arity) = row_projected(operand, original_select, arena)?;
                    let collations =
                        subquery_collations(original_select, storage, txid, arena, outer)?;
                    let row_witness = (arity > 1)
                        .then(|| row_subquery_witness(original_select, storage, txid, arena, outer))
                        .transpose()?;
                    if storage.spill_attached() && !matches!(timing, SubqueryTiming::Retained) {
                        lists[n_lists] = external_list_subquery(
                            node,
                            select,
                            storage,
                            txid,
                            arena,
                            params,
                            depth,
                            arity,
                            row_witness,
                            collations,
                            outer,
                            sequences,
                        )?;
                        n_lists += 1;
                        return Ok(());
                    }
                    let (values, saw_null, witness) = run_subquery(
                        select, storage, txid, arena, params, depth, outer, arity, sequences,
                    )?;
                    lists[n_lists] = SubqueryList {
                        node: (*node as *const Expr).cast(),
                        values,
                        probe: None,
                        saw_null,
                        witness: row_witness.unwrap_or(witness),
                        collations,
                    };
                    n_lists += 1;
                }
                _ => unreachable!("collector only stores subquery nodes"),
            }
            Ok(())
        };
        evaluate().map_err(|error| subquery_shape_position(error, node))?;
    }
    debug_assert_eq!(n_scalars, scalars.len());
    debug_assert_eq!(n_lists, lists.len());
    Ok(SubqueryValues {
        scalars,
        lists,
        deferred,
    })
}

/// PostgreSQL reports a subquery's column count at the subquery.
pub(super) fn subquery_shape_position(error: SqlError, node: &Expr<'_>) -> SqlError {
    if matches!(
        error.message.as_str(),
        "subquery must return only one column"
            | "subquery has too many columns"
            | "subquery has too few columns"
    ) {
        crate::sql::error_position::at_node(error, node)
    } else {
        error
    }
}

/// When prepared subqueries run.
#[derive(Clone, Copy)]
enum SubqueryTiming {
    /// Before the expressions that contain them.
    Now,
    /// Now, gathering a list in memory so its result can be saved for
    /// later rows instead of spooled.
    Retained,
    /// One with effects runs where evaluation reaches it.
    WhereReached,
}

/// Whether running a scalar subquery early would reorder its effects: it
/// calls a volatile function, or its statement reads data-modifying `WITH`
/// queries, which run when first read. `IN` and quantified subqueries are
/// prepared before their expressions.
fn subquery_has_effects(node: &Expr<'_>, storage: &Storage, txid: u32) -> bool {
    let (Expr::Subquery(select)
    | Expr::RowSubquery { select, .. }
    | Expr::Exists(select)
    | Expr::ArraySubquery(select)
    | Expr::InSubquery { select, .. }
    | Expr::QuantifiedSubquery { select, .. }) = node
    else {
        return false;
    };
    super::statement_reads_write_queries()
        || super::cte::select_contains_volatile(select, storage, txid)
}

/// Runs a subquery only to determine whether it yields any row (EXISTS).
/// Stops at the first matching row. `outer` supplies correlated columns.
#[allow(clippy::too_many_arguments)]
fn subquery_exists<'a>(
    select: &'a Select<'a>,
    storage: &'a Storage,
    txid: u32,
    arena: &'a Arena,
    params: &[Datum<'a>],
    depth: u32,
    outer: Option<&dyn ColumnLookup<'a>>,
    sequences: Option<&dyn crate::sql::eval::SequenceAccess>,
) -> Result<bool, SqlError> {
    if depth == 0 {
        return Err(sql_err!(
            sqlstate::STATEMENT_TOO_COMPLEX,
            "subqueries nested too deeply"
        ));
    }
    if let Some(tree) = select.set_body {
        let (vals, _, _) = run_set_subquery(
            tree, select, storage, txid, arena, params, outer, 1, sequences,
        )?;
        return Ok(!vals.is_empty());
    }
    if !select.group_by.is_empty() || select.having.is_some() || select.distinct {
        // Grouped/DISTINCT EXISTS: the row-source executor already handles
        // grouping, HAVING, and DISTINCT — existence is whether it emits.
        let mut found = false;
        select_into_rows(
            storage,
            txid,
            select,
            arena,
            params,
            outer,
            sequences,
            &mut |_| {
                found = true;
                Ok(())
            },
        )?;
        return Ok(found);
    }
    // EXISTS consumes row presence. Preparing its discarded projection would
    // issue effects from scalar subqueries whose values are never requested.
    let sample_capacity = select
        .from
        .as_ref()
        .map_or(0, |from| 2 * (from.joins.len() + 1));
    let item_exprs = arena
        .alloc_slice_with(1 + sample_capacity, |_| Option::<&'a Expr<'a>>::None)
        .map_err(|_| arena_full())?;
    item_exprs[0] = select.where_clause;
    if let Some(from) = &select.from {
        collect_table_sample_expressions(from, &mut item_exprs[1..]);
    }
    let inner_subs = prepare_subqueries(
        item_exprs,
        storage,
        txid,
        arena,
        params,
        depth - 1,
        outer,
        sequences,
    )?;
    let catalog = super::storage_catalog(storage, arena, txid);
    let hooks = EvalHooks {
        group: None,
        aggs: None,
        subs: Some(&inner_subs),
        subquery_executor: None,
        windows: None,
        catalog: Some(&catalog),
        srf_index: None,
        project_sets: None,
        sequences,
        merge_action: None,
    };

    let Some(from) = &select.from else {
        // FROM-less: an aggregate query yields its one output row even over
        // zero input rows (WHERE false), so EXISTS is true unless HAVING
        // filters it. A plain query yields one row when WHERE holds.
        let aggregate_columns = SubqueryColumns {
            scope: None,
            outer,
            catalog: crate::sql::exec::CatalogCols {
                definition: None,
                alias: None,
                output_aliases: &[],
                storage,
                txid,
                arena,
            },
        };
        let mut agg_nodes = ArenaList::new(arena);
        for item in select.items {
            if let SelectItem::Expr { expression, .. } = item {
                collect_aggs(
                    expression,
                    &mut agg_nodes,
                    storage,
                    txid,
                    &aggregate_columns,
                )?;
            }
        }
        if let Some(h) = select.having {
            collect_aggs(h, &mut agg_nodes, storage, txid, &aggregate_columns)?;
        }
        if !agg_nodes.is_empty() || select.having.is_some() {
            let base = Chained {
                inner: &crate::sql::eval::NoColumns,
                outer,
            };
            let hook_data = fromless_aggregate_hooks(
                select,
                agg_nodes.as_slice(),
                storage,
                txid,
                arena,
                params,
                &base,
                &hooks,
            )?;
            return Ok(hook_data.is_some());
        }
        if let Some(w) = select.where_clause {
            let base = Chained {
                inner: &crate::sql::eval::NoColumns,
                outer,
            };
            return Ok(matches!(
                eval_full(w, arena, params, &base, &hooks)?,
                Datum::Bool(true)
            ));
        }
        return Ok(true);
    };
    let scope = QueryScope::resolve_exec_outer(
        storage,
        from,
        select.where_clause,
        txid,
        arena,
        params,
        hooks.sequences,
        outer,
    )?;
    let mut found = false;
    let pax_columns = match select.where_clause {
        Some(predicate) => pax_column_demand(&scope, from, &[predicate]),
        None => pax_column_demand(&scope, from, &[]),
    };
    scan_source_with_pax_columns(
        storage,
        &scope,
        from,
        txid,
        select.where_clause,
        arena,
        params,
        &hooks,
        outer,
        pax_columns,
        &mut |_| {
            found = true;
            Ok(false) // stop at the first row
        },
    )?;
    Ok(found)
}

/// A chain of query scopes from innermost outward, used to decide whether a
/// subquery references a column belonging to an enclosing query.
struct ScopeChain<'s, 'd> {
    scope: Option<&'s QueryScope<'d>>,
    parent: Option<&'s ScopeChain<'s, 'd>>,
}

impl ScopeChain<'_, '_> {
    /// True if the name resolves at this scope or any enclosing scope.
    fn resolves(&self, qualifier: Option<&str>, name: &str) -> bool {
        if self
            .scope
            .is_some_and(|scope| scope.find_column(qualifier, name).is_ok())
        {
            return true;
        }
        self.parent
            .is_some_and(|parent| parent.resolves(qualifier, name))
    }
}

fn set_tree_has_outer_ref<'a>(
    tree: &'a SetTree<'a>,
    chain: &ScopeChain<'_, 'a>,
    storage: &'a Storage,
    txid: u32,
    arena: &'a Arena,
) -> Result<bool, SqlError> {
    match tree {
        SetTree::Select(select) => {
            let scope = select
                .from
                .as_ref()
                .map(|from| correlation_schema(storage, txid, from, arena))
                .transpose()?
                .flatten();
            let child = ScopeChain {
                scope: scope.as_ref(),
                parent: Some(chain),
            };
            select_has_outer_ref(select, &child, storage, txid, arena)
        }
        SetTree::Op { left, right, .. } => {
            Ok(set_tree_has_outer_ref(left, chain, storage, txid, arena)?
                || set_tree_has_outer_ref(right, chain, storage, txid, arena)?)
        }
    }
}

/// Whether a top-level subquery node references any column from the enclosing
/// query — i.e. is correlated and must be re-evaluated per outer row.
fn subquery_node_correlated<'a>(
    node: &'a Expr<'a>,
    storage: &'a Storage,
    txid: u32,
    arena: &'a Arena,
) -> Result<bool, SqlError> {
    let select = match node {
        Expr::Subquery(s)
        | Expr::RowSubquery { select: s, .. }
        | Expr::InSubquery { select: s, .. }
        | Expr::QuantifiedSubquery { select: s, .. }
        | Expr::Exists(s)
        | Expr::ArraySubquery(s) => s,
        _ => return Ok(false),
    };
    select_correlated(select, storage, txid, arena)
}

/// Whether `select` references a column of a query level around it.
fn select_correlated<'a>(
    select: &'a Select<'a>,
    storage: &'a Storage,
    txid: u32,
    arena: &'a Arena,
) -> Result<bool, SqlError> {
    let scope = select
        .from
        .as_ref()
        .map(|from| correlation_schema(storage, txid, from, arena))
        .transpose()?
        .flatten();
    let chain = ScopeChain {
        scope: scope.as_ref(),
        parent: None,
    };
    select_has_outer_ref(select, &chain, storage, txid, arena)
}

pub(crate) fn expression_has_correlated_subquery<'a>(
    expression: &'a Expr<'a>,
    storage: &'a Storage,
    txid: u32,
    arena: &'a Arena,
) -> Result<bool, SqlError> {
    let mut nodes = ArenaList::new(arena);
    collect_subqueries(expression, &mut nodes)?;
    for &node in nodes.as_slice() {
        if subquery_node_correlated(node, storage, txid, arena)? {
            return Ok(true);
        }
    }
    Ok(false)
}

/// A scope that needs an enclosing row cannot be described as a stand-alone
/// schema. Preserve that state explicitly so correlation analysis can inspect
/// its outer references in the statement's catalog snapshot.
fn correlation_schema<'a>(
    storage: &'a Storage,
    txid: u32,
    from: &'a crate::sql::ast::FromClause<'a>,
    arena: &'a Arena,
) -> Result<Option<QueryScope<'a>>, SqlError> {
    // A function source can name an outer column only through its
    // arguments; one whose arguments read no column resolves on its own.
    fn reads_columns(expression: &Expr<'_>) -> bool {
        if matches!(
            expression,
            Expr::Column { .. }
                | Expr::WholeRow(_)
                | Expr::SchemaColumn { .. }
                | Expr::Subquery(_)
                | Expr::RowSubquery { .. }
                | Expr::InSubquery { .. }
                | Expr::QuantifiedSubquery { .. }
                | Expr::Exists(_)
                | Expr::ArraySubquery(_)
        ) {
            return true;
        }
        let mut found = false;
        let _ = walk_children(expression, &mut |child| {
            found |= reads_columns(child);
            Ok(())
        });
        found
    }
    let function_reads_columns = |table: &crate::sql::ast::TableRef<'_>| {
        table
            .func_args
            .is_some_and(|arguments| arguments.iter().any(|argument| reads_columns(argument)))
            || table.rows_from.is_some_and(|functions| {
                functions.iter().any(|function| {
                    function.func_args.is_some_and(|arguments| {
                        arguments.iter().any(|argument| reads_columns(argument))
                    })
                })
            })
    };
    // A derived table may read the levels around its query, though not the
    // tables beside it; its columns then exist only per outer row.
    let references_outer = |table: &'a crate::sql::ast::TableRef<'a>| {
        if table.lateral || (table.is_function_source() && function_reads_columns(table)) {
            return Ok(true);
        }
        match table.subquery {
            Some(select) => select_correlated(select, storage, txid, arena),
            None => Ok(false),
        }
    };
    if references_outer(&from.base)? {
        return Ok(None);
    }
    for join in from.joins {
        if references_outer(&join.table)? {
            return Ok(None);
        }
    }
    QueryScope::resolve_schema(storage, from, txid, arena, None).map(Some)
}

/// Whether any column in this select (WHERE or projection) fails to resolve
/// within `chain` (which already includes this select's own scope).
fn select_has_outer_ref<'a>(
    select: &'a Select<'a>,
    chain: &ScopeChain<'_, 'a>,
    storage: &'a Storage,
    txid: u32,
    arena: &'a Arena,
) -> Result<bool, SqlError> {
    // Each branch of a set operation is a query level of its own.
    if let Some(tree) = select.set_body {
        return set_tree_has_outer_ref(tree, chain, storage, txid, arena);
    }
    if let Some(from) = select.from.as_ref() {
        if table_ref_has_outer_ref(&from.base, chain, storage, txid, arena)? {
            return Ok(true);
        }
        for join in from.joins {
            if table_ref_has_outer_ref(&join.table, chain, storage, txid, arena)?
                || join
                    .on
                    .map(|on| expr_has_outer_ref(on, chain, storage, txid, arena))
                    .transpose()?
                    .unwrap_or(false)
            {
                return Ok(true);
            }
        }
    }
    if let Some(predicate) = select.where_clause
        && expr_has_outer_ref(predicate, chain, storage, txid, arena)?
    {
        return Ok(true);
    }
    if let Some(predicate) = select.having
        && expr_has_outer_ref(predicate, chain, storage, txid, arena)?
    {
        return Ok(true);
    }
    for expression in select.group_by {
        if expr_has_outer_ref(expression, chain, storage, txid, arena)? {
            return Ok(true);
        }
    }
    for order in select.order_by {
        if expr_has_outer_ref(order.expression, chain, storage, txid, arena)? {
            return Ok(true);
        }
    }
    for item in select.items {
        if match item {
            SelectItem::Expr { expression, .. } => {
                expr_has_outer_ref(expression, chain, storage, txid, arena)?
            }
            _ => false,
        } {
            return Ok(true);
        }
    }
    Ok(false)
}

/// Whether a FROM item depends on a column beyond the select's own scope.
/// Function arguments participate because table functions are implicitly
/// lateral; a derived subquery pushes its own scope just like an expression
/// subquery does.
fn table_ref_has_outer_ref<'a>(
    table: &'a crate::sql::ast::TableRef<'a>,
    chain: &ScopeChain<'_, 'a>,
    storage: &'a Storage,
    txid: u32,
    arena: &'a Arena,
) -> Result<bool, SqlError> {
    if let Some(sample) = table.sample {
        if expr_has_outer_ref(sample.percentage, chain, storage, txid, arena)? {
            return Ok(true);
        }
        if let Some(repeatable) = sample.repeatable
            && expr_has_outer_ref(repeatable, chain, storage, txid, arena)?
        {
            return Ok(true);
        }
    }
    if let Some(arguments) = table.func_args {
        for argument in arguments {
            if expr_has_outer_ref(argument, chain, storage, txid, arena)? {
                return Ok(true);
            }
        }
    }
    if let Some(functions) = table.rows_from {
        for function in functions {
            if table_ref_has_outer_ref(function, chain, storage, txid, arena)? {
                return Ok(true);
            }
        }
    }
    let Some(select) = table.subquery else {
        return Ok(false);
    };
    let scope = select
        .from
        .as_ref()
        .map(|from| correlation_schema(storage, txid, from, arena))
        .transpose()?
        .flatten();
    let child = ScopeChain {
        scope: scope.as_ref(),
        parent: Some(chain),
    };
    select_has_outer_ref(select, &child, storage, txid, arena)
}

/// Whether any column reference in `expression` resolves only in an enclosing scope
/// beyond `chain`. Nested subqueries push their own scope onto the chain, so a
/// column they provide themselves does not count as an outer reference.
fn expr_has_outer_ref<'a>(
    expression: &'a Expr<'a>,
    chain: &ScopeChain<'_, 'a>,
    storage: &'a Storage,
    txid: u32,
    arena: &'a Arena,
) -> Result<bool, SqlError> {
    match expression {
        Expr::Column { qualifier, name } => Ok(!chain.resolves(*qualifier, name)),
        Expr::SchemaColumn {
            schema,
            table,
            name,
        } => {
            let qualifier = crate::stack_format!(130, "{}.{}", schema, table);
            Ok(!chain.resolves(Some(qualifier.as_str()), name))
        }
        Expr::Subquery(s)
        | Expr::RowSubquery { select: s, .. }
        | Expr::Exists(s)
        | Expr::ArraySubquery(s) => {
            let sscope = s
                .from
                .as_ref()
                .map(|from| correlation_schema(storage, txid, from, arena))
                .transpose()?
                .flatten();
            let child = ScopeChain {
                scope: sscope.as_ref(),
                parent: Some(chain),
            };
            select_has_outer_ref(s, &child, storage, txid, arena)
        }
        Expr::InSubquery {
            operand, select, ..
        }
        | Expr::QuantifiedSubquery {
            operand, select, ..
        } => {
            let sscope = select
                .from
                .as_ref()
                .map(|from| correlation_schema(storage, txid, from, arena))
                .transpose()?
                .flatten();
            let child = ScopeChain {
                scope: sscope.as_ref(),
                parent: Some(chain),
            };
            Ok(select_has_outer_ref(select, &child, storage, txid, arena)?
                || expr_has_outer_ref(operand, chain, storage, txid, arena)?)
        }
        _ => {
            let mut found = false;
            walk_children(expression, &mut |child| {
                if expr_has_outer_ref(child, chain, storage, txid, arena)? {
                    found = true;
                }
                Ok(())
            })?;
            Ok(found)
        }
    }
}

/// Pre-evaluated uncorrelated subqueries plus the list of correlated subquery
/// nodes that must be re-evaluated per outer row.
pub(super) struct OuterSubs<'a> {
    pub(super) base: SubqueryValues<'a, 'a>,
    pub(super) correlated: &'a [&'a Expr<'a>],
}

/// Splits a query's subqueries into uncorrelated (evaluated once here) and
/// correlated (deferred to per-row evaluation during the scan).
pub(super) fn prepare_outer_subqueries<'a>(
    exprs: &[Option<&'a Expr<'a>>],
    storage: &'a Storage,
    txid: u32,
    arena: &'a Arena,
    params: &[Datum<'a>],
    sequences: Option<&dyn crate::sql::eval::SequenceAccess>,
) -> Result<OuterSubs<'a>, SqlError> {
    let mut nodes = ArenaList::new(arena);
    for expression in exprs.iter().flatten() {
        collect_subqueries(expression, &mut nodes)?;
    }
    let mut uncorr = ArenaList::new(arena);
    let mut corr = ArenaList::new(arena);
    for &node in nodes.as_slice() {
        if subquery_node_correlated(node, storage, txid, arena)? {
            corr.push(node).map_err(|_| arena_full())?;
        } else {
            uncorr.push(node).map_err(|_| arena_full())?;
        }
    }
    let base = eval_subquery_nodes(
        uncorr.as_slice(),
        storage,
        txid,
        arena,
        params,
        SUBQUERY_DEPTH,
        None,
        sequences,
        SubqueryTiming::WhereReached,
    )?;
    Ok(OuterSubs {
        base,
        correlated: corr.as_slice(),
    })
}

/// The exact scratch sizes `merge_correlated` fills for `base` + `correlated`
/// (scalar slots, list slots). Callers allocate once per statement execution
/// and reuse the slices across rows, so per-row merges never grow the arena.
pub(super) fn merge_scratch_lens(
    base: Option<&SubqueryValues<'_, '_>>,
    correlated: &[&Expr<'_>],
) -> (usize, usize) {
    let Some(base) = base else {
        return (0, 0);
    };
    let list_nodes = correlated
        .iter()
        .filter(|node| {
            matches!(
                node,
                Expr::InSubquery { .. } | Expr::QuantifiedSubquery { .. }
            )
        })
        .count();
    (
        base.scalars.len() + correlated.len() - list_nodes,
        base.lists.len() + list_nodes,
    )
}

/// Per-row merge scratch [`merge_correlated`] fills: scalar and list slots
/// keyed by subquery node address.
pub(super) type ScalarSlots<'a> = [(*const Expr<'a>, Datum<'a>, Datum<'a>)];
pub(super) type ListSlots<'a> = [SubqueryList<'a>];

/// Allocates the reusable merge scratch [`merge_correlated`] fills, sized
/// exactly by [`merge_scratch_lens`]. Callers allocate once per statement (or
/// per grouping pass) and reuse the slices across rows, so per-row merges
/// never grow the arena.
pub(super) fn alloc_merge_scratch<'a>(
    base: Option<&SubqueryValues<'_, '_>>,
    correlated: &[&Expr<'_>],
    arena: &'a Arena,
) -> Result<(&'a mut ScalarSlots<'a>, &'a mut ListSlots<'a>), SqlError> {
    let (n_scalars, n_lists) = merge_scratch_lens(base, correlated);
    let scalars = arena
        .alloc_slice_with(n_scalars, |_| (core::ptr::null(), Datum::Null, Datum::Null))
        .map_err(|_| arena_full())?;
    let lists = arena
        .alloc_slice_with(n_lists, |_| empty_subquery_list())
        .map_err(|_| arena_full())?;
    Ok((scalars, lists))
}

/// Builds per-outer-row [`SubqueryValues`] by merging the pre-evaluated
/// uncorrelated results with correlated subqueries evaluated against `outer`.
/// The merged arrays live in caller-provided scratch sized by
/// [`merge_scratch_lens`] (no arena growth for the bookkeeping; only the
/// subquery result values themselves use the arena).
#[allow(clippy::too_many_arguments)]
#[expect(
    clippy::unnecessary_cast,
    reason = "the base values' node addresses were narrowed by covariance; the cast restores the statement-arena lifetime rustc requires"
)]
pub(super) fn merge_correlated<'a, 'b>(
    correlated: &[&'a Expr<'a>],
    base: &SubqueryValues<'b, 'a>,
    row: &dyn ColumnLookup<'a>,
    enclosing: Option<&dyn ColumnLookup<'a>>,
    storage: &'a Storage,
    txid: u32,
    arena: &'a Arena,
    params: &[Datum<'a>],
    scalars: &'b mut [(*const Expr<'a>, Datum<'a>, Datum<'a>)],
    lists: &'b mut [SubqueryList<'a>],
    sequences: Option<&dyn crate::sql::eval::SequenceAccess>,
) -> Result<SubqueryValues<'b, 'a>, SqlError> {
    // A correlated subquery reads the current row and every level around
    // its query, such as a routine's variables.
    let chained = super::Chained {
        inner: row,
        outer: enclosing,
    };
    let outer: &dyn ColumnLookup<'a> = &chained;
    let mut ns = 0;
    for (p, v, w) in base.scalars {
        // Node addresses are identity keys; every base value was built from
        // this statement's arena, so the widened pointer is the same address.
        scalars[ns] = (*p as *const Expr<'a>, *v, *w);
        ns += 1;
    }
    let mut nl = 0;
    for list in base.lists {
        lists[nl] = *list;
        nl += 1;
    }
    for node in correlated {
        if subquery_has_effects(node, storage, txid) {
            continue;
        }
        match node {
            Expr::RowSubquery { select, arity } => {
                let (values, _, witness) = run_row_subquery(
                    select,
                    storage,
                    txid,
                    arena,
                    params,
                    SUBQUERY_DEPTH,
                    Some(outer),
                    *arity as usize,
                    sequences,
                )?;
                if values.len() > 1 {
                    return Err(sql_err!(
                        crate::sql::eval::sqlstate::CARDINALITY_VIOLATION,
                        "more than one row returned by a subquery used as an expression"
                    ));
                }
                scalars[ns] = (
                    *node as *const _,
                    values.first().copied().unwrap_or(Datum::Null),
                    witness,
                );
                ns += 1;
            }
            Expr::Subquery(select) => {
                if storage.spill_attached() {
                    let (value, witness) = streaming_scalar_subquery(
                        select,
                        storage,
                        txid,
                        arena,
                        params,
                        SUBQUERY_DEPTH,
                        Some(outer),
                        sequences,
                    )?;
                    scalars[ns] = (*node as *const _, value, witness);
                    ns += 1;
                    continue;
                }
                let (values, _, witness) = run_subquery(
                    select,
                    storage,
                    txid,
                    arena,
                    params,
                    SUBQUERY_DEPTH,
                    Some(outer),
                    1,
                    sequences,
                )?;
                if values.len() > 1 {
                    return Err(sql_err!(
                        crate::sql::eval::sqlstate::CARDINALITY_VIOLATION,
                        "more than one row returned by a subquery used as an expression"
                    ));
                }
                scalars[ns] = (
                    *node as *const _,
                    values.first().copied().unwrap_or(Datum::Null),
                    witness,
                );
                ns += 1;
            }
            Expr::Exists(select) => {
                let found = subquery_exists(
                    select,
                    storage,
                    txid,
                    arena,
                    params,
                    SUBQUERY_DEPTH,
                    Some(outer),
                    sequences,
                )?;
                scalars[ns] = (*node as *const _, Datum::Bool(found), Datum::Bool(false));
                ns += 1;
            }
            Expr::ArraySubquery(select) => {
                if storage.spill_attached() {
                    let (v, witness) = streaming_array_subquery(
                        select,
                        storage,
                        txid,
                        arena,
                        params,
                        SUBQUERY_DEPTH,
                        Some(outer),
                        sequences,
                    )?;
                    scalars[ns] = (*node as *const _, v, witness);
                    ns += 1;
                    continue;
                }
                let plan = array_subquery_plan(select, storage, txid, arena, Some(outer))?;
                let (values, _, _) = run_subquery(
                    select,
                    storage,
                    txid,
                    arena,
                    params,
                    SUBQUERY_DEPTH,
                    Some(outer),
                    1,
                    sequences,
                )?;
                let v = build_array_scalar(values, plan, storage, txid, arena)?;
                scalars[ns] = (*node as *const _, v, v);
                ns += 1;
            }
            Expr::InSubquery {
                operand, select, ..
            }
            | Expr::QuantifiedSubquery {
                operand, select, ..
            } => {
                let original_select = *select;
                let (select, arity) = row_projected(operand, original_select, arena)?;
                let collations =
                    subquery_collations(original_select, storage, txid, arena, Some(outer))?;
                let row_witness = (arity > 1)
                    .then(|| {
                        row_subquery_witness(original_select, storage, txid, arena, Some(outer))
                    })
                    .transpose()?;
                if storage.spill_attached() {
                    lists[nl] = external_list_subquery(
                        node,
                        select,
                        storage,
                        txid,
                        arena,
                        params,
                        SUBQUERY_DEPTH,
                        arity,
                        row_witness,
                        collations,
                        Some(outer),
                        sequences,
                    )?;
                    nl += 1;
                    continue;
                }
                let (values, saw_null, witness) = run_subquery(
                    select,
                    storage,
                    txid,
                    arena,
                    params,
                    SUBQUERY_DEPTH,
                    Some(outer),
                    arity,
                    sequences,
                )?;
                lists[nl] = SubqueryList {
                    node: (*node as *const Expr).cast(),
                    values,
                    probe: None,
                    saw_null,
                    witness: row_witness.unwrap_or(witness),
                    collations,
                };
                nl += 1;
            }
            _ => unreachable!("correlated list holds only subquery nodes"),
        }
    }
    let effects = correlated
        .iter()
        .filter(|node| subquery_has_effects(node, storage, txid))
        .count();
    let deferred: &'b [super::super::eval::DeferredSubquery<'b>] = if effects == 0 {
        base.deferred
    } else {
        let executor: &'a dyn super::super::eval::SubqueryExecution = arena
            .alloc(ProceduralSubqueries { storage, txid })
            .map_err(|_| arena_full())?;
        let mut correlated_effects = correlated
            .iter()
            .filter(|node| subquery_has_effects(node, storage, txid));
        // Each outer row runs its correlated subqueries afresh.
        &*arena
            .alloc_slice_with(base.deferred.len() + effects, |index| {
                base.deferred.get(index).copied().unwrap_or_else(|| {
                    super::super::eval::DeferredSubquery {
                        node: correlated_effects
                            .next()
                            .map_or(core::ptr::null(), |node| (*node as *const Expr).cast()),
                        executor,
                        owner: arena,
                        cache: None,
                    }
                })
            })
            .map_err(|_| arena_full())?
    };
    Ok(SubqueryValues {
        scalars: &scalars[..ns],
        lists: &lists[..nl],
        deferred,
    })
}

/// Evaluates a predicate after materializing only the correlated subqueries it
/// contains. Keeping this at the subquery choke point makes every scan path
/// apply WHERE before it evaluates correlated projection or ordering work.
#[allow(clippy::too_many_arguments)]
pub(super) fn correlated_where_passes<'a>(
    correlated: &[&'a Expr<'a>],
    base: &SubqueryValues<'a, 'a>,
    predicate: Option<&'a Expr<'a>>,
    row: &dyn ColumnLookup<'a>,
    enclosing: Option<&dyn ColumnLookup<'a>>,
    storage: &'a Storage,
    txid: u32,
    arena: &'a Arena,
    params: &[Datum<'a>],
    hooks: &EvalHooks<'_, 'a>,
    scalars: &mut [(*const Expr<'a>, Datum<'a>, Datum<'a>)],
    lists: &mut [SubqueryList<'a>],
) -> Result<bool, SqlError> {
    let Some(predicate) = predicate else {
        return Ok(true);
    };
    let values = merge_correlated(
        correlated,
        base,
        row,
        enclosing,
        storage,
        txid,
        arena,
        params,
        scalars,
        lists,
        hooks.sequences,
    )?;
    let where_hooks = EvalHooks {
        subs: Some(&values),
        ..*hooks
    };
    let row = super::Chained {
        inner: row,
        outer: enclosing,
    };
    where_passes(predicate, arena, params, &row, &where_hooks)
}

/// A representative zero value of a column type, used to coerce an IN operand
/// to the subquery's result type even over an empty or all-NULL set. Text /
/// bytea / numeric use a text witness, which `coerce_unknown` leaves untouched
/// (no spurious error), matching that these accept an unknown literal as-is.
pub(crate) fn type_witness(ct: ColType) -> Result<Datum<'static>, SqlError> {
    Ok(match ct {
        ColType::Void | ColType::Internal | ColType::Pseudo(_) => Datum::Null,
        ColType::PgDdlCommand => Datum::PgDdlCommand,
        ColType::AnyArray => Datum::Null,
        // An empty record: enough for coerce_unknown to leave values alone.
        ColType::Record => Datum::Record(&[]),
        // A named composite is not an anonymous record: even an empty
        // subquery must retain the catalog slot so ARRAY(subquery) chooses
        // the named composite array OID.
        ColType::Composite(slot) => Datum::Composite {
            slot: crate::storage::TypeCreation::require(slot)?,
            fields: &[],
        },
        ColType::Bool => Datum::Bool(false),
        ColType::Int2 | ColType::Int4 => Datum::Int4(0),
        ColType::PgNodeTree => Datum::PgNodeTree(crate::sql::catalog_node::CatalogNode::NULL),
        ColType::PgNdistinct
        | ColType::PgDependencies
        | ColType::PgMcvList
        | ColType::PgStatisticArray
        | ColType::AclItem => Datum::Text(""),
        ColType::Oid | ColType::Xid => Datum::Oid(0),
        ColType::Xid8 => Datum::Xid8(0),
        ColType::PgSnapshot | ColType::TxidSnapshot => Datum::Snapshot {
            value: crate::sql::snapshot::WITNESS,
            legacy: ct == ColType::TxidSnapshot,
        },
        ColType::PgLsn => Datum::PgLsn(0),
        ColType::Money => Datum::Money(0),
        ColType::Tid => Datum::Tid(crate::sql::types::Tid {
            block: 0,
            offset: 0,
        }),
        ColType::Cid => Datum::Cid(0),
        ColType::Regtype => Datum::Regtype {
            referenced_oid: 0,
            name: "-",
        },
        ColType::Regproc
        | ColType::Regprocedure
        | ColType::Regoper
        | ColType::Regoperator
        | ColType::Regclass
        | ColType::Regnamespace
        | ColType::Regrole
        | ColType::Regconfig
        | ColType::Regdictionary
        | ColType::Regcollation => Datum::RegObject {
            type_oid: ct.oid(),
            referenced_oid: 0,
            name: "-",
        },
        ColType::Int8 => Datum::Int8(0),
        ColType::Time => Datum::Time(0),
        ColType::Timetz => Datum::Timetz(0, 0),
        ColType::Interval => Datum::Interval(crate::sql::types::Interval {
            months: 0,
            days: 0,
            micros: 0,
        }),
        ColType::Json => Datum::Json {
            text: "null",
            jsonb: false,
        },
        ColType::Jsonb => Datum::Json {
            text: "null",
            jsonb: true,
        },
        ColType::Xml => Datum::Xml(""),
        ColType::Jsonpath => Datum::JsonPath("$"),
        ColType::TsVector => Datum::TsVector(crate::sql::full_text::restore_vector("")),
        ColType::TsQuery => Datum::TsQuery(crate::sql::full_text::restore_query("")),
        ColType::Array(element) => Datum::Array {
            element,
            raw: crate::sql::array::EMPTY,
        },
        ColType::Int2Vector => Datum::Int2Vector {
            lower_bound: 0,
            raw: &[],
        },
        ColType::OidVector => Datum::OidVector {
            lower_bound: 0,
            raw: &[],
        },
        ColType::Float4 => Datum::Float4(0.0),
        ColType::Float8 => Datum::Float8(0.0),
        ColType::Date => Datum::Date(0),
        ColType::Timestamp => Datum::Timestamp(0),
        ColType::Timestamptz => Datum::Timestamptz(0),
        ColType::Uuid => Datum::Uuid([0; 16]),
        ColType::Char
        | ColType::Text
        | ColType::Refcursor
        | ColType::Varchar
        | ColType::Bpchar
        | ColType::Name
        | ColType::Bytea
        | ColType::Numeric => Datum::Text(""),
        ColType::Range(kind) => Datum::Range {
            text: "empty",
            kind,
        },
        ColType::Bit { varying } => Datum::Bit { bits: "", varying },
        ColType::Multirange(kind) => Datum::Multirange { text: "{}", kind },
        ColType::Inet => Datum::Inet(crate::sql::net::NetAddr::zero_v4()),
        ColType::Cidr => Datum::Cidr(crate::sql::net::NetAddr::zero_v4()),
        ColType::Macaddr => Datum::Macaddr([0; 6]),
        ColType::Macaddr8 => Datum::Macaddr8([0; 8]),
        ColType::Geometry(kind) => Datum::Geometry {
            kind,
            text: match kind {
                crate::sql::types::GeometryKind::Point => "(0,0)",
                crate::sql::types::GeometryKind::Lseg => "[(0,0),(0,0)]",
                crate::sql::types::GeometryKind::Path => "[(0,0)]",
                crate::sql::types::GeometryKind::Box => "(0,0),(0,0)",
                crate::sql::types::GeometryKind::Polygon => "((0,0),(0,0),(0,0))",
                crate::sql::types::GeometryKind::Line => "{0,0,0}",
                crate::sql::types::GeometryKind::Circle => "<(0,0),0>",
            },
        },
        // A witness carries only the type; the empty label is never compared or
        // output (an empty/all-NULL set produces no rows).
        ColType::Enum(slot) => Datum::Enum {
            slot: crate::storage::TypeCreation::require(slot)?,
            member: None,
            sort: 0.0,
            label: "",
        },
    })
}

/// The type witness for a subquery's single result column, inferred from its
/// projection expression. It is resolved through the same catalog boundary as
/// a query descriptor, because it determines empty-subquery coercion.
pub(crate) fn subquery_witness(
    storage: &Storage,
    txid: u32,
    item: &Expr,
    scope: Option<&QueryScope>,
    arena: &Arena,
) -> Result<Datum<'static>, SqlError> {
    subquery_witness_with_outer(storage, txid, item, scope, None, arena)
}

fn subquery_witness_with_outer(
    storage: &Storage,
    txid: u32,
    item: &Expr,
    scope: Option<&QueryScope>,
    outer: Option<&dyn ColumnLookup<'_>>,
    arena: &Arena,
) -> Result<Datum<'static>, SqlError> {
    let inferred = crate::sql::exec::infer_type_res(
        item,
        &SubqueryColumns {
            scope,
            outer,
            catalog: crate::sql::exec::CatalogCols {
                definition: None,
                alias: None,
                output_aliases: &[],
                storage,
                txid,
                arena,
            },
        },
    )?;
    if inferred.0 == crate::sql::types::oid::UNKNOWN {
        return type_witness(ColType::Text);
    }
    catalog_type_witness(storage, txid, inferred.0)
}

/// Query and procedural subqueries resolve inner columns before outer values,
/// while retaining catalog identities for domains, routines and nested queries.
pub(super) struct SubqueryColumns<'scope, 'definition, 'outer, 'row, 'catalog> {
    pub(super) scope: Option<&'scope QueryScope<'definition>>,
    pub(super) outer: Option<&'outer dyn ColumnLookup<'row>>,
    pub(super) catalog: crate::sql::exec::CatalogCols<'catalog>,
}

impl crate::sql::exec::ColTypeResolver for SubqueryColumns<'_, '_, '_, '_, '_> {
    /// A whole row of this level's tables, else of an enclosing level, such
    /// as a trigger's `NEW` around a statement's subquery.
    fn is_whole_row(&self, name: &str) -> bool {
        match self.scope {
            Some(scope) if scope.qualified_star_columns(name).is_ok() => true,
            None if self.catalog.is_whole_row(name) => true,
            _ => self
                .outer
                .is_some_and(|outer| outer.whole_row_type(name).is_some()),
        }
    }

    fn whole_row_field(
        &self,
        name: &str,
        index: usize,
    ) -> Option<(crate::util::StackStr<64>, crate::sql::exec::StaticTypeMeta)> {
        let own = match self.scope {
            Some(scope) => scope.qualified_star_columns(name).is_ok(),
            None => self.catalog.is_whole_row(name),
        };
        if own {
            return match self.scope {
                Some(scope) => CatalogScopeCols {
                    scope,
                    outer_scope: None,
                    storage: self.catalog.storage,
                    txid: self.catalog.txid,
                    arena: self.catalog.arena,
                }
                .whole_row_field(name, index),
                None => self.catalog.whole_row_field(name, index),
            };
        }
        let catalog =
            super::storage_catalog(self.catalog.storage, self.catalog.arena, self.catalog.txid);
        crate::sql::exec::ColTypeResolver::whole_row_field(
            &crate::sql::eval::RuntimeColumnTypes {
                row: self.outer?,
                catalog: Some(&catalog),
            },
            name,
            index,
        )
    }

    fn validate_enum_input_literal(&self, text: &str, target_oid: i32) -> Result<(), SqlError> {
        self.catalog.validate_enum_input_literal(text, target_oid)
    }

    fn is_empty_scope(&self) -> bool {
        self.scope.is_none_or(|scope| scope.n == 0)
            && self.outer.is_none_or(|outer| outer.is_empty_scope())
            && self.catalog.is_empty_scope()
    }

    fn type_name_fixed(&self, oid: i32) -> Result<Option<crate::util::StackStr<260>>, SqlError> {
        self.catalog.type_name_fixed(oid)
    }

    fn routine_call_selection(
        &self,
        name: &str,
        kind: crate::sql::ast::CallKind,
        bound_oid: Option<i32>,
        argument_names: &[Option<&str>],
        variadic: bool,
        arguments: &[i32],
    ) -> Option<crate::storage::RoutineCallSelection> {
        self.catalog.routine_call_selection(
            name,
            kind,
            bound_oid,
            argument_names,
            variadic,
            arguments,
        )
    }

    fn validate_explicit_cast(
        &self,
        source_oid: i32,
        target_oid: i32,
        target: ColType,
    ) -> Result<(), SqlError> {
        self.catalog
            .validate_explicit_cast(source_oid, target_oid, target)
    }

    fn visit_named_composite_fields(
        &self,
        name: &str,
        visit: &mut dyn FnMut(&str, crate::sql::exec::StaticTypeMeta),
    ) -> Result<Option<usize>, SqlError> {
        self.catalog.visit_named_composite_fields(name, visit)
    }

    fn routine_record_field(
        &self,
        name: &str,
        kind: crate::sql::ast::CallKind,
        bound_oid: Option<i32>,
        argument_names: &[Option<&str>],
        variadic: bool,
        arguments: &[i32],
        index: usize,
    ) -> Option<(crate::util::StackStr<64>, crate::sql::exec::StaticTypeMeta)> {
        self.catalog.routine_record_field(
            name,
            kind,
            bound_oid,
            argument_names,
            variadic,
            arguments,
            index,
        )
    }

    fn resolve(&self, qualifier: Option<&str>, name: &str) -> Result<ColType, SqlError> {
        let inner = match self.scope {
            Some(scope) => ScopeCols(scope).resolve(qualifier, name),
            None => self.catalog.resolve(qualifier, name),
        };
        match inner {
            Ok(ctype) => Ok(ctype),
            Err(error)
                if matches!(
                    error.sqlstate.as_str(),
                    sqlstate::UNDEFINED_COLUMN | sqlstate::UNDEFINED_TABLE
                ) =>
            {
                if qualifier.is_some_and(|qualifier| {
                    self.scope.map_or_else(
                        || self.catalog.is_whole_row(qualifier),
                        |scope| scope.qualified_star_columns(qualifier).is_ok(),
                    )
                }) {
                    return Err(error);
                }
                let outer = self.outer.ok_or(error)?;
                let catalog = super::storage_catalog(
                    self.catalog.storage,
                    self.catalog.arena,
                    self.catalog.txid,
                );
                crate::sql::exec::ColTypeResolver::resolve(
                    &crate::sql::eval::RuntimeColumnTypes {
                        row: outer,
                        catalog: Some(&catalog),
                    },
                    qualifier,
                    name,
                )
            }
            Err(error) => Err(error),
        }
    }

    fn column_meta(
        &self,
        qualifier: Option<&str>,
        name: &str,
    ) -> Option<crate::sql::exec::StaticTypeMeta> {
        if let Some(scope) = self.scope
            && scope.find_column(qualifier, name).is_err_and(|error| {
                !matches!(
                    error.sqlstate.as_str(),
                    sqlstate::UNDEFINED_COLUMN | sqlstate::UNDEFINED_TABLE
                )
            })
        {
            return None;
        }
        if let Some(scope) = self.scope
            && scope.find_column(qualifier, name).is_ok()
        {
            return CatalogScopeCols {
                scope,
                outer_scope: None,
                storage: self.catalog.storage,
                txid: self.catalog.txid,
                arena: self.catalog.arena,
            }
            .column_meta(qualifier, name);
        }
        if qualifier.is_some_and(|qualifier| {
            self.scope.map_or_else(
                || self.catalog.is_whole_row(qualifier),
                |scope| scope.qualified_star_columns(qualifier).is_ok(),
            )
        }) {
            return None;
        }
        let outer = self.outer?;
        let catalog =
            super::storage_catalog(self.catalog.storage, self.catalog.arena, self.catalog.txid);
        crate::sql::exec::ColTypeResolver::column_meta(
            &crate::sql::eval::RuntimeColumnTypes {
                row: outer,
                catalog: Some(&catalog),
            },
            qualifier,
            name,
        )
    }

    fn visit_composite_fields(
        &self,
        creation: Option<crate::storage::TypeCreation>,
        visit: &mut dyn FnMut(&str, crate::sql::exec::StaticTypeMeta),
    ) -> Option<usize> {
        self.catalog.visit_composite_fields(creation, visit)
    }

    fn domain_base_type(&self, type_oid: i32) -> Option<ColType> {
        self.catalog.domain_base_type(type_oid)
    }

    fn named_type_oid(&self, type_name: &str) -> Option<i32> {
        self.catalog.named_type_oid(type_name)
    }

    fn routine_result(
        &self,
        name: &str,
        kind: crate::sql::ast::CallKind,
        bound_oid: Option<i32>,
        argument_names: &[Option<&str>],
        variadic: bool,
        arguments: &[i32],
    ) -> Option<crate::sql::exec::StaticTypeMeta> {
        self.catalog
            .routine_result(name, kind, bound_oid, argument_names, variadic, arguments)
    }

    fn operator_result(
        &self,
        name: &str,
        left: i32,
        right: i32,
    ) -> Option<crate::sql::exec::StaticTypeMeta> {
        self.catalog.operator_result(name, left, right)
    }

    fn subquery_type(&self, expression: &Expr<'_>) -> Result<Option<(i32, i16)>, SqlError> {
        let schema = self.scope.map(ScopeSchema);
        let empty = crate::sql::eval::NoColumns;
        let inner: &dyn ColumnLookup<'_> = match &schema {
            Some(schema) => schema,
            None => &empty,
        };
        let outer = Chained {
            inner,
            outer: self.outer,
        };
        subquery_expression_type(
            expression,
            self.catalog.storage,
            self.catalog.txid,
            self.catalog.arena,
            &outer,
        )
        .map(|oid| oid.map(|oid| (oid, ColType::from_oid(oid).map_or(-1, ColType::typlen))))
    }

    fn whole_row_scalar_type(&self, name: &str) -> Option<ColType> {
        self.scope?.func_scalar_type(name)
    }

    fn whole_row_composite_type(&self, name: &str) -> Option<ColType> {
        self.scope?
            .whole_row_type(name)
            .filter(|ctype| matches!(ctype, ColType::Composite(_)))
    }

    fn table_columns(&self, name: &str) -> Option<&[crate::storage::ColumnMeta]> {
        let scope = self.scope?;
        let source = scope.source(name).ok()?;
        Some(scope.source_columns(source))
    }

    fn record_column_handle(&self, qualifier: Option<&str>, name: &str) -> Option<i32> {
        let metadata = self.column_meta(qualifier, name)?;
        (metadata.ctype == ColType::Record && metadata.type_mod >= 0).then_some(metadata.type_mod)
    }
}

pub(crate) fn subquery_expression_type(
    expression: &Expr<'_>,
    storage: &Storage,
    txid: u32,
    arena: &Arena,
    outer: &dyn ColumnLookup<'_>,
) -> Result<Option<i32>, SqlError> {
    let (select, array) = match expression {
        Expr::Subquery(select) => (select, false),
        Expr::ArraySubquery(select) => (select, true),
        // One column of a multiple-column assignment's sub-SELECT.
        Expr::RecordFieldIndex {
            base: Expr::RowSubquery { select, .. },
            index,
        } => {
            if select.set_body.is_some() {
                let mut type_oid = None;
                let mut column = 0;
                super::visit_projection_types(
                    select,
                    storage,
                    txid,
                    arena,
                    Some(outer),
                    &mut |source, _| {
                        if column == usize::from(*index) {
                            type_oid = Some(source);
                        }
                        column += 1;
                        Ok(())
                    },
                )?;
                return Ok(type_oid);
            }
            let Some(SelectItem::Expr { expression, .. }) = select.items.get(usize::from(*index))
            else {
                return Ok(None);
            };
            let scope = select
                .from
                .as_ref()
                .map(|from| QueryScope::resolve_schema(storage, from, txid, arena, Some(outer)))
                .transpose()?;
            let columns = SubqueryColumns {
                scope: scope.as_ref(),
                outer: Some(outer),
                catalog: crate::sql::exec::CatalogCols {
                    definition: None,
                    alias: None,
                    output_aliases: &[],
                    storage,
                    txid,
                    arena,
                },
            };
            let type_oid = crate::sql::exec::infer_routine_argument_oid(expression, &columns)?;
            return Ok(Some(if type_oid == crate::sql::types::oid::UNKNOWN {
                ColType::Text.oid()
            } else {
                type_oid
            }));
        }
        _ => return Ok(None),
    };
    if select.set_body.is_some() && !array {
        // Each branch may read the enclosing row, as a correlated subquery does.
        let mut type_oid = None;
        let mut columns = 0usize;
        super::visit_projection_types(
            select,
            storage,
            txid,
            arena,
            Some(outer),
            &mut |source, _| {
                if columns == 0 {
                    type_oid = Some(source);
                }
                columns += 1;
                Ok(())
            },
        )?;
        if columns != 1 {
            return Err(subquery_shape_position(
                sql_err!(
                    sqlstate::SYNTAX_ERROR,
                    "subquery must return only one column"
                ),
                expression,
            ));
        }
        return Ok(type_oid);
    }
    if select.set_body.is_some() {
        return super::subquery_result_type(expression, None, storage, txid, arena)
            .map(|result| result.map(|result| result.type_oid));
    }
    let scope = select
        .from
        .as_ref()
        .map(|from| QueryScope::resolve_schema(storage, from, txid, arena, Some(outer)))
        .transpose()?;
    let columns = SubqueryColumns {
        scope: scope.as_ref(),
        outer: Some(outer),
        catalog: crate::sql::exec::CatalogCols {
            definition: None,
            alias: None,
            output_aliases: &[],
            storage,
            txid,
            arena,
        },
    };
    let type_oid = match select.items {
        [SelectItem::Expr { expression, .. }] => {
            crate::sql::exec::infer_routine_argument_oid(expression, &columns)?
        }
        [SelectItem::Wildcard(_)]
            if scope
                .as_ref()
                .is_some_and(|scope| scope.star_columns() == 1) =>
        {
            let scope = scope.as_ref().unwrap();
            let entry = scope.star_entry(0);
            let user_type = match entry {
                crate::sql::query::scope::ResolvedColumn::Table(table, column) => {
                    scope.defs.get(table).expect("resolved source").columns[column].user_type
                }
                crate::sql::query::scope::ResolvedColumn::Merged(_) => None,
            };
            storage
                .routine_type_oid(scope.output_type(entry), user_type, txid)
                .ok_or_else(|| {
                    sql_err!(
                        sqlstate::INTERNAL_ERROR,
                        "subquery column has no declared type"
                    )
                })?
        }
        _ => {
            return super::subquery_result_type(expression, None, storage, txid, arena)
                .map(|result| result.map(|result| result.type_oid));
        }
    };
    let type_oid = if type_oid == crate::sql::types::oid::UNKNOWN {
        ColType::Text.oid()
    } else {
        type_oid
    };
    if !array {
        return Ok(Some(type_oid));
    }
    Ok(Some(
        array_element_plan(type_oid, storage, txid)?
            .element
            .array_oid(),
    ))
}

fn array_element_plan(
    type_oid: i32,
    storage: &Storage,
    txid: u32,
) -> Result<crate::sql::eval::ArrayConstructorType, SqlError> {
    let domain = matches!(
        crate::storage::NamedTypeKind::from_oid(type_oid),
        Some((crate::storage::NamedTypeKind::Domain, _, false))
    );
    let physical = crate::sql::eval::constructor_type_from_oid(type_oid, &|oid| {
        storage.domain_base_type_for_oid(oid, txid)
    });
    let element = if let Some((crate::storage::NamedTypeKind::Domain, creation, false)) =
        crate::storage::NamedTypeKind::from_oid(type_oid)
    {
        storage
            .domain_base_type_for_oid(type_oid, txid)
            .and_then(|base| crate::sql::types::ArrElem::domain(creation, base))
    } else {
        physical.and_then(|ctype| match ctype {
            ColType::Array(element) => Some(element),
            scalar => crate::sql::types::ArrElem::from_coltype(scalar),
        })
    }
    .ok_or_else(|| {
        sql_err!(
            sqlstate::FEATURE_NOT_SUPPORTED,
            "ARRAY subquery result type is not supported"
        )
    })?;
    Ok(crate::sql::eval::ArrayConstructorType {
        element,
        multidimensional: !domain && matches!(physical, Some(ColType::Array(_))),
    })
}

fn array_subquery_plan(
    select: &Select<'_>,
    storage: &Storage,
    txid: u32,
    arena: &Arena,
    outer: Option<&dyn ColumnLookup<'_>>,
) -> Result<crate::sql::eval::ArrayConstructorType, SqlError> {
    let empty = crate::sql::eval::NoColumns;
    let type_oid = subquery_expression_type(
        &Expr::Subquery(select),
        storage,
        txid,
        arena,
        outer.unwrap_or(&empty),
    )?
    .ok_or_else(|| {
        sql_err!(
            sqlstate::INTERNAL_ERROR,
            "array subquery has no declared column type"
        )
    })?;
    array_element_plan(type_oid, storage, txid)
}

fn catalog_type_witness(
    storage: &Storage,
    txid: u32,
    type_oid: i32,
) -> Result<Datum<'static>, SqlError> {
    crate::sql::exec::catalog_column_type(storage, txid, type_oid)
        .ok_or_else(|| {
            sql_err!(
                sqlstate::FEATURE_NOT_SUPPORTED,
                "subquery result type (oid {}) is not supported",
                type_oid
            )
        })
        .and_then(|(ctype, _)| type_witness(ctype))
}

#[allow(clippy::too_many_arguments)]
fn run_scalar_record_subquery<'a>(
    select: &'a Select<'a>,
    storage: &'a Storage,
    txid: u32,
    arena: &'a Arena,
    params: &[Datum<'a>],
    outer: Option<&dyn ColumnLookup<'a>>,
    row_arity: usize,
    sequences: Option<&dyn crate::sql::eval::SequenceAccess>,
) -> Result<(&'a [Datum<'a>], bool, Datum<'a>), SqlError> {
    let witness = row_subquery_witness(select, storage, txid, arena, outer)?;
    let Datum::Record(witness_fields) = witness else {
        unreachable!("row subquery witness is a record")
    };
    if witness_fields.len() < row_arity {
        return Err(sql_err!(
            sqlstate::SYNTAX_ERROR,
            "subquery has too few columns"
        ));
    }
    if witness_fields.len() > row_arity {
        return Err(sql_err!(
            sqlstate::SYNTAX_ERROR,
            "subquery has too many columns"
        ));
    }
    let mut value = None;
    select_into_rows(
        storage,
        txid,
        select,
        arena,
        params,
        outer,
        sequences,
        &mut |row| {
            if row.len() != row_arity {
                return Err(sql_err!(
                    sqlstate::SYNTAX_ERROR,
                    "subquery changed its projected column count"
                ));
            }
            if value.is_some() {
                return Err(sql_err!(
                    sqlstate::CARDINALITY_VIOLATION,
                    "more than one row returned by a subquery used as an expression"
                ));
            }
            let mut fields = [RecordField {
                name: "",
                type_oid: 0,
                value: Datum::Null,
            }; MAX_PROJ];
            for (column, field_value) in row.iter().copied().enumerate() {
                fields[column] = RecordField {
                    name: witness_fields[column].name,
                    type_oid: witness_fields[column].type_oid,
                    value: crate::sql::exec::detach_routine_datum(field_value, arena)?,
                };
            }
            let fields = arena
                .alloc_slice_copy(&fields[..row_arity])
                .map_err(|_| arena_full())?;
            value = Some(Datum::Record(fields));
            Ok(())
        },
    )?;
    let output = match value {
        Some(value) => arena.alloc_slice_copy(&[value]).map_err(|_| arena_full())?,
        None => &[][..],
    };
    Ok((output, false, witness))
}

#[allow(clippy::too_many_arguments)]
fn run_record_subquery<'a>(
    select: &'a Select<'a>,
    storage: &'a Storage,
    txid: u32,
    arena: &'a Arena,
    params: &[Datum<'a>],
    outer: Option<&dyn ColumnLookup<'a>>,
    row_arity: usize,
    sequences: Option<&dyn crate::sql::eval::SequenceAccess>,
) -> Result<(&'a [Datum<'a>], bool, Datum<'a>), SqlError> {
    let witness = row_subquery_witness(select, storage, txid, arena, outer)?;
    let Datum::Record(witness_fields) = witness else {
        unreachable!("row subquery witness is a record")
    };
    if witness_fields.len() < row_arity {
        return Err(sql_err!(
            sqlstate::SYNTAX_ERROR,
            "subquery has too few columns"
        ));
    }
    if witness_fields.len() > row_arity {
        return Err(sql_err!(
            sqlstate::SYNTAX_ERROR,
            "subquery has too many columns"
        ));
    }
    let mut output = ArenaList::new(arena);
    select_into_rows(
        storage,
        txid,
        select,
        arena,
        params,
        outer,
        sequences,
        &mut |values| {
            if values.len() != row_arity {
                return Err(sql_err!(
                    sqlstate::SYNTAX_ERROR,
                    "subquery changed its projected column count"
                ));
            }
            let mut fields = [RecordField {
                name: "",
                type_oid: 0,
                value: Datum::Null,
            }; MAX_PROJ];
            for (column, value) in values.iter().copied().enumerate() {
                fields[column] = RecordField {
                    name: witness_fields[column].name,
                    type_oid: witness_fields[column].type_oid,
                    value,
                };
            }
            let fields = arena
                .alloc_slice_copy(&fields[..row_arity])
                .map_err(|_| arena_full())?;
            let encoded =
                crate::sql::exec::encode_projected_pub(&[Datum::Record(&*fields)], arena)?;
            let value = crate::sql::exec::decode_projected_col_record(encoded, 0, arena)?;
            output.push(value).map_err(|_| arena_full())?;
            Ok(())
        },
    )?;
    Ok((output.as_slice(), false, witness))
}

#[allow(clippy::too_many_arguments)]
fn run_row_subquery<'a>(
    select: &'a Select<'a>,
    storage: &'a Storage,
    txid: u32,
    arena: &'a Arena,
    params: &[Datum<'a>],
    depth: u32,
    outer: Option<&dyn ColumnLookup<'a>>,
    row_arity: usize,
    sequences: Option<&dyn crate::sql::eval::SequenceAccess>,
) -> Result<(&'a [Datum<'a>], bool, Datum<'a>), SqlError> {
    if depth == 0 {
        return Err(sql_err!(
            sqlstate::STATEMENT_TOO_COMPLEX,
            "subqueries nested too deeply"
        ));
    }
    if let Some(tree) = select.set_body {
        let (values, saw_null, witness) = run_set_subquery(
            tree, select, storage, txid, arena, params, outer, row_arity, sequences,
        )?;
        if values.len() > 1 {
            return Err(sql_err!(
                sqlstate::CARDINALITY_VIOLATION,
                "more than one row returned by a subquery used as an expression"
            ));
        }
        return Ok((values, saw_null, witness));
    }
    run_scalar_record_subquery(
        select, storage, txid, arena, params, outer, row_arity, sequences,
    )
}

/// Executes a subquery to a value list: exactly one select item, full
/// WHERE/aggregate support, no grouping/ordering (irrelevant for IN, and a
/// scalar has at most one row). Also returns a type witness for the result
/// column (see [`type_witness`]); scalar callers ignore it.
#[allow(clippy::too_many_arguments)]
fn run_subquery<'a>(
    select: &'a Select<'a>,
    storage: &'a Storage,
    txid: u32,
    arena: &'a Arena,
    params: &[Datum<'a>],
    depth: u32,
    outer: Option<&dyn ColumnLookup<'a>>,
    row_arity: usize,
    sequences: Option<&dyn crate::sql::eval::SequenceAccess>,
) -> Result<(&'a [Datum<'a>], bool, Datum<'a>), SqlError> {
    if depth == 0 {
        return Err(sql_err!(
            sqlstate::STATEMENT_TOO_COMPLEX,
            "subqueries nested too deeply"
        ));
    }
    if let Some(tree) = select.set_body {
        return run_set_subquery(
            tree, select, storage, txid, arena, params, outer, row_arity, sequences,
        );
    }
    if row_arity > 1 {
        return run_record_subquery(
            select, storage, txid, arena, params, outer, row_arity, sequences,
        );
    }
    if select.items.len() != 1 {
        return Err(sql_err!(
            sqlstate::SYNTAX_ERROR,
            "subquery must return exactly one column"
        ));
    }
    // `SELECT *` is accepted when the source has exactly one column (resolved
    // below); until then a placeholder stands in (a wildcard carries no
    // subqueries or aggregates of its own).
    let wildcard = matches!(&select.items[0], SelectItem::Wildcard(_));
    let table_star = match &select.items[0] {
        SelectItem::TableWildcard(q) => Some(*q),
        _ => None,
    };
    let item: &Expr = match &select.items[0] {
        SelectItem::Expr { expression, .. } => expression,
        SelectItem::Wildcard(_) | SelectItem::TableWildcard(_) | SelectItem::RecordStar(_) => {
            &Expr::Null
        }
    };
    // A window function needs rows materialized before it can be computed, so
    // its body belongs to the row-source executor just as a grouped one does.
    let has_window = super::expr_has_window(item)
        || select
            .order_by
            .iter()
            .any(|key| super::expr_has_window(key.expression));
    // A set-returning function in the subquery's select list expands to many
    // rows, so its body belongs to the row-source executor too (which handles
    // SRF expansion) — just like a grouped or windowed subquery.
    let has_srf = super::srf::has_project_set(select.items, storage, txid);
    if !select.group_by.is_empty()
        || select.having.is_some()
        || select.distinct
        || has_window
        || has_srf
    {
        // Grouped/DISTINCT/windowed/SRF subquery: the row-source executor
        // already handles grouping, HAVING, DISTINCT, windows, and SRF
        // expansion; collect its single output column.
        let mut out = ArenaList::new(arena);
        let mut any_null = false;
        select_into_rows(
            storage,
            txid,
            select,
            arena,
            params,
            outer,
            sequences,
            &mut |vals| {
                if vals.len() != 1 {
                    return Err(sql_err!(
                        sqlstate::SYNTAX_ERROR,
                        "subquery must return only one column"
                    ));
                }
                // Row-source values may borrow a recycling external-run
                // reader. Re-encode the scalar into the statement arena
                // before retaining it in the subquery result.
                let encoded = crate::sql::exec::encode_projected_pub(vals, arena)?;
                out.push(crate::sql::exec::decode_projected_col_record(
                    encoded, 0, arena,
                )?)
                .map_err(|_| arena_full())?;
                any_null |= vals[0].is_null();
                Ok(())
            },
        )?;
        let own_scope = select
            .from
            .as_ref()
            .and_then(|f| QueryScope::resolve_schema(storage, f, txid, arena, outer).ok());
        let witness = spooled_column_witness(
            select,
            storage,
            txid,
            own_scope.as_ref(),
            arena,
            item,
            outer,
        )?;
        return Ok((out.as_slice(), any_null, witness));
    }

    // Inner subqueries first.
    let sample_capacity = select
        .from
        .as_ref()
        .map_or(0, |from| 2 * (from.joins.len() + 1));
    let inner_expressions = arena
        .alloc_slice_with(2 + sample_capacity, |_| None)
        .map_err(|_| arena_full())?;
    inner_expressions[0] = Some(item);
    inner_expressions[1] = select.where_clause;
    if let Some(from) = &select.from {
        collect_table_sample_expressions(from, &mut inner_expressions[2..]);
    }
    let inner_subs = prepare_subqueries(
        inner_expressions,
        storage,
        txid,
        arena,
        params,
        depth - 1,
        outer,
        sequences,
    )?;
    let catalog = super::storage_catalog(storage, arena, txid);
    let hooks = EvalHooks {
        group: None,
        aggs: None,
        subs: Some(&inner_subs),
        subquery_executor: None,
        windows: None,
        catalog: Some(&catalog),
        srf_index: None,
        project_sets: None,
        sequences,
        merge_action: None,
    };

    let Some(from) = &select.from else {
        if wildcard {
            return Err(sql_err!(
                sqlstate::SYNTAX_ERROR,
                "SELECT * with no tables specified is not valid"
            ));
        }
        // FROM-less: one row (outer columns still visible if correlated).
        // Aggregates fold over that single virtual row (zero when WHERE is
        // false) and still yield their one output row.
        let aggregate_columns = SubqueryColumns {
            scope: None,
            outer,
            catalog: crate::sql::exec::CatalogCols {
                definition: None,
                alias: None,
                output_aliases: &[],
                storage,
                txid,
                arena,
            },
        };
        let mut agg_nodes = ArenaList::new(arena);
        collect_aggs(item, &mut agg_nodes, storage, txid, &aggregate_columns)?;
        if !agg_nodes.is_empty() {
            let Some((ptrs, values)) = fromless_aggregate_hooks(
                select,
                agg_nodes.as_slice(),
                storage,
                txid,
                arena,
                params,
                &Chained {
                    inner: &crate::sql::eval::NoColumns,
                    outer,
                },
                &hooks,
            )?
            else {
                return Ok((
                    &[],
                    false,
                    subquery_witness_with_outer(storage, txid, item, None, outer, arena)?,
                ));
            };
            let agg_hooks = EvalHooks {
                aggs: Some((ptrs, values)),
                ..hooks
            };
            let base = Chained {
                inner: &crate::sql::eval::NoColumns,
                outer,
            };
            let v = eval_full(item, arena, params, &base, &agg_hooks)?;
            let out = arena.alloc_slice_copy(&[v]).map_err(|_| arena_full())?;
            return Ok((
                &*out,
                v.is_null(),
                subquery_witness_with_outer(storage, txid, item, None, outer, arena)?,
            ));
        }
        let base = Chained {
            inner: &crate::sql::eval::NoColumns,
            outer,
        };
        if let Some(w) = select.where_clause
            && !where_passes(w, arena, params, &base, &hooks)?
        {
            return Ok((
                &[],
                false,
                subquery_witness_with_outer(storage, txid, item, None, outer, arena)?,
            ));
        }
        let v = eval_full(item, arena, params, &base, &hooks)?;
        let out = arena.alloc_slice_copy(&[v]).map_err(|_| arena_full())?;
        return Ok((
            &*out,
            v.is_null(),
            subquery_witness_with_outer(storage, txid, item, None, outer, arena)?,
        ));
    };
    let scope = QueryScope::resolve_exec_outer(
        storage,
        from,
        select.where_clause,
        txid,
        arena,
        params,
        hooks.sequences,
        outer,
    )?;

    // `SELECT *` is a single-column subquery only if the source is exactly one
    // column; expand it to that column so the row-value path below applies.
    let item: &Expr = if wildcard {
        if scope.star_columns() != 1 {
            return Err(sql_err!(
                sqlstate::SYNTAX_ERROR,
                "subquery must return only one column"
            ));
        }
        let name = scope.output_name(scope.star_entry(0));
        arena
            .alloc(Expr::Column {
                qualifier: None,
                name,
            })
            .map_err(|_| arena_full())?
    } else if let Some(q) = table_star {
        if scope.qualified_star_columns(q)? != 1 {
            return Err(sql_err!(
                sqlstate::SYNTAX_ERROR,
                "subquery must return only one column"
            ));
        }
        arena
            .alloc(Expr::Column {
                qualifier: Some(q),
                name: scope.output_name(scope.qualified_star_entry(q, 0)?),
            })
            .map_err(|_| arena_full())?
    } else {
        item
    };

    // Aggregate subquery: one row.
    let aggregate_columns = SubqueryColumns {
        scope: Some(&scope),
        outer,
        catalog: crate::sql::exec::CatalogCols {
            definition: None,
            alias: None,
            output_aliases: &[],
            storage,
            txid,
            arena,
        },
    };
    let mut agg_nodes = ArenaList::new(arena);
    collect_aggs(item, &mut agg_nodes, storage, txid, &aggregate_columns)?;
    if !agg_nodes.is_empty() {
        let agg_values = fold_aggregates(
            storage,
            &scope,
            from,
            txid,
            select.where_clause,
            agg_nodes.as_slice(),
            arena,
            params,
            &hooks,
            outer,
        )?;
        let agg_nodes = agg_nodes.as_slice();
        let ptrs = arena
            .alloc_slice_with(agg_nodes.len(), |i| agg_nodes[i].0)
            .map_err(|_| arena_full())?;
        let agg_hooks = EvalHooks {
            group: None,
            aggs: Some((&*ptrs, agg_values)),
            subs: hooks.subs,
            subquery_executor: hooks.subquery_executor,
            windows: None,
            catalog: hooks.catalog,
            srf_index: None,
            project_sets: None,
            sequences: hooks.sequences,
            merge_action: hooks.merge_action,
        };
        let schema = ScopeSchema(&scope);
        let base = Chained {
            inner: &schema,
            outer,
        };
        let v = eval_full(item, arena, params, &base, &agg_hooks)?;
        let out = arena.alloc_slice_copy(&[v]).map_err(|_| arena_full())?;
        return Ok((
            &*out,
            v.is_null(),
            spooled_column_witness(select, storage, txid, Some(&scope), arena, item, outer)?,
        ));
    }

    // Plain scan: collect item values (and ORDER BY keys). Two passes (count
    // then fill), then sort and apply OFFSET/LIMIT so a subquery's own ORDER BY
    // / LIMIT is honored (element order matters for ARRAY(...) and scalar).
    let n_keys = select.order_by.len();
    let mut expressions = [&Expr::Null; MAX_PROJ + 3];
    expressions[0] = item;
    let mut expression_count = 1;
    if let Some(predicate) = select.where_clause {
        expressions[expression_count] = predicate;
        expression_count += 1;
    }
    for order in select.order_by {
        expressions[expression_count] = order.expression;
        expression_count += 1;
    }
    if let Some(limit) = select.limit {
        expressions[expression_count] = limit;
        expression_count += 1;
    }
    if let Some(offset) = select.offset {
        expressions[expression_count] = offset;
        expression_count += 1;
    }
    let pax_columns = pax_column_demand(&scope, from, &expressions[..expression_count]);
    let mut vals = ArenaList::new(arena);
    let mut keys = ArenaList::new(arena);
    scan_source_with_pax_columns(
        storage,
        &scope,
        from,
        txid,
        select.where_clause,
        arena,
        params,
        &hooks,
        outer,
        pax_columns,
        &mut |row| {
            let chained_row = Chained { inner: row, outer };
            let value = eval_full(item, arena, params, &chained_row, &hooks)?;
            vals.push(value).map_err(|_| arena_full())?;
            for o in select.order_by.iter() {
                // A positional `ORDER BY 1` sorts by the single output column.
                let key = match o.expression {
                    Expr::Int(_) => value,
                    e => eval_full(e, arena, params, &chained_row, &hooks)?,
                };
                keys.push(key).map_err(|_| arena_full())?;
            }
            Ok(true)
        },
    )?;

    let count = vals.len();
    let vals = vals.as_slice();
    let keys = keys.as_slice();

    // Stable sort of row indices by the ORDER BY keys.
    let order = arena
        .alloc_slice_with(count, |i| i)
        .map_err(|_| arena_full())?;
    if n_keys > 0 {
        let collations = arena
            .alloc_slice_with(n_keys, |_| crate::sql::ast::Collation::None)
            .map_err(|_| arena_full())?;
        for (collation, o) in collations.iter_mut().zip(select.order_by) {
            // A positional `ORDER BY 1` sorts by the single output column.
            let expression = match o.expression {
                Expr::Int(_) => item,
                e => e,
            };
            *collation = scope.expression_collation(storage, arena, expression)?;
        }
        crate::mem::arena::try_stable_sort_via(
            arena,
            order,
            |&a, &b| {
                cmp_key_rows(
                    storage,
                    txid,
                    collations,
                    &keys[a * n_keys..a * n_keys + n_keys],
                    &keys[b * n_keys..b * n_keys + n_keys],
                    select.order_by,
                )
            },
            |_| arena_full(),
        )?;
    }

    // Apply OFFSET/LIMIT over the ordered rows.
    let offset = crate::sql::exec::eval_offset_pub(select.offset, arena, params, &hooks)? as usize;
    let limit = crate::sql::exec::eval_limit_pub(select.limit, arena, params, &hooks)?;
    let start = offset.min(count);
    let n = ((count - start) as u64).min(limit) as usize;
    let mut saw_null = false;
    let out = arena
        .alloc_slice_with(n, |i| {
            let v = vals[order[start + i]];
            if v.is_null() {
                saw_null = true;
            }
            v
        })
        .map_err(|_| arena_full())?;
    Ok((
        &*out,
        saw_null,
        spooled_column_witness(select, storage, txid, Some(&scope), arena, item, outer)?,
    ))
}

/// Runs a set-operation query (UNION / INTERSECT / EXCEPT) in subquery position,
/// yielding its single output column as datums. Mirrors [`set_query`]'s type
/// unification and row combining, then decodes the lone column back to datums so
/// scalar / IN callers can consume them. Correlated columns are not visible to a
/// set-operation body (each leaf is materialized independently); an unresolved
/// reference surfaces loudly as a missing-column error from the leaf itself.
#[expect(
    clippy::too_many_arguments,
    reason = "explicit subquery execution context"
)]
fn run_set_subquery<'a>(
    tree: &'a SetTree<'a>,
    outer_select: &'a Select<'a>,
    storage: &'a Storage,
    txid: u32,
    arena: &'a Arena,
    params: &[Datum<'a>],
    outer: Option<&dyn ColumnLookup<'a>>,
    row_arity: usize,
    sequences: Option<&dyn crate::sql::eval::SequenceAccess>,
) -> Result<(&'a [Datum<'a>], bool, Datum<'a>), SqlError> {
    let bound_catalog = super::storage_catalog(storage, arena, txid);
    let bound_queries = super::ProceduralSubqueries { storage, txid };
    let bound_hooks = crate::sql::eval::EvalHooks {
        catalog: Some(&bound_catalog),
        subquery_executor: Some(&bound_queries),
        sequences,
        ..crate::sql::eval::NO_HOOKS
    };

    // Each branch reads the levels around the subquery.
    let (rows, target, n_cols) = super::setops::materialize_set_body_tied(
        storage, txid, tree, arena, params, sequences, outer,
    )?;
    if n_cols != row_arity {
        return Err(sql_err!(
            sqlstate::SYNTAX_ERROR,
            "subquery must return only one column"
        ));
    }
    if row_arity > 1 {
        return set_record_rows(
            rows,
            &target[..n_cols],
            outer_select,
            arena,
            params,
            &bound_hooks,
        );
    }
    let (start, n) = set_window(rows.len(), outer_select, arena, params, &bound_hooks)?;
    let mut saw_null = false;
    // The slice is reserved before any element decodes: a record column's
    // structural decode allocates its fields from the same arena, which must
    // not interleave with the slice's own reservation.
    let out = arena
        .alloc_slice_with(n, |_| Datum::Null)
        .map_err(|_| arena_full())?;
    for (i, slot) in out.iter_mut().enumerate() {
        let v = crate::sql::exec::decode_projected_col_record(rows[start + i], 0, arena)?;
        if v.is_null() {
            saw_null = true;
        }
        *slot = v;
    }
    let witness = match target[0] {
        ColType::Composite(slot) => {
            composite_slot_witness(
                storage,
                txid,
                crate::storage::TypeCreation::require(slot)?,
                arena,
            )?
            .0
        }
        ctype => type_witness(ctype)?,
    };
    Ok((&*out, saw_null, witness))
}

/// The row-comparison form of the above: each materialized row becomes a
/// `Datum::Record` of its columns, so `(a, b) IN (SELECT x, y ... UNION ...)`
/// compares against the same record the left-hand `ROW(...)` builds. The
/// branches were already combined and deduplicated column-wise, which is where
/// a record — having no storage type — could not have gone.
fn set_record_rows<'a>(
    rows: &'a [&'a [u8]],
    target: &[ColType],
    outer_select: &'a Select<'a>,
    arena: &'a Arena,
    params: &[Datum<'a>],
    hooks: &EvalHooks<'_, 'a>,
) -> Result<(&'a [Datum<'a>], bool, Datum<'a>), SqlError> {
    let (start, n) = set_window(rows.len(), outer_select, arena, params, hooks)?;
    let mut names = [""; MAX_PROJ];
    for (column, name) in names[..target.len()].iter_mut().enumerate() {
        let text = stack_format!(12, "f{}", column + 1);
        *name = arena.alloc_str(text.as_str()).map_err(|_| arena_full())?;
    }
    let record = |values: &mut dyn FnMut(usize) -> Result<Datum<'a>, SqlError>|
     -> Result<Datum<'a>, SqlError> {
        let mut fields = [RecordField { name: "", type_oid: 0, value: Datum::Null }; MAX_PROJ];
        for (column, field) in fields[..target.len()].iter_mut().enumerate() {
            let value = values(column)?;
            *field = RecordField {
                name: names[column],
                type_oid: target[column].oid(),
                value,
            };
        }
        let fields = arena.alloc_slice_copy(&fields[..target.len()]).map_err(|_| arena_full())?;
        Ok(Datum::Record(&*fields))
    };
    let out = arena
        .alloc_slice_with(n, |_| Datum::Null)
        .map_err(|_| arena_full())?;
    for (i, slot) in out.iter_mut().enumerate() {
        *slot = record(&mut |column| {
            crate::sql::exec::decode_projected_col_record(rows[start + i], column, arena)
        })?;
    }
    // The witness types the IN operand before any comparison, so it is built
    // from the column types rather than from a row that may not exist.
    let witness = record(&mut |column| type_witness(target[column]))?;
    // A record itself is never null here; a null *field* makes an individual
    // membership comparison unknown, which `membership_eq` decides.
    Ok((&*out, false, witness))
}

/// The `OFFSET` / `LIMIT` trailing a set operation, as a `(start, count)`
/// window over the materialized rows.
fn set_window<'a>(
    total: usize,
    outer_select: &'a Select<'a>,
    arena: &'a Arena,
    params: &[Datum<'a>],
    hooks: &EvalHooks<'_, 'a>,
) -> Result<(usize, usize), SqlError> {
    let offset = crate::sql::exec::eval_offset_pub(outer_select.offset, arena, params, hooks)?;
    let limit = crate::sql::exec::eval_limit_pub(outer_select.limit, arena, params, hooks)?;
    let start = (offset as usize).min(total);
    Ok((start, ((total - start) as u64).min(limit) as usize))
}

/// Serialize the already-typed value without rechecking domain constraints.
fn normalize_array_subquery_element<'a>(
    value: Datum<'a>,
    element: crate::sql::types::ArrElem,
    storage: &Storage,
    txid: u32,
    arena: &'a Arena,
) -> Result<Datum<'a>, SqlError> {
    if value.is_null() {
        return Ok(value);
    }
    match element.to_coltype() {
        ColType::Composite(identity) => crate::sql::exec::coerce_user_type_array_element(
            value,
            crate::sql::types::ArrElem::Composite(identity),
            storage,
            txid,
            arena,
            None,
        ),
        _ => crate::sql::eval::cast_to(value, element.to_coltype(), arena),
    }
}

/// Declarations fix the element type and whether arrays add a dimension, even
/// when the subquery produces no rows or only null values.
fn build_array_scalar<'a>(
    values: &[Datum<'a>],
    plan: crate::sql::eval::ArrayConstructorType,
    storage: &Storage,
    txid: u32,
    arena: &'a Arena,
) -> Result<Datum<'a>, SqlError> {
    if plan.multidimensional {
        if values.is_empty() {
            return Ok(Datum::Array {
                element: plan.element,
                raw: crate::sql::array::EMPTY,
            });
        }
        return crate::sql::array::stack(values, arena);
    }
    let element = plan.element;
    let buffer = arena
        .alloc_slice_with(values.len(), |i| values[i])
        .map_err(|_| arena_full())?;
    for v in buffer.iter_mut() {
        if !v.is_null() {
            *v = normalize_array_subquery_element(*v, element, storage, txid, arena)?;
        }
    }
    Ok(Datum::Array {
        element,
        raw: crate::sql::array::build(buffer, arena)?,
    })
}

/// Order helpers exported for update/delete WHERE-subquery support.
pub fn subquery_hooks<'a>(
    exprs: &[Option<&'a Expr<'a>>],
    storage: &Storage,
    txid: u32,
    arena: &'a Arena,
    params: &[Datum<'a>],
    sequences: Option<&dyn crate::sql::eval::SequenceAccess>,
) -> Result<SubqueryValues<'a, 'a>, SqlError> {
    subquery_hooks_with_outer(exprs, storage, txid, arena, params, None, sequences)
}

pub(crate) fn subquery_hooks_outer<'a>(
    exprs: &[Option<&'a Expr<'a>>],
    storage: &Storage,
    txid: u32,
    arena: &'a Arena,
    params: &[Datum<'a>],
    outer: &dyn ColumnLookup<'a>,
    sequences: Option<&dyn crate::sql::eval::SequenceAccess>,
) -> Result<SubqueryValues<'a, 'a>, SqlError> {
    subquery_hooks_with_outer(exprs, storage, txid, arena, params, Some(outer), sequences)
}

pub(crate) fn subquery_hooks_with_outer<'a>(
    exprs: &[Option<&'a Expr<'a>>],
    storage: &Storage,
    txid: u32,
    arena: &'a Arena,
    params: &[Datum<'a>],
    outer: Option<&dyn ColumnLookup<'a>>,
    sequences: Option<&dyn crate::sql::eval::SequenceAccess>,
) -> Result<SubqueryValues<'a, 'a>, SqlError> {
    // `prepare_subqueries` may return values borrowing relation bytes. This
    // adapter immediately round-trips every value through the arena codec, so
    // no storage borrow escapes. The engine keeps Storage live for the call.
    // SAFETY: Storage outlives this call, remains immutable during evaluation,
    // and every possibly borrowed datum is detached before this function returns.
    let storage_for_eval: &'a Storage = unsafe { &*(storage as *const Storage) };
    let borrowed = prepare_subqueries(
        exprs,
        storage_for_eval,
        txid,
        arena,
        params,
        SUBQUERY_DEPTH,
        outer,
        sequences,
    )?;
    detach_subquery_values(borrowed, arena)
}

fn detach_subquery_values<'a>(
    borrowed: SubqueryValues<'a, '_>,
    arena: &'a Arena,
) -> Result<SubqueryValues<'a, 'a>, SqlError> {
    let scalars = arena
        .alloc_slice_with(borrowed.scalars.len(), |_| {
            (core::ptr::null(), Datum::Null, Datum::Null)
        })
        .map_err(|_| arena_full())?;
    for (index, (node, value, witness)) in borrowed.scalars.iter().enumerate() {
        scalars[index] = (
            (*node).cast(),
            detach_subquery_datum(*value, arena)?,
            detach_subquery_datum(*witness, arena)?,
        );
    }
    let lists: &mut [SubqueryList<'a>] = arena
        .alloc_slice_with(borrowed.lists.len(), |_| empty_subquery_list())
        .map_err(|_| arena_full())?;
    for (index, list) in borrowed.lists.iter().enumerate() {
        let detached_values = arena
            .alloc_slice_with(list.values.len(), |_| Datum::Null)
            .map_err(|_| arena_full())?;
        for (value_index, value) in list.values.iter().enumerate() {
            detached_values[value_index] = detach_subquery_datum(*value, arena)?;
        }
        let collations = arena
            .alloc_slice_copy(list.collations)
            .map_err(|_| arena_full())?;
        lists[index] = SubqueryList {
            node: list.node,
            values: &*detached_values,
            probe: list.probe,
            saw_null: list.saw_null,
            witness: detach_subquery_datum(list.witness, arena)?,
            collations: &*collations,
        };
    }
    Ok(SubqueryValues {
        scalars: &*scalars,
        lists: &*lists,
        deferred: borrowed.deferred,
    })
}

#[derive(Clone, Copy)]
pub(crate) struct ProceduralSubqueries<'s> {
    pub storage: &'s Storage,
    pub txid: u32,
}

impl super::super::eval::SubqueryExecution for ProceduralSubqueries<'_> {
    fn evaluate<'a>(
        &self,
        expression: &Expr<'a>,
        arena: &'a Arena,
        params: &[Datum<'a>],
        outer: &dyn ColumnLookup<'a>,
        sequences: Option<&dyn crate::sql::eval::SequenceAccess>,
        retained: bool,
    ) -> Result<super::super::eval::SubqueryResult<'a>, SqlError> {
        // SAFETY: storage remains immutable and live for this call; the
        // result is detached before the borrow can escape.
        let storage: &'a Storage = unsafe { &*(self.storage as *const Storage) };
        let borrowed = eval_subquery_nodes(
            &[expression],
            storage,
            self.txid,
            arena,
            params,
            SUBQUERY_DEPTH,
            Some(outer),
            sequences,
            if retained {
                SubqueryTiming::Retained
            } else {
                SubqueryTiming::Now
            },
        )?;
        let values = detach_subquery_values(borrowed, arena)?;
        match (values.scalars, values.lists) {
            ([(_, value, witness)], []) => Ok(super::super::eval::SubqueryResult::Scalar {
                value: *value,
                witness: *witness,
            }),
            ([], [list]) => Ok(super::super::eval::SubqueryResult::List(*list)),
            _ => Err(sql_err!(
                sqlstate::INTERNAL_ERROR,
                "one subquery produced an invalid result shape"
            )),
        }
    }
}

/// Detaches a subquery result before DML mutates storage. Records retain their
/// typed fields instead of being flattened through the scalar row codec.
fn detach_subquery_datum<'a>(value: Datum<'_>, arena: &'a Arena) -> Result<Datum<'a>, SqlError> {
    crate::sql::exec::detach_routine_datum(value, arena)
}
