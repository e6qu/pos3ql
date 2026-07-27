//! Statement execution against table storage.
//!
//! Scans decode rows from the memtable heap into stack arrays; ORDER BY
//! materializes sort keys into the per-statement arena (bounded by the
//! arena size, loudly). No allocation anywhere.

use crate::mem::arena::Arena;
use crate::mem::fixed_vec::FixedVec;
use crate::pg::respond::Responder;
use crate::pg::wire::WireFull;
use crate::sql_err;
use crate::stack_format;
use crate::util::StackStr;
use crate::storage::{
    ColumnMeta, RowHome, SeqSpec, SeqType, SqlName, Storage, TableDef, MAX_COLUMNS,
};
use super::txn::TxnState;
use crate::storage::rowenc;
use crate::wal::{Wal, WalOp};

use super::ast::{
    AlterAction, AlterTable, CreateTable, Delete, DropTable, Expr, Insert, LikeClause, Overriding,
    QualName, SelectItem, Update,
};
use super::eval::{cast_to, compare_datums, eval, sqlstate, ColumnLookup, NoColumns, SqlError};
use super::types::{ColDesc, ColType, Datum, TypeMod};

/// Wildcard expansion can double the select list.
pub const MAX_PROJ: usize = MAX_COLUMNS * 2;

/// Column resolution over one decoded row. The datum lifetime `'v` (heap /
/// arena bytes) is independent of the borrow of the value slice itself, so
/// looked-up datums may outlive the decode buffer.
pub struct RowCtx<'s, 'v, 'd> {
    pub def: &'d TableDef,
    pub values: &'s [Datum<'v>],
}

impl<'v> ColumnLookup<'v> for RowCtx<'_, 'v, '_> {
    fn lookup(&self, qualifier: Option<&str>, name: &str) -> Result<Datum<'v>, SqlError> {
        if let Some(q) = qualifier
            && !crate::sql::eval::qualifier_answers_single(self.def, q) {
                return Err(sql_err!(
                    sqlstate::UNDEFINED_TABLE,
                    "missing FROM-clause entry for table \"{}\"",
                    q
                ));
            }
        match self.def.column_index(name) {
            Some(i) => Ok(self.values[i]),
            None => Err(sql_err!(
                sqlstate::UNDEFINED_COLUMN,
                "column \"{}\" does not exist",
                name
            )),
        }
    }

    fn whole_row_fields(
        &self,
        table: &str,
        arena: &'v Arena,
    ) -> Result<Option<&'v [super::types::RecordField<'v>]>, SqlError> {
        if table != self.def.name.as_str() {
            return Err(sql_err!(
                sqlstate::UNDEFINED_TABLE,
                "missing FROM-clause entry for table \"{}\"",
                table
            ));
        }
        let cols = self.def.columns();
        let mut fields = [super::types::RecordField {
            name: "",
            type_oid: 0,
            value: Datum::Null,
        }; MAX_COLUMNS];
        let too_large =
            || sql_err!(sqlstate::PROGRAM_LIMIT_EXCEEDED, "record exceeds the arena");
        for (i, field) in fields.iter_mut().enumerate().take(self.def.n_columns) {
            field.name = arena.alloc_str(cols[i].name.as_str()).map_err(|_| too_large())?;
            field.type_oid = cols[i].ctype.oid();
            field.value = self.values.get(i).copied().unwrap_or(Datum::Null);
        }
        let out = arena.alloc_slice_copy(&fields[..self.def.n_columns]).map_err(|_| too_large())?;
        Ok(Some(&*out))
    }

    fn col_type(&self, qualifier: Option<&str>, name: &str) -> Option<ColType> {
        if let Some(q) = qualifier
            && q != self.def.name.as_str() {
                return None;
            }
        self.def.column_index(name).map(|i| self.def.columns()[i].ctype)
    }

    fn column_domain(&self, qualifier: Option<&str>, name: &str) -> Option<SqlName> {
        if let Some(q) = qualifier
            && q != self.def.name.as_str()
        {
            return None;
        }
        self.def.column_index(name).and_then(|i| self.def.columns()[i].domain)
    }
}

type Outcome = Result<Result<(), SqlError>, WireFull>;

fn sql_ok() -> Outcome {
    Ok(Ok(()))
}

fn sql_fail(e: SqlError) -> Outcome {
    Ok(Err(e))
}

mod describe;
pub use describe::{could_not_identify, init_record_shapes, not_composite, register_shape_for, reset_record_shapes, expr_record_handle as expr_record_handle_pub, visit_record_shape as visit_record_shape_pub,
    check_row_field_types, derived_name, describe_items, infer_type_pub, infer_type_res,
    record_field_type, record_shape, typeof_static, typeof_static_coltype, ColTypeResolver, DefCols, NoCols,
    RECORD_FIELD_NAMES,
};
pub(crate) use describe::{coltype_of_oid, json_each_value_type_pub, unify_numeric_tower};

mod projected;
pub use projected::{
    decode_projected_col_record, decode_projected_pub, decode_projected_value, encode_projected_pub, projected_prefix_len,
    projected_value_len, sort_dedup_projected,
};

mod ddl;
use ddl::{add_unique_key, attach_constraints, auto_key_name, build_column, build_def};

mod constraints;
pub use constraints::{check_all_unique, check_unique, check_unique_indexes};
use constraints::{
    apply_fk_parent_actions, enforce_row_constraints, parse_checks, parse_defaults,
    parse_generated, referenced_key_changed, table_is_referenced, ParsedChecks,
    MAX_FK_CASCADE_DEPTH,
};

pub fn create_table(
    storage: &mut Storage,
    wal: &mut Wal,
    txn: &mut TxnState,
    statement: &CreateTable,
    arena: &Arena,
    responder: &mut Responder,
) -> Outcome {
    let mut def = match build_def_with_likes(storage, statement, txn.txid, arena) {
        Ok(d) => d,
        Err(e) => return sql_fail(e),
    };
    def.schema = match storage.creation_schema(
        statement.name.schema,
        statement.name.name,
        txn.txid,
    ) {
        Ok(n) => n,
        Err(e) => return sql_fail(e),
    };
    // A copied constraint lands before the explicitly written ones, so a
    // duplicate primary key is caught with PostgreSQL's own message.
    if let Err(e) = copy_like_constraints(storage, &mut def, statement, txn.txid) {
        return sql_fail(e);
    }
    if let Err(e) = reject_multiple_primary(&def) {
        return sql_fail(e);
    }
    if let Err(e) = attach_constraints(storage, &mut def, statement.constraints, txn.txid, arena) {
        return sql_fail(e);
    }
    match storage.create_table_in(def, txn.txid) {
        Ok(slot) => {
            let lsn = storage.bump_lsn();
            if let Err(e) = wal.append(lsn, &WalOp::CreateTable(def)) {
                // Nothing reached the journal; undo the in-memory apply.
                storage.rollback_create(slot);
                return sql_fail(e);
            }
            if let Err(e) = txn.record_ddl(super::txn::DdlUndo::Created(slot as u32)) {
                storage.rollback_create(slot);
                return sql_fail(e);
            }
            // Seed an identity column's counter from its START WITH, so the
            // first value handed out equals START (the generator advances by the
            // step before yielding).
            for c in statement.columns {
                if let Some(start) = ddl::identity_start(c)
                    && let Some(idx) = def.column_index(c.name)
                {
                    let step = def.columns()[idx].auto_increment_step;
                    let table = storage.table_mut(slot);
                    table.serial_last[idx] = start.wrapping_sub(step);
                    table.serial_dirty = true;
                }
            }
        }
        Err(e) if e.sqlstate == sqlstate::DUPLICATE_TABLE && statement.if_not_exists => {
            responder.notice(
                crate::sql::eval::sqlstate::DUPLICATE_TABLE,
                stack_format!(128, "relation \"{}\" already exists, skipping", statement.name.name).as_str(),
            )?;
        }
        Err(e) => return sql_fail(e),
    }
    if let Err(e) = copy_like_indexes(storage, wal, txn, statement, &def) {
        return sql_fail(e);
    }
    responder.command_complete("CREATE TABLE")?;
    sql_ok()
}

/// A table gets one primary key. A column-level `PRIMARY KEY` sets the column's
/// flag directly and never reaches [`attach_constraints`], so two of them — or
/// one alongside a key copied by `LIKE ... INCLUDING INDEXES` — is only caught
/// by counting the assembled definition.
fn reject_multiple_primary(def: &TableDef) -> Result<(), SqlError> {
    let declared = def.columns().iter().filter(|c| c.primary).count()
        + def.uniques[..def.n_uniques].iter().filter(|k| k.is_primary).count();
    if declared > 1 {
        return Err(sql_err!(
            crate::sql::eval::sqlstate::INVALID_TABLE_DEFINITION,
            "multiple primary keys for table \"{}\" are not allowed",
            def.name.as_str()
        ));
    }
    Ok(())
}

/// The source table of a `LIKE`, or PostgreSQL's undefined-table error.
fn like_source<'s>(
    storage: &'s Storage,
    like: &LikeClause,
    txid: u32,
) -> Result<&'s TableDef, SqlError> {
    match resolve_dml_table(storage, &like.source, txid) {
        Ok(i) => Ok(&storage.table(i).def),
        Err(e) => Err(e),
    }
}

/// [`build_def`] with each `LIKE source` element's columns spliced in where it
/// was written. A copied column always keeps its name, type and NOT NULL; the
/// rest of its properties follow the element's `INCLUDING` flags.
fn build_def_with_likes(
    storage: &Storage,
    statement: &CreateTable,
    txid: u32,
    arena: &Arena,
) -> Result<TableDef, SqlError> {
    if statement.likes.is_empty() {
        return build_def(statement.name.name, statement.columns, storage, txid, arena);
    }
    let mut def = TableDef { name: SqlName::parse(statement.name.name)?, ..TableDef::empty() };
    let mut n = 0usize;
    for position in 0..=statement.columns.len() {
        for like in statement.likes.iter().filter(|l| l.at == position) {
            let source = like_source(storage, like, txid)?;
            for column in source.columns() {
                let mut copied = *column;
                if !like.defaults {
                    copied.default_value = None;
                    if !copied.is_generated {
                        copied.default_expr = None;
                    }
                }
                if !like.indexes {
                    copied.unique = false;
                    copied.primary = false;
                }
                if !like.identity {
                    copied.auto_increment = false;
                }
                // Without INCLUDING GENERATED a generated column is copied as a
                // plain column (its generation expression is dropped).
                if !like.generated && copied.is_generated {
                    copied.is_generated = false;
                    copied.default_expr = None;
                }
                push_column(&mut def, &mut n, copied)?;
            }
        }
        if let Some(column) = statement.columns.get(position) {
            push_column(&mut def, &mut n, build_column(column, storage, txid, arena)?)?;
        }
    }
    def.n_columns = n;
    Ok(def)
}

/// Appends one column, rejecting a name already taken.
fn push_column(def: &mut TableDef, n: &mut usize, column: ColumnMeta) -> Result<(), SqlError> {
    if *n == MAX_COLUMNS {
        return Err(sql_err!(
            sqlstate::PROGRAM_LIMIT_EXCEEDED,
            "tables can have at most {} columns",
            MAX_COLUMNS
        ));
    }
    if def.columns[..*n].iter().any(|prev| prev.name == column.name) {
        return Err(sql_err!(
            sqlstate::DUPLICATE_COLUMN,
            "column \"{}\" specified more than once",
            column.name.as_str()
        ));
    }
    def.columns[*n] = column;
    *n += 1;
    Ok(())
}

/// Copies the CHECK constraints and multi-column keys of each `LIKE` source
/// that asked for them. Foreign keys are never copied, as in PostgreSQL.
fn copy_like_constraints(
    storage: &Storage,
    def: &mut TableDef,
    statement: &CreateTable,
    txid: u32,
) -> Result<(), SqlError> {
    for like in statement.likes {
        let source = like_source(storage, like, txid)?;
        if like.constraints {
            for check in &source.checks[..source.n_checks] {
                if def.n_checks == crate::storage::MAX_CHECKS {
                    return Err(sql_err!(
                        sqlstate::PROGRAM_LIMIT_EXCEEDED,
                        "a table can have at most {} CHECK constraints",
                        crate::storage::MAX_CHECKS
                    ));
                }
                let mut copied = *check;
                // The name is regenerated from the new table, as PostgreSQL
                // does, so the two tables' constraints stay distinguishable.
                copied.name = auto_key_name(def, &[], "check", false)?;
                def.checks[def.n_checks] = copied;
                def.n_checks += 1;
            }
        }
        if like.indexes {
            for key in &source.uniques[..source.n_uniques] {
                let columns = remap_columns(def, source, &key.columns[..key.n_cols])?;
                add_unique_key(
                    def,
                    None,
                    if key.is_primary { "pkey" } else { "key" },
                    &columns,
                    key.n_cols,
                    key.is_primary,
                )?;
            }
        }
    }
    Ok(())
}

/// Maps column positions in `source` onto the new table, which may have shifted
/// them by preceding columns.
fn remap_columns(
    def: &TableDef,
    source: &TableDef,
    columns: &[u16],
) -> Result<[u16; crate::storage::MAX_INDEX_COLS], SqlError> {
    let mut out = [0u16; crate::storage::MAX_INDEX_COLS];
    for (slot, &c) in out.iter_mut().zip(columns) {
        let name = source.columns()[c as usize].name.as_str();
        match def.column_index(name) {
            Some(i) => *slot = i as u16,
            None => {
                return Err(sql_err!(
                    sqlstate::UNDEFINED_COLUMN,
                    "column \"{}\" does not exist",
                    name
                ))
            }
        }
    }
    Ok(out)
}

/// One source index, captured before the mutable borrow that creates its copy.
#[derive(Clone, Copy)]
struct CopiedIndex {
    columns: [u16; crate::storage::MAX_INDEX_COLS],
    n_cols: usize,
    unique: bool,
}

/// Recreates each `LIKE` source's secondary indexes on the new table. It has no
/// rows yet, so the uniqueness scan [`create_index`] performs is unnecessary.
fn copy_like_indexes(
    storage: &mut Storage,
    wal: &mut Wal,
    txn: &mut TxnState,
    statement: &CreateTable,
    def: &TableDef,
) -> Result<(), SqlError> {
    use crate::storage::IndexDef;
    for like in statement.likes.iter().filter(|l| l.indexes) {
        // Collected up front: creating one needs `storage` mutably.
        let mut copied = [CopiedIndex { columns: [0; crate::storage::MAX_INDEX_COLS], n_cols: 0, unique: false };
            MAX_LIKE_INDEXES];
        let mut n_copied = 0;
        let source_def = storage.table(resolve_dml_table(storage, &like.source, txn.txid)?).def;
        for index in storage.indexes_for(
            source_def.schema.as_str(),
            source_def.name.as_str(),
            txn.txid,
        ) {
            if n_copied == MAX_LIKE_INDEXES {
                return Err(sql_err!(
                    sqlstate::PROGRAM_LIMIT_EXCEEDED,
                    "cannot copy more than {} indexes",
                    MAX_LIKE_INDEXES
                ));
            }
            copied[n_copied] =
                CopiedIndex { columns: index.columns, n_cols: index.n_cols, unique: index.unique };
            n_copied += 1;
        }
        let source = source_def;
        for index in &copied[..n_copied] {
            let columns = remap_columns(def, &source, &index.columns[..index.n_cols])?;
            let name = auto_key_name(def, &columns[..index.n_cols], "idx", true)?;
            let slot = storage.create_index(
                IndexDef {
                    schema: def.schema,
                    name,
                    table: def.name,
                    columns,
                    n_cols: index.n_cols,
                    unique: index.unique,
                    live: true,
                    pending: None,
                },
                txn.txid,
            )?;
            let lsn = storage.bump_lsn();
            if let Err(e) = wal.append(
                lsn,
                &WalOp::CreateIndex {
                    schema: def.schema.as_str(),
                    name: name.as_str(),
                    table: def.name.as_str(),
                    columns,
                    n_cols: index.n_cols,
                    unique: index.unique,
                },
            ) {
                storage.rollback_index_create(slot);
                return Err(e);
            }
            txn.record_ddl(super::txn::DdlUndo::IndexCreated(slot as u32))?;
        }
    }
    Ok(())
}

/// Upper bound on the secondary indexes one `LIKE ... INCLUDING INDEXES` copies.
const MAX_LIKE_INDEXES: usize = 8;


/// The next value of a serial/identity column: a real sequence, as PostgreSQL
/// has it. Explicit inserts do not advance it, deletes and TRUNCATE do not
/// rewind it, and the advance survives a rollback (a consumed number stays
/// consumed). The counter is journaled at commit and floored against the
/// stored rows at startup.
fn next_auto_value<'x>(
    storage: &mut Storage,
    table_index: usize,
    col: usize,
    ctype: ColType,
) -> Result<Datum<'x>, SqlError> {
    let table = storage.table_mut(table_index);
    let step = table.def.columns()[col].auto_increment_step;
    let next = table.serial_last[col] + step;
    let bound_error = |what: &'static str| {
        sql_err!(sqlstate::NUMERIC_OUT_OF_RANGE, "{} out of range", what)
    };
    let out = match ctype {
        ColType::Int8 => Datum::Int8(next),
        ColType::Int2 => {
            Datum::Int2(i16::try_from(next).map_err(|_| bound_error("smallint"))?)
        }
        _ => Datum::Int4(i32::try_from(next).map_err(|_| bound_error("integer"))?),
    };
    table.serial_last[col] = next;
    table.serial_dirty = true;
    Ok(out)
}

/// Finds an existing visible row that conflicts with the candidate on a
/// column-level UNIQUE/PRIMARY KEY or a UNIQUE index — the row ON CONFLICT
/// acts on. NULLs are distinct, so a candidate with a NULL key never conflicts.
fn find_conflict(
    storage: &Storage,
    table_index: usize,
    def: &TableDef,
    schema: &[ColType],
    values: &[Datum],
    txid: u32,
) -> Option<u64> {
    let mut found: Option<u64> = None;
    let _ = storage.for_each_row_state(table_index, &mut |rowid, state| {
        use core::ops::ControlFlow;
        let Some(home) = state.visible_at(txid, storage.read_snapshot()) else {
            return Ok(ControlFlow::Continue(()));
        };
        let hit = storage
            .with_row_bytes(table_index, rowid, home, |bytes| {
                let mut other = [Datum::Null; MAX_COLUMNS];
                if rowenc::decode(bytes, schema, &mut other).is_err() {
                    return Ok(false);
                }
                let eq = |a: &Datum, b: &Datum| {
                    !a.is_null()
                        && !b.is_null()
                        && compare_datums(a, b).map(|o| o.is_eq()).unwrap_or(false)
                };
                for (i, c) in def.columns().iter().enumerate() {
                    if c.unique && eq(&values[i], &other[i]) {
                        return Ok(true);
                    }
                }
                // Table-level keys, including the single-column ones that carry
                // an explicit name (so they live here rather than on a flag).
                for uk in def.uniques() {
                    let cols = uk.columns();
                    if !cols.iter().any(|&c| values[c as usize].is_null())
                        && cols.iter().all(|&c| eq(&values[c as usize], &other[c as usize]))
                    {
                        return Ok(true);
                    }
                }
                for index in
                    storage.unique_indexes_for(def.schema.as_str(), def.name.as_str(), txid)
                {
                    let icols = &index.columns[..index.n_cols];
                    if !icols.iter().any(|&c| values[c as usize].is_null())
                        && icols.iter().all(|&c| eq(&values[c as usize], &other[c as usize]))
                    {
                        return Ok(true);
                    }
                }
                Ok(false)
            })
            .unwrap_or(false);
        if hit {
            found = Some(rowid);
            return Ok(ControlFlow::Break(()));
        }
        Ok(ControlFlow::Continue(()))
    });
    found
}

/// Column lookup for ON CONFLICT DO UPDATE: `excluded.<col>` resolves to the
/// row proposed by INSERT; every other reference resolves to the existing
/// (conflicting) row.
struct ExcludedCtx<'s, 'v, 'd> {
    def: &'d TableDef,
    existing: &'s [Datum<'v>],
    excluded: &'s [Datum<'v>],
}

impl<'v> ColumnLookup<'v> for ExcludedCtx<'_, 'v, '_> {
    fn lookup(&self, qualifier: Option<&str>, name: &str) -> Result<Datum<'v>, SqlError> {
        let src = if qualifier == Some("excluded") {
            self.excluded
        } else {
            if let Some(q) = qualifier
                && !crate::sql::eval::qualifier_answers_single(self.def, q)
            {
                return Err(sql_err!(sqlstate::UNDEFINED_TABLE, "missing FROM-clause entry for table \"{}\"", q));
            }
            self.existing
        };
        match self.def.column_index(name) {
            Some(i) => Ok(src[i]),
            None => Err(sql_err!(sqlstate::UNDEFINED_COLUMN, "column \"{}\" does not exist", name)),
        }
    }

    fn col_type(&self, _qualifier: Option<&str>, name: &str) -> Option<ColType> {
        self.def.column_index(name).map(|i| self.def.columns()[i].ctype)
    }
}

enum ConflictOutcome {
    Store,
    Skip,
    Updated,
}

/// Applies an ON CONFLICT clause to one candidate row.
#[allow(clippy::too_many_arguments)]
fn handle_conflict(
    storage: &mut Storage,
    txn: &mut TxnState,
    table_index: usize,
    def: &TableDef,
    schema: &[ColType],
    values: &[Datum],
    on_conflict: &Option<super::ast::OnConflict>,
    checks: &ParsedChecks,
    arena: &Arena,
    params: &[Datum],
) -> Result<ConflictOutcome, SqlError> {
    let Some(oc) = on_conflict else {
        return Ok(ConflictOutcome::Store);
    };
    let Some(rowid) = find_conflict(storage, table_index, def, schema, values, txn.txid) else {
        return Ok(ConflictOutcome::Store);
    };
    let Some(assigns) = oc.update else {
        return Ok(ConflictOutcome::Skip); // DO NOTHING
    };
    // DO UPDATE: recompute the conflicting row, `excluded` = the proposed row.
    let new_bytes = {
        let mut existing = [Datum::Null; MAX_COLUMNS];
        let home = storage
            .table(table_index)
            .rows
            .get(&rowid)
            .and_then(|s| s.visible_at(txn.txid, storage.read_snapshot()))
            .ok_or_else(|| sql_err!(sqlstate::INTERNAL_ERROR, "conflict row vanished"))?;
        let bytes = storage.row_bytes(table_index, rowid, home, arena)?;
        rowenc::decode(bytes, schema, &mut existing)?;
        let context = ExcludedCtx { def, existing: &existing[..def.n_columns], excluded: values };
        if let Some(cond) = oc.update_where
            && !matches!(eval(cond, arena, params, &context)?, Datum::Bool(true))
        {
            return Ok(ConflictOutcome::Skip); // WHERE excluded this row
        }
        let mut new_values = [Datum::Null; MAX_COLUMNS];
        new_values[..def.n_columns].copy_from_slice(&existing[..def.n_columns]);
        for (name, expression) in assigns {
            let Some(target) = def.column_index(name) else {
                return Err(sql_err!(
                    sqlstate::UNDEFINED_COLUMN,
                    "column \"{}\" of relation \"{}\" does not exist",
                    name,
                    def.name.as_str()
                ));
            };
            let v = eval(expression, arena, params, &context)?;
            new_values[target] = coerce(v, &def.columns()[target], storage, arena)?;
        }
        check_not_null(def, &new_values)?;
        enforce_row_constraints(
            storage, table_index, def, schema, &new_values[..def.n_columns], Some(rowid),
            txn.txid, checks, arena, params,
        )?;
        let len = rowenc::encoded_len(&new_values[..def.n_columns]);
        let out = arena
            .alloc_slice_with(len, |_| 0u8)
            .map_err(|_| sql_err!(sqlstate::PROGRAM_LIMIT_EXCEEDED, "updated row exceeds the arena"))?;
        rowenc::encode(&new_values[..def.n_columns], out);
        &*out
    };
    let (new_loc, slice) = storage.heap.append(new_bytes.len())?;
    slice.copy_from_slice(new_bytes);
    let prior = storage.write_pending(table_index, rowid, txn.txid, txn.command_id(), Some(new_loc))?;
    if let Err(e) = txn.touch(table_index as u32, rowid, prior) {
        storage.restore_pending(table_index, rowid, txn.txid, prior);
        return Err(e);
    }
    Ok(ConflictOutcome::Updated)
}

/// Assigns each omitted/defaulted auto-increment column its next value. A
/// column the statement set *explicitly* — even to NULL — is left alone, so
/// an explicit NULL falls through to the not-null check exactly as PostgreSQL
/// rejects it, and an explicit value never advances the sequence.
fn fill_auto_increment(
    storage: &mut Storage,
    table_index: usize,
    def: &TableDef,
    values: &mut [Datum],
    explicit: &[bool; MAX_COLUMNS],
) -> Result<(), SqlError> {
    if !def.columns().iter().any(|c| c.auto_increment) {
        return Ok(());
    }
    for i in 0..def.n_columns {
        let col = &def.columns()[i];
        if col.auto_increment && !explicit[i] && values[i].is_null() {
            values[i] = next_auto_value(storage, table_index, i, col.ctype)?;
        }
    }
    Ok(())
}


/// PostgreSQL names the kind of object a DROP could not find — `table "x" does
/// not exist`, not `relation` — while every other lookup says relation.
fn undefined_kind(kind: &str, name: &str) -> SqlError {
    sql_err!(sqlstate::UNDEFINED_TABLE, "{} \"{}\" does not exist", kind, name)
}

pub fn drop_table(
    storage: &mut Storage,
    wal: &mut Wal,
    txn: &mut TxnState,
    statement: &DropTable,
    responder: &mut Responder,
) -> Outcome {
    for name in statement.names {
        // A DROP whose qualifier names no schema is PostgreSQL's 3F000 (a
        // SELECT of the same spelling is 42P01 — the codes really differ).
        if let Some(schema) = name.schema
            && storage.find_schema_visible(schema, txn.txid).is_none()
            && !statement.if_exists
        {
            return sql_fail(sql_err!(
                sqlstate::INVALID_SCHEMA_NAME,
                "schema \"{}\" does not exist",
                schema
            ));
        }
        // A sequence is a relation, but not a table: DROP TABLE on it is a type
        // error (42809), which IF EXISTS does not suppress.
        if storage.sequence_on_path(name.schema, name.name, txn.txid).is_some() {
            return sql_fail(sql_err!(
                sqlstate::WRONG_OBJECT_TYPE,
                "\"{}\" is not a table",
                name.name
            ));
        }
        match storage.resolve_relation(name.schema, name.name, txn.txid) {
            Some(crate::storage::ResolvedRelation::Table(index))
                if storage
                    .matview_slot(
                        storage.table(index).def.schema.as_str(),
                        storage.table(index).def.name.as_str(),
                        txn.txid,
                    )
                    .is_some() =>
            {
                // The backing table of a materialized view is not an ordinary
                // table; PostgreSQL refuses DROP TABLE on it (42809), directing
                // the user to DROP MATERIALIZED VIEW.
                return sql_fail(sql_err!(
                    sqlstate::WRONG_OBJECT_TYPE,
                    "\"{}\" is not a table",
                    name.name
                ));
            }
            Some(crate::storage::ResolvedRelation::Table(index)) => {
                if let Some(other) = storage.table(index).ddl_locked_by_other(txn.txid) {
                    let _ = other;
                    return sql_fail(sql_err!(
                        crate::sql::eval::sqlstate::SERIALIZATION_FAILURE,
                        "could not serialize access due to concurrent DDL on \"{}\"",
                        name.name
                    ));
                }
                let def = storage.table(index).def;
                let lsn = storage.bump_lsn();
                if let Err(e) = wal.append(
                    lsn,
                    &WalOp::DropTable {
                        schema: def.schema.as_str(),
                        name: def.name.as_str(),
                    },
                ) {
                    return sql_fail(e);
                }
                if let Err(e) = txn.record_ddl(super::txn::DdlUndo::Dropped(index as u32)) {
                    return sql_fail(e);
                }
                storage.drop_table_in(index, txn.txid);
                // A table's indexes are dropped with it (no separate journal
                // record; DropTable replay re-applies this).
                storage.drop_indexes_for(def.schema.as_str(), def.name.as_str(), txn.txid);
            }
            _ if statement.if_exists => {
                // PostgreSQL's skip notice carries SQLSTATE 00000.
                responder.notice(
                    crate::sql::eval::sqlstate::SUCCESSFUL_COMPLETION,
                    stack_format!(128, "table \"{}\" does not exist, skipping", name.name)
                        .as_str(),
                )?;
            }
            _ => return sql_fail(undefined_kind("table", name.name)),
        }
    }
    responder.command_complete("DROP TABLE")?;
    sql_ok()
}

/// CREATE SCHEMA [IF NOT EXISTS]: registers a schema in the catalog,
/// journaled and transactional like table DDL.
pub fn create_schema(
    storage: &mut Storage,
    wal: &mut Wal,
    txn: &mut TxnState,
    name: &str,
    if_not_exists: bool,
    responder: &mut Responder,
) -> Outcome {
    if name.starts_with("pg_") {
        let mut detail = crate::util::StackStr::<512>::new();
        let _ = core::fmt::Write::write_str(
            &mut detail,
            "The prefix \"pg_\" is reserved for system schemas.",
        );
        crate::sql::eval::stash_diagnostic(detail, None);
        return sql_fail(sql_err!(
            crate::sql::eval::sqlstate::RESERVED_NAME,
            "unacceptable schema name \"{}\"",
            name
        ));
    }
    let taken_by_system = name == "information_schema";
    let created = if taken_by_system {
        Err(sql_err!(
            crate::sql::eval::sqlstate::DUPLICATE_SCHEMA,
            "schema \"{}\" already exists",
            name
        ))
    } else {
        let sqlname = match SqlName::parse(name) {
            Ok(n) => n,
            Err(e) => return sql_fail(e),
        };
        storage.create_schema_in(sqlname, txn.txid)
    };
    match created {
        Ok(slot) => {
            let lsn = storage.bump_lsn();
            if let Err(e) = wal.append(lsn, &WalOp::CreateSchema(name)) {
                storage.rollback_schema_create(slot);
                return sql_fail(e);
            }
            if let Err(e) = txn.record_ddl(super::txn::DdlUndo::SchemaCreated(slot as u32)) {
                storage.rollback_schema_create(slot);
                return sql_fail(e);
            }
        }
        Err(e)
            if e.sqlstate == crate::sql::eval::sqlstate::DUPLICATE_SCHEMA
                && if_not_exists =>
        {
            responder.notice(
                crate::sql::eval::sqlstate::DUPLICATE_SCHEMA,
                stack_format!(128, "schema \"{}\" already exists, skipping", name).as_str(),
            )?;
        }
        Err(e) => return sql_fail(e),
    }
    responder.command_complete("CREATE SCHEMA")?;
    sql_ok()
}

/// One object a DROP SCHEMA sweeps up, for dependency reports and the
/// cascaded drops.
#[derive(Clone, Copy)]
enum SchemaObject {
    Table(usize),
    View(usize),
    /// An inbound foreign key on a table that itself survives.
    InboundFk { table: usize, fk_index: usize },
}

/// DROP SCHEMA [IF EXISTS] name [, ...] [CASCADE | RESTRICT]: RESTRICT (the
/// default) refuses a non-empty schema with PostgreSQL's dependency report;
/// CASCADE drops the contained tables and views and severs inbound foreign
/// keys from surviving tables.
pub fn drop_schema(
    storage: &mut Storage,
    wal: &mut Wal,
    txn: &mut TxnState,
    names: &[&str],
    if_exists: bool,
    cascade: bool,
    responder: &mut Responder,
) -> Outcome {
    use core::fmt::Write as _;
    const MAX_DROP_SCHEMAS: usize = 16;
    let mut slots: [usize; MAX_DROP_SCHEMAS] = [0; MAX_DROP_SCHEMAS];
    let mut n_slots = 0usize;
    for name in names {
        if *name == "pg_catalog" || *name == "information_schema" {
            return sql_fail(sql_err!(
                crate::sql::eval::sqlstate::DEPENDENT_OBJECTS_STILL_EXIST,
                "cannot drop schema {} because it is required by the database system",
                name
            ));
        }
        match storage.find_schema_visible(name, txn.txid) {
            Some(slot) => {
                if !slots[..n_slots].contains(&slot) {
                    if n_slots == MAX_DROP_SCHEMAS {
                        return sql_fail(sql_err!(
                            sqlstate::PROGRAM_LIMIT_EXCEEDED,
                            "too many schemas in one DROP SCHEMA"
                        ));
                    }
                    slots[n_slots] = slot;
                    n_slots += 1;
                }
            }
            None if if_exists => {
                responder.notice(
                    crate::sql::eval::sqlstate::SUCCESSFUL_COMPLETION,
                    stack_format!(128, "schema \"{}\" does not exist, skipping", name)
                        .as_str(),
                )?;
            }
            None => {
                return sql_fail(sql_err!(
                    sqlstate::INVALID_SCHEMA_NAME,
                    "schema \"{}\" does not exist",
                    name
                ))
            }
        }
    }
    // Everything the drop sweeps up, across all listed schemas.
    let in_listed = |storage: &Storage, schema: &str| {
        slots[..n_slots]
            .iter()
            .any(|&slot| storage.schema_def(slot).name.as_str() == schema)
    };
    let mut objects: [Option<SchemaObject>; 64] = [const { None }; 64];
    let mut n_objects = 0usize;
    let mut push = |o: SchemaObject, n_objects: &mut usize| -> Result<(), SqlError> {
        if *n_objects == objects.len() {
            return Err(sql_err!(
                sqlstate::PROGRAM_LIMIT_EXCEEDED,
                "DROP SCHEMA sweeps up more than {} objects",
                64
            ));
        }
        objects[*n_objects] = Some(o);
        *n_objects += 1;
        Ok(())
    };
    for t in 0..storage.table_count() {
        if storage.table(t).visible_to(txn.txid)
            && in_listed(storage, storage.table(t).def.schema.as_str())
            && let Err(e) = push(SchemaObject::Table(t), &mut n_objects)
        {
            return sql_fail(e);
        }
    }
    for v in 0..storage.view_count() {
        if storage.view(v).visible_to(txn.txid)
            && in_listed(storage, storage.view(v).schema.as_str())
            && let Err(e) = push(SchemaObject::View(v), &mut n_objects)
        {
            return sql_fail(e);
        }
    }
    // Inbound foreign keys: a surviving table referencing a dropped one loses
    // the constraint (PostgreSQL drops the constraint, not the table).
    for t in 0..storage.table_count() {
        if !storage.table(t).visible_to(txn.txid)
            || in_listed(storage, storage.table(t).def.schema.as_str())
        {
            continue;
        }
        for f in 0..storage.table(t).def.n_fkeys {
            if in_listed(storage, storage.table(t).def.fkeys[f].parent_schema.as_str())
                && let Err(e) = push(SchemaObject::InboundFk { table: t, fk_index: f }, &mut n_objects)
            {
                return sql_fail(e);
            }
        }
    }
    // PostgreSQL walks the listed schemas in reverse, and reports each
    // schema's dependents in OID (creation) order; a severed constraint
    // sorts with its child table, after it.
    let schema_rank = |storage: &Storage, schema: &str| -> usize {
        slots[..n_slots]
            .iter()
            .position(|&slot| storage.schema_def(slot).name.as_str() == schema)
            .map(|p| n_slots - p)
            .unwrap_or(0)
    };
    let sort_key = |storage: &Storage, o: &SchemaObject| -> (usize, u64, u8) {
        match o {
            SchemaObject::Table(t) => {
                let table = storage.table(*t);
                (
                    schema_rank(storage, table.def.schema.as_str()),
                    table.created_at,
                    0,
                )
            }
            SchemaObject::View(v) => {
                let view = storage.view(*v);
                (schema_rank(storage, view.schema.as_str()), view.created_at, 0)
            }
            SchemaObject::InboundFk { table, fk_index } => {
                let child = storage.table(*table);
                (
                    schema_rank(
                        storage,
                        child.def.fkeys[*fk_index].parent_schema.as_str(),
                    ),
                    child.created_at,
                    1,
                )
            }
        }
    };
    {
        let slice = &mut objects[..n_objects];
        slice.sort_unstable_by_key(|o| sort_key(storage, o.as_ref().expect("filled")));
    }
    // Renders one swept object the way PostgreSQL's dependency report does:
    // qualified only when its schema is not on the current search path.
    let in_path = |storage: &Storage, schema: &str| {
        storage.path().entries().iter().any(|e| match e {
            crate::storage::PathEntry::Schema(slot) => {
                storage.schema_def(*slot as usize).name.as_str() == schema
            }
            crate::storage::PathEntry::Catalog => schema == "pg_catalog",
        })
    };
    let describe = |storage: &Storage, o: &SchemaObject, out: &mut crate::util::StackStr<192>| {
        let write_rel = |out: &mut crate::util::StackStr<192>, schema: &SqlName, name: &SqlName| {
            if in_path(storage, schema.as_str()) {
                let _ = write!(out, "{}", name.as_str());
            } else {
                let _ = write!(out, "{}.{}", schema.as_str(), name.as_str());
            }
        };
        match o {
            SchemaObject::Table(t) => {
                let def = &storage.table(*t).def;
                let _ = write!(out, "table ");
                write_rel(out, &def.schema, &def.name);
            }
            SchemaObject::View(v) => {
                let view = storage.view(*v);
                let _ = write!(out, "view ");
                write_rel(out, &view.schema, &view.name);
            }
            SchemaObject::InboundFk { table, fk_index } => {
                let def = &storage.table(*table).def;
                let _ = write!(
                    out,
                    "constraint {} on table ",
                    def.fkeys[*fk_index].name.as_str()
                );
                write_rel(out, &def.schema, &def.name);
            }
        }
    };
    if n_objects > 0 && !cascade {
        let first = slots[0];
        let mut detail = crate::util::StackStr::<512>::new();
        for (i, o) in objects[..n_objects].iter().flatten().enumerate() {
            let mut line = crate::util::StackStr::<192>::new();
            describe(storage, o, &mut line);
            let _ = write!(
                detail,
                "{}{} depends on schema {}",
                if i > 0 { "\n" } else { "" },
                line.as_str(),
                // PostgreSQL's report names the schema each object hangs off;
                // for a multi-schema drop each line names its own.
                match o {
                    SchemaObject::Table(t) => storage.table(*t).def.schema.as_str(),
                    SchemaObject::View(v) => storage.view(*v).schema.as_str(),
                    SchemaObject::InboundFk { table, fk_index } =>
                        storage.table(*table).def.fkeys[*fk_index].parent_schema.as_str(),
                }
            );
        }
        let mut hint = crate::util::StackStr::<128>::new();
        let _ = write!(hint, "Use DROP ... CASCADE to drop the dependent objects too.");
        crate::sql::eval::stash_diagnostic(detail, Some(hint));
        return sql_fail(sql_err!(
            crate::sql::eval::sqlstate::DEPENDENT_OBJECTS_STILL_EXIST,
            "cannot drop schema {} because other objects depend on it",
            storage.schema_def(first).name.as_str()
        ));
    }
    if n_objects == 1 {
        let mut line = crate::util::StackStr::<192>::new();
        describe(storage, objects[0].as_ref().expect("counted"), &mut line);
        responder.notice(
            crate::sql::eval::sqlstate::SUCCESSFUL_COMPLETION,
            stack_format!(224, "drop cascades to {}", line.as_str()).as_str(),
        )?;
    } else if n_objects > 1 {
        let mut detail = crate::util::StackStr::<512>::new();
        for (i, o) in objects[..n_objects].iter().flatten().enumerate() {
            let mut line = crate::util::StackStr::<192>::new();
            describe(storage, o, &mut line);
            let _ = write!(
                detail,
                "{}drop cascades to {}",
                if i > 0 { "\n" } else { "" },
                line.as_str()
            );
        }
        crate::sql::eval::stash_diagnostic(detail, None);
        responder.notice(
            crate::sql::eval::sqlstate::SUCCESSFUL_COMPLETION,
            stack_format!(128, "drop cascades to {} other objects", n_objects).as_str(),
        )?;
    }
    // Apply the cascade: severed constraints first, then views, then tables,
    // then the schemas themselves — the order replay reproduces.
    for o in objects[..n_objects].iter().flatten() {
        match o {
            SchemaObject::InboundFk { table, fk_index } => {
                let def = &storage.table(*table).def;
                let fk_name = def.fkeys[*fk_index].name;
                let (schema, tname) = (def.schema, def.name);
                let lsn = storage.bump_lsn();
                if let Err(e) = wal.append(
                    lsn,
                    &WalOp::DropTableFk {
                        schema: schema.as_str(),
                        table: tname.as_str(),
                        fk_name: fk_name.as_str(),
                    },
                ) {
                    return sql_fail(e);
                }
                let Some(fk) = storage.drop_fk(*table, fk_name.as_str()) else {
                    continue;
                };
                if let Err(e) =
                    txn.record_ddl(super::txn::DdlUndo::FkDropped { table: *table as u32, fk })
                {
                    return sql_fail(e);
                }
            }
            SchemaObject::View(v) => {
                let (schema, vname) = {
                    let view = storage.view(*v);
                    (view.schema, view.name)
                };
                let lsn = storage.bump_lsn();
                if let Err(e) = wal.append(
                    lsn,
                    &WalOp::DropView { schema: schema.as_str(), name: vname.as_str() },
                ) {
                    return sql_fail(e);
                }
                let dropped = match storage.drop_view(schema.as_str(), vname.as_str(), txn.txid)
                {
                    Ok(d) => d,
                    Err(e) => return sql_fail(e),
                };
                if let Some(slot) = dropped
                    && let Err(e) =
                        txn.record_ddl(super::txn::DdlUndo::ViewDropped(slot as u32))
                {
                    return sql_fail(e);
                }
            }
            SchemaObject::Table(t) => {
                if let Some(other) = storage.table(*t).ddl_locked_by_other(txn.txid) {
                    let _ = other;
                    return sql_fail(sql_err!(
                        crate::sql::eval::sqlstate::SERIALIZATION_FAILURE,
                        "could not serialize access due to concurrent DDL on \"{}\"",
                        storage.table(*t).def.name.as_str()
                    ));
                }
                let def = storage.table(*t).def;
                let lsn = storage.bump_lsn();
                if let Err(e) = wal.append(
                    lsn,
                    &WalOp::DropTable {
                        schema: def.schema.as_str(),
                        name: def.name.as_str(),
                    },
                ) {
                    return sql_fail(e);
                }
                if let Err(e) = txn.record_ddl(super::txn::DdlUndo::Dropped(*t as u32)) {
                    return sql_fail(e);
                }
                storage.drop_table_in(*t, txn.txid);
                storage.drop_indexes_for(def.schema.as_str(), def.name.as_str(), txn.txid);
            }
        }
    }
    for &slot in &slots[..n_slots] {
        let name = storage.schema_def(slot).name;
        let lsn = storage.bump_lsn();
        if let Err(e) = wal.append(lsn, &WalOp::DropSchema(name.as_str())) {
            return sql_fail(e);
        }
        if let Err(e) = txn.record_ddl(super::txn::DdlUndo::SchemaDropped(slot as u32)) {
            return sql_fail(e);
        }
        storage.drop_schema_in(slot, txn.txid);
    }
    responder.command_complete("DROP SCHEMA")?;
    sql_ok()
}

/// CREATE [OR REPLACE] VIEW: stores the view's SELECT text durably (journaled
/// and checkpointed) and registers it. View DDL is applied immediately, not
/// rolled back with the surrounding transaction (see BUGS.md).
#[allow(clippy::too_many_arguments)]
pub fn create_view(
    storage: &mut Storage,
    wal: &mut Wal,
    txn: &mut super::txn::TxnState,
    name: &QualName,
    or_replace: bool,
    sql: &str,
    raw_path: &str,
    arena: &Arena,
    responder: &mut Responder,
) -> Outcome {
    use core::fmt::Write;
    let mut buffer = crate::util::StackStr::<{ crate::storage::VIEW_SQL_MAX }>::new();
    let _ = write!(buffer, "{sql}");
    if buffer.is_truncated() {
        return sql_fail(sql_err!(
            sqlstate::PROGRAM_LIMIT_EXCEEDED,
            "view definition exceeds {} bytes",
            crate::storage::VIEW_SQL_MAX
        ));
    }
    // Validate the definition now (tables/views exist, columns resolve), as
    // PostgreSQL does at CREATE VIEW time.
    if let Err(e) = super::query::validate_view(buffer.as_str(), storage, txn.txid, arena) {
        return sql_fail(e);
    }
    let schema = match storage.creation_schema(name.schema, name.name, txn.txid) {
        Ok(n) => n,
        Err(e) => return sql_fail(e),
    };
    let sqlname = match SqlName::parse(name.name) {
        Ok(n) => n,
        Err(e) => return sql_fail(e),
    };
    let mut creation_path = crate::util::StackStr::<128>::new();
    let _ = core::fmt::Write::write_str(&mut creation_path, raw_path);
    if creation_path.is_truncated() {
        return sql_fail(sql_err!(
            sqlstate::PROGRAM_LIMIT_EXCEEDED,
            "search_path is too long to store with a view"
        ));
    }
    match storage.create_view(schema, sqlname, buffer, creation_path, or_replace, txn.txid) {
        Ok((new_slot, old_slot)) => {
            let lsn = storage.bump_lsn();
            if let Err(e) = wal.append(
                lsn,
                &WalOp::CreateView {
                    schema: schema.as_str(),
                    name: name.name,
                    sql,
                    path: raw_path,
                },
            ) {
                // The journal rejected the record; undo the in-memory apply.
                storage.rollback_view_create(new_slot);
                if let Some(o) = old_slot {
                    storage.rollback_view_drop(o, txn.txid);
                }
                return sql_fail(e);
            }
            // Rollback undo: drop the new view; revive any superseded one.
            if let Err(e) = txn.record_ddl(super::txn::DdlUndo::ViewCreated(new_slot as u32)) {
                return sql_fail(e);
            }
            if let Some(o) = old_slot
                && let Err(e) = txn.record_ddl(super::txn::DdlUndo::ViewDropped(o as u32))
            {
                return sql_fail(e);
            }
        }
        Err(e) => return sql_fail(e),
    }
    responder.command_complete("CREATE VIEW")?;
    sql_ok()
}

/// `COMMENT ON <object> IS { 'text' | NULL }`. Resolves and kind-checks the
/// object, applies the comment as this transaction's uncommitted overlay, and
/// journals it (promoted on commit, discarded on rollback — like other DDL).
pub fn comment(
    storage: &mut Storage,
    wal: &mut Wal,
    txn: &mut TxnState,
    target: &super::ast::CommentTarget,
    text: Option<&str>,
    responder: &mut Responder,
) -> Outcome {
    use crate::storage::{CommentClass, StoredRelKind};
    use super::ast::CommentTarget;

    let txid = txn.txid;
    // Resolve the target to a comment key `(class, schema, name, subid)`,
    // matching PostgreSQL's resolution and its error wording.
    let (class, schema, name, subid) = match *target {
        CommentTarget::Relation { kind, name: rel } => {
            let Some((schema, actual)) = storage.classify_relation(rel.schema, rel.name, txid)
            else {
                return sql_fail(sql_err!(
                    sqlstate::UNDEFINED_TABLE,
                    "relation \"{}\" does not exist",
                    rel.name
                ));
            };
            let ok = matches!(
                (kind, actual),
                (super::ast::CommentRelKind::Table, StoredRelKind::Table)
                    | (super::ast::CommentRelKind::View, StoredRelKind::View)
                    | (super::ast::CommentRelKind::MaterializedView, StoredRelKind::Matview)
                    | (super::ast::CommentRelKind::Index, StoredRelKind::Index)
                    | (super::ast::CommentRelKind::Sequence, StoredRelKind::Sequence)
            );
            if !ok {
                return sql_fail(sql_err!(
                    sqlstate::WRONG_OBJECT_TYPE,
                    "\"{}\" is not a{} {}",
                    rel.name,
                    if kind.noun().starts_with(['a', 'e', 'i', 'o', 'u']) { "n" } else { "" },
                    kind.noun()
                ));
            }
            let stored = match SqlName::parse(rel.name) {
                Ok(n) => n,
                Err(e) => return sql_fail(e),
            };
            (CommentClass::Relation, schema, stored, 0u32)
        }
        CommentTarget::Column { relation, column } => {
            let Some((schema, actual)) =
                storage.classify_relation(relation.schema, relation.name, txid)
            else {
                return sql_fail(sql_err!(
                    sqlstate::UNDEFINED_TABLE,
                    "relation \"{}\" does not exist",
                    relation.name
                ));
            };
            if !matches!(actual, StoredRelKind::Table | StoredRelKind::Matview) {
                // Column comments on views/sequences/indexes need the
                // relation's column list resolved from its body, which our
                // stored catalog does not carry for non-tables.
                return sql_fail(sql_err!(
                    sqlstate::FEATURE_NOT_SUPPORTED,
                    "COMMENT ON COLUMN is supported only for tables and materialized views"
                ));
            }
            let slot = storage
                .find_visible(schema.as_str(), relation.name, txid)
                .expect("classified relation resolves to a table slot");
            let Some(attnum) = storage.column_number(slot, column) else {
                return sql_fail(sql_err!(
                    sqlstate::UNDEFINED_COLUMN,
                    "column \"{}\" of relation \"{}\" does not exist",
                    column,
                    relation.name
                ));
            };
            let stored = match SqlName::parse(relation.name) {
                Ok(n) => n,
                Err(e) => return sql_fail(e),
            };
            (CommentClass::Relation, schema, stored, attnum)
        }
        CommentTarget::Schema(schema_name) => {
            if storage.find_schema_visible(schema_name, txid).is_none() {
                return sql_fail(sql_err!(
                    sqlstate::INVALID_SCHEMA_NAME,
                    "schema \"{}\" does not exist",
                    schema_name
                ));
            }
            let stored = match SqlName::parse(schema_name) {
                Ok(n) => n,
                Err(e) => return sql_fail(e),
            };
            (CommentClass::Schema, SqlName::EMPTY, stored, 0u32)
        }
    };

    let stored_text = match text {
        Some(t) => match crate::storage::comment_stackstr(t) {
            Ok(s) => Some(s),
            Err(e) => return sql_fail(e),
        },
        None => None,
    };

    let (slot, prior) =
        match storage.set_comment(class, schema, name, subid, stored_text, txid) {
            Ok(v) => v,
            Err(e) => return sql_fail(e),
        };
    let lsn = storage.bump_lsn();
    if let Err(e) = wal.append(
        lsn,
        &WalOp::Comment {
            class: class.to_u8(),
            schema: schema.as_str(),
            name: name.as_str(),
            subid,
            text,
        },
    ) {
        // The journal rejected the record; undo the in-memory overlay.
        storage.restore_comment_pending(slot, prior);
        return sql_fail(e);
    }
    if let Err(e) = txn.record_ddl(super::txn::DdlUndo::CommentSet { slot: slot as u32, prior }) {
        storage.restore_comment_pending(slot, prior);
        return sql_fail(e);
    }
    responder.command_complete("COMMENT")?;
    sql_ok()
}

/// CREATE TABLE ... AS <query> [WITH [NO] DATA]: build a table from the query's
/// output schema, then populate it by running the query once.
#[allow(clippy::too_many_arguments)]
pub fn create_table_as(
    storage: &mut Storage,
    wal: &mut Wal,
    txn: &mut TxnState,
    name: &crate::sql::ast::QualName,
    rename: &[&str],
    sql: &str,
    with_data: bool,
    if_not_exists: bool,
    materialized: bool,
    raw_path: &str,
    arena: &Arena,
    params: &[Datum],
    responder: &mut Responder,
) -> Outcome {
    use crate::storage::{ColumnMeta, SqlName, TableDef};
    // Resolve the query's output columns without running it.
    let mut columns = [crate::sql::types::ColDesc::new("", 0, 0); MAX_PROJ];
    let n_cols = match super::query::describe_query(sql, storage, txn.txid, arena, &mut columns) {
        Ok(n) => n,
        Err(e) => return sql_fail(e),
    };
    if !rename.is_empty() && rename.len() != n_cols {
        return sql_fail(sql_err!(
            sqlstate::SYNTAX_ERROR,
            "CREATE TABLE AS specifies too {} column names",
            if rename.len() > n_cols { "many" } else { "few" }
        ));
    }
    // Build the backing table's definition from those columns.
    let mut def = TableDef::empty();
    def.schema = match storage.creation_schema(name.schema, name.name, txn.txid) {
        Ok(s) => s,
        Err(e) => return sql_fail(e),
    };
    def.name = match SqlName::parse(name.name) {
        Ok(n) => n,
        Err(e) => return sql_fail(e),
    };
    def.n_columns = n_cols;
    for i in 0..n_cols {
        let Some(ctype) = coltype_of_oid(columns[i].type_oid) else {
            return sql_fail(sql_err!(
                sqlstate::FEATURE_NOT_SUPPORTED,
                "CREATE TABLE AS cannot materialize column {} (type oid {})",
                i + 1,
                columns[i].type_oid
            ));
        };
        let col_name = if rename.is_empty() { columns[i].name } else { rename[i] };
        let parsed = match SqlName::parse(col_name) {
            Ok(n) => n,
            Err(e) => return sql_fail(e),
        };
        def.columns[i] = ColumnMeta {
            name: parsed,
            ctype,
            type_mod: columns[i].type_mod,
            not_null: false,
            unique: false,
            primary: false,
            auto_increment: false,
            default_value: None,
            default_expr: None,
            is_generated: false,
            is_identity: false,
            identity_always: false,
            auto_increment_step: 1,
            domain: None,
        };
    }
    // Create the empty table, journaled — exactly as CREATE TABLE does.
    let table_index = match storage.create_table_in(def, txn.txid) {
        Ok(slot) => {
            let lsn = storage.bump_lsn();
            if let Err(e) = wal.append(lsn, &WalOp::CreateTable(def)) {
                storage.rollback_create(slot);
                return sql_fail(e);
            }
            if let Err(e) = txn.record_ddl(super::txn::DdlUndo::Created(slot as u32)) {
                storage.rollback_create(slot);
                return sql_fail(e);
            }
            slot
        }
        Err(e) if e.sqlstate == sqlstate::DUPLICATE_TABLE && if_not_exists => {
            responder.notice(
                sqlstate::DUPLICATE_TABLE,
                stack_format!(128, "relation \"{}\" already exists, skipping", name.name).as_str(),
            )?;
            responder.command_complete("CREATE TABLE AS")?;
            return sql_ok();
        }
        Err(e) => return sql_fail(e),
    };
    // Populate, unless WITH NO DATA. Two passes: materialize the query's rows
    // into the arena (reading storage immutably), then store them (mutably) —
    // the source could reference another table, so the phases must not overlap.
    let mut count = 0u64;
    if with_data {
        let sel = match crate::sql::parser::parse_query(sql, arena) {
            Ok(s) => s,
            Err(e) => return sql_fail(e),
        };
        let mut rows = 0usize;
        if let Err(e) = super::query::select_into_rows(
            storage, txn.txid, sel, arena, params, None, None, &mut |_| {
                rows += 1;
                Ok(())
            },
        ) {
            return sql_fail(e);
        }
        let empty: &[u8] = &[];
        let rows_bytes: &mut [&[u8]] = match arena.alloc_slice_with(rows, |_| empty) {
            Ok(r) => r,
            Err(_) => {
                return sql_fail(sql_err!(
                    sqlstate::PROGRAM_LIMIT_EXCEEDED,
                    "CREATE TABLE AS result exceeds the statement arena"
                ))
            }
        };
        let mut at = 0usize;
        if let Err(e) = super::query::select_into_rows(
            storage, txn.txid, sel, arena, params, None, None, &mut |vals| {
                rows_bytes[at] = encode_projected_pub(vals, arena)?;
                at += 1;
                Ok(())
            },
        ) {
            return sql_fail(e);
        }
        for bytes in rows_bytes.iter() {
            let mut values = [Datum::Null; MAX_COLUMNS];
            for (i, slot) in values.iter_mut().enumerate().take(n_cols) {
                let v = decode_projected_pub(bytes, i);
                match coerce(v, &def.columns()[i], storage, arena) {
                    Ok(v) => *slot = v,
                    Err(e) => return sql_fail(e),
                }
            }
            if let Err(e) = store_row(storage, txn, table_index, None, &values[..n_cols]) {
                return sql_fail(e);
            }
            count += 1;
        }
    }
    // A materialized view additionally records its defining query (for REFRESH)
    // and its populated state in the parallel matview catalog. Its rows are the
    // backing table's, which we just filled.
    if materialized {
        use core::fmt::Write;
        let mut buffer = crate::util::StackStr::<{ crate::storage::VIEW_SQL_MAX }>::new();
        let _ = write!(buffer, "{sql}");
        if buffer.is_truncated() {
            return sql_fail(sql_err!(
                sqlstate::PROGRAM_LIMIT_EXCEEDED,
                "materialized view query exceeds {} bytes",
                crate::storage::VIEW_SQL_MAX
            ));
        }
        let mut cpath = crate::util::StackStr::<128>::new();
        let _ = write!(cpath, "{raw_path}");
        match storage.create_matview(def.schema, def.name, buffer, cpath, with_data, txn.txid) {
            Ok(slot) => {
                let lsn = storage.bump_lsn();
                if let Err(e) = wal.append(
                    lsn,
                    &WalOp::CreateMatview {
                        schema: def.schema.as_str(),
                        name: name.name,
                        sql,
                        path: raw_path,
                        populated: with_data,
                    },
                ) {
                    storage.rollback_matview_create(slot);
                    return sql_fail(e);
                }
                if let Err(e) = txn.record_ddl(super::txn::DdlUndo::MatviewCreated(slot as u32)) {
                    storage.rollback_matview_create(slot);
                    return sql_fail(e);
                }
            }
            Err(e) => return sql_fail(e),
        }
    }
    responder.command_complete(stack_format!(32, "SELECT {count}").as_str())?;
    sql_ok()
}

/// REFRESH MATERIALIZED VIEW: re-run the stored query, replacing every row of
/// the backing table.
pub fn refresh_materialized_view(
    storage: &mut Storage,
    wal: &mut Wal,
    txn: &mut TxnState,
    name: &crate::sql::ast::QualName,
    arena: &Arena,
    params: &[Datum],
    responder: &mut Responder,
) -> Outcome {
    let table_index = match storage.resolve_relation(name.schema, name.name, txn.txid) {
        Some(crate::storage::ResolvedRelation::Table(idx)) => idx,
        _ => return sql_fail(undefined_kind("relation", name.name)),
    };
    let def = storage.table(table_index).def;
    let Some(slot) = storage.matview_slot(def.schema.as_str(), def.name.as_str(), txn.txid) else {
        return sql_fail(sql_err!(
            sqlstate::WRONG_OBJECT_TYPE,
            "\"{}\" is not a materialized view",
            name.name
        ));
    };
    // Copy the stored query out before mutating storage.
    let sql = match arena.alloc_str(storage.matview(slot).sql.as_str()) {
        Ok(s) => s,
        Err(_) => {
            return sql_fail(sql_err!(
                sqlstate::PROGRAM_LIMIT_EXCEEDED,
                "materialized view query exceeds the statement arena"
            ))
        }
    };
    // Remove every visible row, transactionally (a matview has no constraints).
    let mut rowids: [u64; 4096] = [0; 4096];
    loop {
        let mut count = 0usize;
        let _ = storage.for_each_row_state(table_index, &mut |rowid, state| {
            use core::ops::ControlFlow;
            if state.visible_at(txn.txid, storage.read_snapshot()).is_none() {
                return Ok(ControlFlow::Continue(()));
            }
            if count == rowids.len() {
                return Ok(ControlFlow::Break(()));
            }
            rowids[count] = rowid;
            count += 1;
            Ok(ControlFlow::Continue(()))
        });
        if count == 0 {
            break;
        }
        for &rowid in &rowids[..count] {
            match storage.write_pending(table_index, rowid, txn.txid, txn.command_id(), None) {
                Ok(prior) => {
                    if let Err(e) = txn.touch(table_index as u32, rowid, prior) {
                        storage.restore_pending(table_index, rowid, txn.txid, prior);
                        return sql_fail(e);
                    }
                }
                Err(e) => return sql_fail(e),
            }
        }
    }
    // Re-run the query and store its rows into the backing table (two-pass, so
    // the source may read another table without overlapping the write).
    let sel = match crate::sql::parser::parse_query(sql, arena) {
        Ok(s) => s,
        Err(e) => return sql_fail(e),
    };
    let mut rows = 0usize;
    if let Err(e) = super::query::select_into_rows(
        storage, txn.txid, sel, arena, params, None, None, &mut |_| {
            rows += 1;
            Ok(())
        },
    ) {
        return sql_fail(e);
    }
    let empty: &[u8] = &[];
    let rows_bytes: &mut [&[u8]] = match arena.alloc_slice_with(rows, |_| empty) {
        Ok(r) => r,
        Err(_) => {
            return sql_fail(sql_err!(
                sqlstate::PROGRAM_LIMIT_EXCEEDED,
                "REFRESH result exceeds the statement arena"
            ))
        }
    };
    let mut at = 0usize;
    if let Err(e) = super::query::select_into_rows(
        storage, txn.txid, sel, arena, params, None, None, &mut |vals| {
            rows_bytes[at] = encode_projected_pub(vals, arena)?;
            at += 1;
            Ok(())
        },
    ) {
        return sql_fail(e);
    }
    let n_cols = def.n_columns;
    for bytes in rows_bytes.iter() {
        let mut values = [Datum::Null; MAX_COLUMNS];
        for (i, slot_v) in values.iter_mut().enumerate().take(n_cols) {
            let v = decode_projected_pub(bytes, i);
            match coerce(v, &def.columns()[i], storage, arena) {
                Ok(v) => *slot_v = v,
                Err(e) => return sql_fail(e),
            }
        }
        if let Err(e) = store_row(storage, txn, table_index, None, &values[..n_cols]) {
            return sql_fail(e);
        }
    }
    // Mark it populated (durably).
    storage.set_matview_populated(slot, true);
    let lsn = storage.bump_lsn();
    if let Err(e) = wal.append(
        lsn,
        &WalOp::SetMatviewPopulated {
            schema: def.schema.as_str(),
            name: def.name.as_str(),
            populated: true,
        },
    ) {
        return sql_fail(e);
    }
    responder.command_complete("REFRESH MATERIALIZED VIEW")?;
    sql_ok()
}

/// DROP MATERIALIZED VIEW [IF EXISTS]: drops the backing table and its matview
/// catalog entry.
pub fn drop_materialized_view(
    storage: &mut Storage,
    wal: &mut Wal,
    txn: &mut TxnState,
    names: &[crate::sql::ast::QualName],
    if_exists: bool,
    responder: &mut Responder,
) -> Outcome {
    for name in names {
        let idx = match storage.resolve_relation(name.schema, name.name, txn.txid) {
            Some(crate::storage::ResolvedRelation::Table(idx))
                if storage
                    .matview_slot(
                        storage.table(idx).def.schema.as_str(),
                        storage.table(idx).def.name.as_str(),
                        txn.txid,
                    )
                    .is_some() =>
            {
                idx
            }
            // A relation that exists but is not a materialized view is a type
            // error (42809), which IF EXISTS does not suppress.
            Some(_) => {
                return sql_fail(sql_err!(
                    sqlstate::WRONG_OBJECT_TYPE,
                    "\"{}\" is not a materialized view",
                    name.name
                ))
            }
            None if if_exists => {
                responder.notice(
                    crate::sql::eval::sqlstate::SUCCESSFUL_COMPLETION,
                    stack_format!(128, "materialized view \"{}\" does not exist, skipping", name.name)
                        .as_str(),
                )?;
                continue;
            }
            None => return sql_fail(undefined_kind("materialized view", name.name)),
        };
        let def = storage.table(idx).def;
        // Drop the backing table.
        let lsn = storage.bump_lsn();
        if let Err(e) = wal.append(
            lsn,
            &WalOp::DropTable { schema: def.schema.as_str(), name: def.name.as_str() },
        ) {
            return sql_fail(e);
        }
        if let Err(e) = txn.record_ddl(super::txn::DdlUndo::Dropped(idx as u32)) {
            return sql_fail(e);
        }
        storage.drop_table_in(idx, txn.txid);
        storage.drop_indexes_for(def.schema.as_str(), def.name.as_str(), txn.txid);
        // Drop the matview catalog entry.
        let lsn = storage.bump_lsn();
        if let Err(e) = wal.append(
            lsn,
            &WalOp::DropMatview { schema: def.schema.as_str(), name: def.name.as_str() },
        ) {
            return sql_fail(e);
        }
        match storage.drop_matview(def.schema.as_str(), def.name.as_str(), txn.txid) {
            Ok(Some(slot)) => {
                if let Err(e) = txn.record_ddl(super::txn::DdlUndo::MatviewDropped(slot as u32)) {
                    return sql_fail(e);
                }
            }
            Ok(None) => {}
            Err(e) => return sql_fail(e),
        }
    }
    responder.command_complete("DROP MATERIALIZED VIEW")?;
    sql_ok()
}

/// Maps an `AS <type>` name to a [`SeqType`]; unknown types are rejected exactly
/// as PostgreSQL rejects them.
fn seq_type_of(name: &str) -> Result<SeqType, SqlError> {
    match name {
        "smallint" | "int2" => Ok(SeqType::Smallint),
        "integer" | "int" | "int4" => Ok(SeqType::Integer),
        "bigint" | "int8" => Ok(SeqType::Bigint),
        _ => Err(sql_err!(
            sqlstate::INVALID_PARAMETER_VALUE,
            "sequence type must be smallint, integer, or bigint"
        )),
    }
}

/// Computes and validates a [`SeqSpec`] from parsed options, against real
/// PostgreSQL's rules and SQLSTATEs. `base` is the current spec for ALTER (an
/// omitted option keeps its current value); `None` for CREATE (omitted options
/// take their defaults). Returns the spec and any RESTART target.
fn resolve_seq_spec(
    options: &crate::sql::ast::SeqOptions,
    base: Option<SeqSpec>,
) -> Result<(SeqSpec, Option<i64>), SqlError> {
    use crate::sql::ast::SeqBound;
    let data_type = match options.data_type {
        Some(n) => seq_type_of(n)?,
        None => base.map(|b| b.data_type).unwrap_or(SeqType::Bigint),
    };
    let increment = options
        .increment
        .or(base.map(|b| b.increment))
        .unwrap_or(1);
    if increment == 0 {
        return Err(sql_err!(
            sqlstate::INVALID_PARAMETER_VALUE,
            "INCREMENT must not be zero"
        ));
    }
    let ascending = increment > 0;
    let (type_min, type_max) = data_type.bounds();
    let default_min = if ascending { 1 } else { type_min };
    let default_max = if ascending { type_max } else { -1 };
    let min_value = match options.min_value {
        SeqBound::Value(v) => {
            if v < type_min || v > type_max {
                return Err(sql_err!(
                    sqlstate::INVALID_PARAMETER_VALUE,
                    "MINVALUE ({}) is out of range for sequence data type {}",
                    v,
                    data_type.sql_name()
                ));
            }
            v
        }
        SeqBound::NoBound => default_min,
        SeqBound::Unset => base.map(|b| b.min_value).unwrap_or(default_min),
    };
    let max_value = match options.max_value {
        SeqBound::Value(v) => {
            if v < type_min || v > type_max {
                return Err(sql_err!(
                    sqlstate::INVALID_PARAMETER_VALUE,
                    "MAXVALUE ({}) is out of range for sequence data type {}",
                    v,
                    data_type.sql_name()
                ));
            }
            v
        }
        SeqBound::NoBound => default_max,
        SeqBound::Unset => base.map(|b| b.max_value).unwrap_or(default_max),
    };
    if min_value >= max_value {
        return Err(sql_err!(
            sqlstate::INVALID_PARAMETER_VALUE,
            "MINVALUE ({}) must be less than MAXVALUE ({})",
            min_value,
            max_value
        ));
    }
    let default_start = if ascending { min_value } else { max_value };
    let start_value = options
        .start
        .or(base.map(|b| b.start_value))
        .unwrap_or(default_start);
    if start_value < min_value {
        return Err(sql_err!(
            sqlstate::INVALID_PARAMETER_VALUE,
            "START value ({}) cannot be less than MINVALUE ({})",
            start_value,
            min_value
        ));
    }
    if start_value > max_value {
        return Err(sql_err!(
            sqlstate::INVALID_PARAMETER_VALUE,
            "START value ({}) cannot be greater than MAXVALUE ({})",
            start_value,
            max_value
        ));
    }
    let cache = options.cache.or(base.map(|b| b.cache)).unwrap_or(1);
    if cache < 1 {
        return Err(sql_err!(
            sqlstate::INVALID_PARAMETER_VALUE,
            "CACHE ({}) must be greater than zero",
            cache
        ));
    }
    let cycle = options.cycle.or(base.map(|b| b.cycle)).unwrap_or(false);
    // RESTART [WITH n]: n defaults to the (new) start value; validate it is in
    // range, matching PostgreSQL.
    let restart = match options.restart {
        None => None,
        Some(explicit) => {
            let target = explicit.unwrap_or(start_value);
            if target < min_value || target > max_value {
                return Err(sql_err!(
                    sqlstate::INVALID_PARAMETER_VALUE,
                    "RESTART value ({}) cannot be less than MINVALUE ({})",
                    target,
                    min_value
                ));
            }
            Some(target)
        }
    };
    Ok((
        SeqSpec { data_type, increment, min_value, max_value, start_value, cache, cycle },
        restart,
    ))
}

/// CREATE SEQUENCE [IF NOT EXISTS].
pub fn create_sequence(
    storage: &mut Storage,
    wal: &mut Wal,
    txn: &mut TxnState,
    name: &QualName,
    if_not_exists: bool,
    options: &crate::sql::ast::SeqOptions,
    responder: &mut Responder,
) -> Outcome {
    let schema = match storage.creation_schema(name.schema, name.name, txn.txid) {
        Ok(n) => n,
        Err(e) => return sql_fail(e),
    };
    if storage.relation_name_taken(schema.as_str(), name.name, txn.txid) {
        if if_not_exists {
            responder.notice(
                sqlstate::DUPLICATE_TABLE,
                stack_format!(128, "relation \"{}\" already exists, skipping", name.name).as_str(),
            )?;
            responder.command_complete("CREATE SEQUENCE")?;
            return sql_ok();
        }
        return sql_fail(sql_err!(
            sqlstate::DUPLICATE_TABLE,
            "relation \"{}\" already exists",
            name.name
        ));
    }
    let (spec, _) = match resolve_seq_spec(options, None) {
        Ok(v) => v,
        Err(e) => return sql_fail(e),
    };
    let sqlname = match SqlName::parse(name.name) {
        Ok(n) => n,
        Err(e) => return sql_fail(e),
    };
    let slot = match storage.create_sequence(schema, sqlname, spec, txn.txid) {
        Ok(slot) => slot,
        Err(e) => return sql_fail(e),
    };
    let lsn = storage.bump_lsn();
    if let Err(e) = wal.append(
        lsn,
        &WalOp::CreateSequence {
            schema: schema.as_str(),
            name: name.name,
            data_type: spec.data_type.to_u8(),
            increment: spec.increment,
            min_value: spec.min_value,
            max_value: spec.max_value,
            start_value: spec.start_value,
            cache: spec.cache,
            cycle: spec.cycle,
        },
    ) {
        storage.rollback_sequence_create(slot);
        return sql_fail(e);
    }
    if let Err(e) = txn.record_ddl(super::txn::DdlUndo::SequenceCreated(slot as u32)) {
        return sql_fail(e);
    }
    responder.command_complete("CREATE SEQUENCE")?;
    sql_ok()
}

/// ALTER SEQUENCE [IF EXISTS] — redefine parameters (and optionally RESTART).
pub fn alter_sequence(
    storage: &mut Storage,
    wal: &mut Wal,
    txn: &mut TxnState,
    name: &QualName,
    if_exists: bool,
    options: &crate::sql::ast::SeqOptions,
    responder: &mut Responder,
) -> Outcome {
    let slot = match resolve_sequence(storage, name, txn.txid) {
        Ok(Some(slot)) => slot,
        Ok(None) if if_exists => {
            responder.notice(
                crate::sql::eval::sqlstate::SUCCESSFUL_COMPLETION,
                stack_format!(128, "sequence \"{}\" does not exist, skipping", name.name).as_str(),
            )?;
            responder.command_complete("ALTER SEQUENCE")?;
            return sql_ok();
        }
        Ok(None) => return sql_fail(undefined_kind("sequence", name.name)),
        Err(e) => return sql_fail(e),
    };
    let base = {
        let s = storage.sequence(slot);
        SeqSpec {
            data_type: s.data_type,
            increment: s.increment,
            min_value: s.min_value,
            max_value: s.max_value,
            start_value: s.start_value,
            cache: s.cache,
            cycle: s.cycle,
        }
    };
    let (spec, restart) = match resolve_seq_spec(options, Some(base)) {
        Ok(v) => v,
        Err(e) => return sql_fail(e),
    };
    storage.alter_sequence(slot, spec, restart);
    let (schema, sname) = {
        let s = storage.sequence(slot);
        (s.schema, s.name)
    };
    // The redefinition journals as a CreateSequence (absolute parameters).
    let lsn = storage.bump_lsn();
    if let Err(e) = wal.append(
        lsn,
        &WalOp::CreateSequence {
            schema: schema.as_str(),
            name: sname.as_str(),
            data_type: spec.data_type.to_u8(),
            increment: spec.increment,
            min_value: spec.min_value,
            max_value: spec.max_value,
            start_value: spec.start_value,
            cache: spec.cache,
            cycle: spec.cycle,
        },
    ) {
        return sql_fail(e);
    }
    // A RESTART changed value state; journal the absolute advance too.
    if restart.is_some() {
        let s = storage.sequence(slot);
        let (last, is_called) = (s.last_value.get(), s.is_called.get());
        s.dirty.set(false);
        let lsn = storage.bump_lsn();
        if let Err(e) = wal.append(
            lsn,
            &WalOp::SequenceAdvance {
                schema: schema.as_str(),
                name: sname.as_str(),
                last,
                is_called,
            },
        ) {
            return sql_fail(e);
        }
    }
    responder.command_complete("ALTER SEQUENCE")?;
    sql_ok()
}

/// DROP SEQUENCE [IF EXISTS].
pub fn drop_sequence(
    storage: &mut Storage,
    wal: &mut Wal,
    txn: &mut TxnState,
    names: &[crate::sql::ast::QualName],
    if_exists: bool,
    responder: &mut Responder,
) -> Outcome {
    for name in names {
        let slot = match resolve_sequence(storage, name, txn.txid) {
            Ok(Some(slot)) => slot,
            Ok(None) if if_exists => {
                responder.notice(
                    crate::sql::eval::sqlstate::SUCCESSFUL_COMPLETION,
                    stack_format!(128, "sequence \"{}\" does not exist, skipping", name.name)
                        .as_str(),
                )?;
                continue;
            }
            Ok(None) => return sql_fail(undefined_kind("sequence", name.name)),
            Err(e) => return sql_fail(e),
        };
        let (schema, sname) = {
            let s = storage.sequence(slot);
            (s.schema, s.name)
        };
        let lsn = storage.bump_lsn();
        if let Err(e) = wal.append(
            lsn,
            &WalOp::DropSequence { schema: schema.as_str(), name: sname.as_str() },
        ) {
            return sql_fail(e);
        }
        match storage.drop_sequence(schema.as_str(), sname.as_str(), txn.txid) {
            Ok(Some(slot)) => {
                if let Err(e) = txn.record_ddl(super::txn::DdlUndo::SequenceDropped(slot as u32)) {
                    return sql_fail(e);
                }
            }
            Ok(None) => {}
            Err(e) => return sql_fail(e),
        }
    }
    responder.command_complete("DROP SEQUENCE")?;
    sql_ok()
}

// --- CREATE / ALTER / DROP DOMAIN --------------------------------------------

/// Copies domain text (a DEFAULT or CHECK source) into a fixed buffer, or a loud
/// `PROGRAM_LIMIT_EXCEEDED` if it is longer than `N`.
fn domain_text<const N: usize>(text: &str) -> Result<StackStr<N>, SqlError> {
    use core::fmt::Write as _;
    let mut out = StackStr::<N>::new();
    let _ = write!(out, "{text}");
    if out.is_truncated() {
        return Err(sql_err!(
            sqlstate::PROGRAM_LIMIT_EXCEEDED,
            "domain constraint or default exceeds {} bytes",
            N
        ));
    }
    Ok(out)
}

/// Validates that a domain CHECK/DEFAULT expression parses and references no
/// column other than `VALUE` (PostgreSQL's placeholder for the input value).
fn validate_domain_expr(
    text: &str,
    allow_value: bool,
    arena: &Arena,
) -> Result<(), SqlError> {
    let expr = crate::sql::parser::parse_expr(text, arena)?;
    let mut bad: Option<SqlError> = None;
    expr.for_each_column(&mut |name| {
        if bad.is_some() {
            return;
        }
        if !(allow_value && name.eq_ignore_ascii_case("value")) {
            bad = Some(sql_err!(sqlstate::UNDEFINED_COLUMN, "column \"{}\" does not exist", name));
        }
    });
    match bad {
        Some(e) => Err(e),
        None => Ok(()),
    }
}

/// Builds the validated [`DomainSpec`] from a base type, its typmod, and the
/// domain's constraints; unnamed CHECKs get PostgreSQL-style `<domain>_check`
/// names.
#[allow(clippy::too_many_arguments)]
fn build_domain_spec(
    domain: &str,
    base_type: &str,
    base_type_mod: i32,
    not_null: bool,
    default_text: Option<&str>,
    ast_checks: &[crate::sql::ast::DomainCheck],
    arena: &Arena,
) -> Result<crate::storage::DomainSpec, SqlError> {
    let base = ColType::from_sql_name(base_type).ok_or_else(|| {
        sql_err!(sqlstate::UNDEFINED_OBJECT, "type \"{}\" does not exist", base_type)
    })?;
    let default_expr = match default_text {
        Some(t) => {
            validate_domain_expr(t, false, arena)?;
            Some(domain_text::<{ crate::storage::DEFAULT_EXPR_MAX }>(t)?)
        }
        None => None,
    };
    let mut checks = [crate::storage::CheckConstraint::EMPTY; crate::storage::MAX_DOMAIN_CHECKS];
    let mut n = 0;
    for c in ast_checks {
        if n == crate::storage::MAX_DOMAIN_CHECKS {
            return Err(sql_err!(
                sqlstate::PROGRAM_LIMIT_EXCEEDED,
                "a domain may have at most {} CHECK constraints",
                crate::storage::MAX_DOMAIN_CHECKS
            ));
        }
        validate_domain_expr(c.expression, true, arena)?;
        let name = match c.name {
            Some(nm) => SqlName::parse(nm)?,
            None => generate_check_name(domain, &checks[..n])?,
        };
        checks[n] = crate::storage::CheckConstraint {
            name,
            expression: domain_text::<{ crate::storage::CHECK_SQL_MAX }>(c.expression)?,
        };
        n += 1;
    }
    Ok(crate::storage::DomainSpec { base, base_type_mod, not_null, default_expr, checks, n_checks: n })
}

/// PostgreSQL's unnamed-constraint naming for a domain CHECK: `<domain>_check`,
/// then `<domain>_check1`, `<domain>_check2`, … avoiding names already used.
fn generate_check_name(
    domain: &str,
    existing: &[crate::storage::CheckConstraint],
) -> Result<SqlName, SqlError> {
    for suffix in 0..1000usize {
        let candidate = if suffix == 0 {
            stack_format!(128, "{}_check", domain)
        } else {
            stack_format!(128, "{}_check{}", domain, suffix)
        };
        if !existing.iter().any(|c| c.name.as_str() == candidate.as_str()) {
            return SqlName::parse(candidate.as_str());
        }
    }
    Err(sql_err!(sqlstate::PROGRAM_LIMIT_EXCEEDED, "cannot name domain CHECK constraint"))
}

pub fn create_domain(
    storage: &mut Storage,
    wal: &mut Wal,
    txn: &mut TxnState,
    d: &crate::sql::ast::CreateDomain,
    arena: &Arena,
    responder: &mut Responder,
) -> Outcome {
    let schema = match storage.creation_schema(d.name.schema, d.name.name, txn.txid) {
        Ok(n) => n,
        Err(e) => return sql_fail(e),
    };
    if storage.domain_slot(schema.as_str(), d.name.name, txn.txid).is_some() {
        return sql_fail(sql_err!(
            sqlstate::DUPLICATE_OBJECT,
            "type \"{}\" already exists",
            d.name.name
        ));
    }
    let spec = match build_domain_spec(
        d.name.name,
        d.base_type,
        d.base_type_mod,
        d.not_null,
        d.default_text,
        d.checks,
        arena,
    ) {
        Ok(s) => s,
        Err(e) => return sql_fail(e),
    };
    let sqlname = match SqlName::parse(d.name.name) {
        Ok(n) => n,
        Err(e) => return sql_fail(e),
    };
    let slot = match storage.create_domain(schema, sqlname, spec, txn.txid) {
        Ok(slot) => slot,
        Err(e) => return sql_fail(e),
    };
    let lsn = storage.bump_lsn();
    if let Err(e) = wal.append(lsn, &WalOp::CreateDomain(*storage.domain(slot))) {
        storage.rollback_domain_create(slot);
        return sql_fail(e);
    }
    if let Err(e) = txn.record_ddl(super::txn::DdlUndo::DomainCreated(slot as u32)) {
        return sql_fail(e);
    }
    responder.command_complete("CREATE DOMAIN")?;
    sql_ok()
}

pub fn drop_domain(
    storage: &mut Storage,
    wal: &mut Wal,
    txn: &mut TxnState,
    names: &[QualName],
    if_exists: bool,
    cascade: bool,
    responder: &mut Responder,
) -> Outcome {
    for name in names {
        let Some(slot) = storage.resolve_domain_slot(name.name, txn.txid) else {
            if if_exists {
                responder.notice(
                    sqlstate::SUCCESSFUL_COMPLETION,
                    stack_format!(128, "type \"{}\" does not exist, skipping", name.name).as_str(),
                )?;
                continue;
            }
            return sql_fail(sql_err!(
                sqlstate::UNDEFINED_OBJECT,
                "type \"{}\" does not exist",
                name.name
            ));
        };
        let (schema, dname) = {
            let d = storage.domain(slot);
            (d.schema, d.name)
        };
        // RESTRICT (the default) fails if any column depends on the domain.
        if !cascade && storage.domain_in_use(schema.as_str(), dname.as_str()).is_some() {
            return sql_fail(sql_err!(
                sqlstate::DEPENDENT_OBJECTS_STILL_EXIST,
                "cannot drop type {} because other objects depend on it",
                dname.as_str()
            ));
        }
        let lsn = storage.bump_lsn();
        if let Err(e) =
            wal.append(lsn, &WalOp::DropDomain { schema: schema.as_str(), name: dname.as_str() })
        {
            return sql_fail(e);
        }
        match storage.drop_domain(schema.as_str(), dname.as_str(), txn.txid) {
            Ok(Some(slot)) => {
                if let Err(e) = txn.record_ddl(super::txn::DdlUndo::DomainDropped(slot as u32)) {
                    return sql_fail(e);
                }
            }
            Ok(None) => {}
            Err(e) => return sql_fail(e),
        }
    }
    responder.command_complete("DROP DOMAIN")?;
    sql_ok()
}

pub fn alter_domain(
    storage: &mut Storage,
    wal: &mut Wal,
    txn: &mut TxnState,
    name: &QualName,
    action: &crate::sql::ast::AlterDomainAction,
    arena: &Arena,
    responder: &mut Responder,
) -> Outcome {
    use crate::sql::ast::AlterDomainAction as A;
    let Some(slot) = storage.resolve_domain_slot(name.name, txn.txid) else {
        return sql_fail(sql_err!(
            sqlstate::UNDEFINED_OBJECT,
            "type \"{}\" does not exist",
            name.name
        ));
    };
    // Start from the current definition and apply the action.
    let current = *storage.domain(slot);
    let mut spec = crate::storage::DomainSpec {
        base: current.base,
        base_type_mod: current.base_type_mod,
        not_null: current.not_null,
        default_expr: current.default_expr,
        checks: current.checks,
        n_checks: current.n_checks,
    };
    match action {
        // NOTE: ALTER DOMAIN SET NOT NULL / ADD CHECK do not re-validate
        // existing rows here (PostgreSQL does). The constraint applies to
        // subsequent writes; see BUGS.md.
        A::SetNotNull => spec.not_null = true,
        A::DropNotNull => spec.not_null = false,
        A::SetDefault(text) => {
            if let Err(e) = validate_domain_expr(text, false, arena) {
                return sql_fail(e);
            }
            match domain_text::<{ crate::storage::DEFAULT_EXPR_MAX }>(text) {
                Ok(t) => spec.default_expr = Some(t),
                Err(e) => return sql_fail(e),
            }
        }
        A::DropDefault => spec.default_expr = None,
        A::AddCheck(check) => {
            if spec.n_checks == crate::storage::MAX_DOMAIN_CHECKS {
                return sql_fail(sql_err!(
                    sqlstate::PROGRAM_LIMIT_EXCEEDED,
                    "a domain may have at most {} CHECK constraints",
                    crate::storage::MAX_DOMAIN_CHECKS
                ));
            }
            if let Err(e) = validate_domain_expr(check.expression, true, arena) {
                return sql_fail(e);
            }
            let cname = match check.name {
                Some(nm) => match SqlName::parse(nm) {
                    Ok(n) => n,
                    Err(e) => return sql_fail(e),
                },
                None => match generate_check_name(current.name.as_str(), &spec.checks[..spec.n_checks]) {
                    Ok(n) => n,
                    Err(e) => return sql_fail(e),
                },
            };
            let expression = match domain_text::<{ crate::storage::CHECK_SQL_MAX }>(check.expression) {
                Ok(t) => t,
                Err(e) => return sql_fail(e),
            };
            spec.checks[spec.n_checks] = crate::storage::CheckConstraint { name: cname, expression };
            spec.n_checks += 1;
        }
        A::DropConstraint { name: cname, if_exists } => {
            let Some(pos) = spec.checks[..spec.n_checks].iter().position(|c| c.name.as_str() == *cname)
            else {
                if *if_exists {
                    responder.notice(
                        sqlstate::SUCCESSFUL_COMPLETION,
                        stack_format!(128, "constraint \"{}\" of domain \"{}\" does not exist, skipping", cname, name.name).as_str(),
                    )?;
                    responder.command_complete("ALTER DOMAIN")?;
                    return sql_ok();
                }
                return sql_fail(sql_err!(
                    sqlstate::UNDEFINED_OBJECT,
                    "constraint \"{}\" of domain \"{}\" does not exist",
                    cname,
                    name.name
                ));
            };
            for i in pos..spec.n_checks - 1 {
                spec.checks[i] = spec.checks[i + 1];
            }
            spec.n_checks -= 1;
            spec.checks[spec.n_checks] = crate::storage::CheckConstraint::EMPTY;
        }
    }
    storage.alter_domain(slot, spec);
    let lsn = storage.bump_lsn();
    if let Err(e) = wal.append(lsn, &WalOp::CreateDomain(*storage.domain(slot))) {
        // Restore the pre-ALTER definition on a journal failure.
        let restore = crate::storage::DomainSpec {
            base: current.base,
            base_type_mod: current.base_type_mod,
            not_null: current.not_null,
            default_expr: current.default_expr,
            checks: current.checks,
            n_checks: current.n_checks,
        };
        storage.alter_domain(slot, restore);
        return sql_fail(e);
    }
    responder.command_complete("ALTER DOMAIN")?;
    sql_ok()
}

/// Builds an [`EnumSpec`] from a `CREATE TYPE ... AS ENUM` label list: rejects
/// duplicates (42710) and over-long labels, and assigns each member a 1-based
/// sort key (PostgreSQL's `enumsortorder`).
fn build_enum_spec(labels: &[&str]) -> Result<crate::storage::EnumSpec, SqlError> {
    if labels.len() > crate::storage::MAX_ENUM_LABELS {
        return Err(sql_err!(
            sqlstate::PROGRAM_LIMIT_EXCEEDED,
            "an enum type may have at most {} labels",
            crate::storage::MAX_ENUM_LABELS
        ));
    }
    let mut members = [crate::storage::EnumMember::EMPTY; crate::storage::MAX_ENUM_LABELS];
    for (i, &label) in labels.iter().enumerate() {
        if labels[..i].contains(&label) {
            return Err(sql_err!(
                sqlstate::DUPLICATE_OBJECT,
                "enum label \"{}\" specified more than once",
                label
            ));
        }
        members[i] = crate::storage::EnumMember { label: SqlName::parse(label)?, sort: (i + 1) as f64 };
    }
    Ok(crate::storage::EnumSpec { members, n_members: labels.len() })
}

pub fn create_enum(
    storage: &mut Storage,
    wal: &mut Wal,
    txn: &mut TxnState,
    name: &QualName,
    labels: &[&str],
    responder: &mut Responder,
) -> Outcome {
    let schema = match storage.creation_schema(name.schema, name.name, txn.txid) {
        Ok(n) => n,
        Err(e) => return sql_fail(e),
    };
    // Types share a namespace: reject a name already taken by a domain or enum.
    if storage.enum_slot(schema.as_str(), name.name, txn.txid).is_some()
        || storage.domain_slot(schema.as_str(), name.name, txn.txid).is_some()
    {
        return sql_fail(sql_err!(
            sqlstate::DUPLICATE_OBJECT,
            "type \"{}\" already exists",
            name.name
        ));
    }
    let spec = match build_enum_spec(labels) {
        Ok(s) => s,
        Err(e) => return sql_fail(e),
    };
    let sqlname = match SqlName::parse(name.name) {
        Ok(n) => n,
        Err(e) => return sql_fail(e),
    };
    let slot = match storage.create_enum(schema, sqlname, spec, txn.txid) {
        Ok(slot) => slot,
        Err(e) => return sql_fail(e),
    };
    let lsn = storage.bump_lsn();
    if let Err(e) = wal.append(lsn, &WalOp::CreateEnum(*storage.enum_def(slot))) {
        storage.rollback_enum_create(slot);
        return sql_fail(e);
    }
    if let Err(e) = txn.record_ddl(super::txn::DdlUndo::EnumCreated(slot as u32)) {
        return sql_fail(e);
    }
    responder.command_complete("CREATE TYPE")?;
    sql_ok()
}

pub fn drop_enum(
    storage: &mut Storage,
    wal: &mut Wal,
    txn: &mut TxnState,
    names: &[QualName],
    if_exists: bool,
    cascade: bool,
    responder: &mut Responder,
) -> Outcome {
    for name in names {
        let Some(slot) = storage.resolve_enum_slot(name.name, txn.txid) else {
            if if_exists {
                responder.notice(
                    sqlstate::SUCCESSFUL_COMPLETION,
                    stack_format!(128, "type \"{}\" does not exist, skipping", name.name).as_str(),
                )?;
                continue;
            }
            return sql_fail(sql_err!(
                sqlstate::UNDEFINED_OBJECT,
                "type \"{}\" does not exist",
                name.name
            ));
        };
        let (schema, ename) = {
            let e = storage.enum_def(slot);
            (e.schema, e.name)
        };
        // RESTRICT (the default) fails if any column is of this enum type.
        if !cascade && storage.enum_in_use(slot).is_some() {
            return sql_fail(sql_err!(
                sqlstate::DEPENDENT_OBJECTS_STILL_EXIST,
                "cannot drop type {} because other objects depend on it",
                ename.as_str()
            ));
        }
        let lsn = storage.bump_lsn();
        if let Err(e) =
            wal.append(lsn, &WalOp::DropEnum { schema: schema.as_str(), name: ename.as_str() })
        {
            return sql_fail(e);
        }
        match storage.drop_enum(schema.as_str(), ename.as_str(), txn.txid) {
            Ok(Some(slot)) => {
                if let Err(e) = txn.record_ddl(super::txn::DdlUndo::EnumDropped(slot as u32)) {
                    return sql_fail(e);
                }
            }
            Ok(None) => {}
            Err(e) => return sql_fail(e),
        }
    }
    responder.command_complete("DROP TYPE")?;
    sql_ok()
}

pub fn alter_type(
    storage: &mut Storage,
    wal: &mut Wal,
    txn: &mut TxnState,
    name: &QualName,
    action: &crate::sql::ast::AlterTypeAction,
    responder: &mut Responder,
) -> Outcome {
    use crate::sql::ast::AlterTypeAction as A;
    let Some(slot) = storage.resolve_enum_slot(name.name, txn.txid) else {
        return sql_fail(sql_err!(
            sqlstate::UNDEFINED_OBJECT,
            "type \"{}\" does not exist",
            name.name
        ));
    };
    match action {
        A::AddValue { label, if_not_exists, before, after } => {
            let current = *storage.enum_def(slot);
            if current.sort_of(label).is_some() {
                if *if_not_exists {
                    responder.notice(
                        sqlstate::DUPLICATE_OBJECT,
                        stack_format!(128, "enum label \"{}\" already exists, skipping", label).as_str(),
                    )?;
                    responder.command_complete("ALTER TYPE")?;
                    return sql_ok();
                }
                return sql_fail(sql_err!(
                    sqlstate::DUPLICATE_OBJECT,
                    "enum label \"{}\" already exists",
                    label
                ));
            }
            if current.n_members >= crate::storage::MAX_ENUM_LABELS {
                return sql_fail(sql_err!(
                    sqlstate::PROGRAM_LIMIT_EXCEEDED,
                    "an enum type may have at most {} labels",
                    crate::storage::MAX_ENUM_LABELS
                ));
            }
            let sort = match compute_add_value_sort(&current, before.as_deref(), after.as_deref()) {
                Ok(s) => s,
                Err(e) => return sql_fail(e),
            };
            let new_label = match SqlName::parse(label) {
                Ok(n) => n,
                Err(e) => return sql_fail(e),
            };
            let mut spec = crate::storage::EnumSpec {
                members: current.members,
                n_members: current.n_members,
            };
            spec.members[spec.n_members] = crate::storage::EnumMember { label: new_label, sort };
            spec.n_members += 1;
            storage.alter_enum(slot, spec);
            let lsn = storage.bump_lsn();
            if let Err(e) = wal.append(lsn, &WalOp::CreateEnum(*storage.enum_def(slot))) {
                storage.alter_enum(slot, crate::storage::EnumSpec {
                    members: current.members,
                    n_members: current.n_members,
                });
                return sql_fail(e);
            }
        }
        A::RenameTo(_) => {
            return sql_fail(sql_err!(
                sqlstate::FEATURE_NOT_SUPPORTED,
                "ALTER TYPE ... RENAME TO is not supported: enum-typed columns store the \
                 type name, so a rename would require rewriting every dependent column"
            ))
        }
        A::RenameValue { .. } => {
            return sql_fail(sql_err!(
                sqlstate::FEATURE_NOT_SUPPORTED,
                "ALTER TYPE ... RENAME VALUE is not supported: enum values are stored inline, \
                 so a rename would require rewriting every stored row"
            ))
        }
    }
    responder.command_complete("ALTER TYPE")?;
    sql_ok()
}

/// The sort key for a new enum member: appended past the current maximum, or —
/// with BEFORE/AFTER — midway between the named neighbour and its adjacent
/// member (fractional, so existing members and stored rows are undisturbed).
fn compute_add_value_sort(
    def: &crate::storage::EnumDef,
    before: Option<&str>,
    after: Option<&str>,
) -> Result<f64, SqlError> {
    let members = def.members();
    let neighbour = before.or(after);
    let Some(pivot) = neighbour else {
        // Append: one past the current maximum sort (or 1.0 for an empty enum).
        let max = members.iter().map(|m| m.sort).fold(f64::NEG_INFINITY, f64::max);
        return Ok(if members.is_empty() { 1.0 } else { max + 1.0 });
    };
    let Some(pivot_sort) = def.sort_of(pivot) else {
        return Err(sql_err!(
            sqlstate::INVALID_TEXT_REPRESENTATION,
            "\"{}\" is not an existing enum label",
            pivot
        ));
    };
    // Sorted neighbours around the pivot bound the fractional insertion.
    let mut sorts: [f64; crate::storage::MAX_ENUM_LABELS] = [0.0; crate::storage::MAX_ENUM_LABELS];
    for (i, m) in members.iter().enumerate() {
        sorts[i] = m.sort;
    }
    sorts[..members.len()].sort_by(|a, b| a.partial_cmp(b).unwrap());
    let pos = sorts[..members.len()].iter().position(|&s| s == pivot_sort).unwrap();
    let new_sort = if before.is_some() {
        let lower = if pos == 0 { pivot_sort - 1.0 } else { sorts[pos - 1] };
        (lower + pivot_sort) / 2.0
    } else {
        let upper = if pos + 1 == members.len() { pivot_sort + 1.0 } else { sorts[pos + 1] };
        (pivot_sort + upper) / 2.0
    };
    Ok(new_sort)
}

/// Resolves a name to a live sequence slot. A relation that exists but is not a
/// sequence is a type error (42809); a missing relation is `Ok(None)` so the
/// caller can apply IF EXISTS.
fn resolve_sequence(
    storage: &Storage,
    name: &QualName,
    txid: u32,
) -> Result<Option<usize>, SqlError> {
    // A qualifier names the schema directly; otherwise search the path.
    if let Some(slot) = storage.sequence_on_path(name.schema, name.name, txid) {
        return Ok(Some(slot));
    }
    // Not a sequence: distinguish a wrong-type relation (42809) from absence.
    if storage.resolve_relation(name.schema, name.name, txid).is_some() {
        return Err(sql_err!(
            sqlstate::WRONG_OBJECT_TYPE,
            "\"{}\" is not a sequence",
            name.name
        ));
    }
    Ok(None)
}

/// DROP VIEW [IF EXISTS].
pub fn drop_view(
    storage: &mut Storage,
    wal: &mut Wal,
    txn: &mut super::txn::TxnState,
    names: &[QualName],
    if_exists: bool,
    responder: &mut Responder,
) -> Outcome {
    for name in names {
        if let Some(schema) = name.schema
            && storage.find_schema_visible(schema, txn.txid).is_none()
            && !if_exists
        {
            return sql_fail(sql_err!(
                sqlstate::INVALID_SCHEMA_NAME,
                "schema \"{}\" does not exist",
                schema
            ));
        }
        let found = match storage.resolve_relation(name.schema, name.name, txn.txid) {
            Some(crate::storage::ResolvedRelation::View(slot)) => Some(slot),
            _ => None,
        };
        if let Some(slot) = found {
            let (schema, view_name) = {
                let v = storage.view(slot);
                (v.schema, v.name)
            };
            let lsn = storage.bump_lsn();
            if let Err(e) = wal.append(
                lsn,
                &WalOp::DropView { schema: schema.as_str(), name: view_name.as_str() },
            ) {
                return sql_fail(e);
            }
            let dropped = match storage.drop_view(schema.as_str(), view_name.as_str(), txn.txid)
            {
                Ok(d) => d,
                Err(e) => return sql_fail(e),
            };
            if let Some(slot) = dropped
                && let Err(e) = txn.record_ddl(super::txn::DdlUndo::ViewDropped(slot as u32))
            {
                return sql_fail(e);
            }
        } else if if_exists {
            responder.notice(
                crate::sql::eval::sqlstate::SUCCESSFUL_COMPLETION,
                stack_format!(128, "view \"{}\" does not exist, skipping", name.name).as_str(),
            )?;
        } else {
            return sql_fail(sql_err!(
                sqlstate::UNDEFINED_TABLE,
                "view \"{}\" does not exist",
                name.name
            ));
        }
    }
    responder.command_complete("DROP VIEW")?;
    sql_ok()
}

/// CREATE [UNIQUE] INDEX: registers a durable index over a table's columns.
/// The engine does full scans, so the index never accelerates a query; a
/// UNIQUE index enforces a uniqueness constraint on its column tuple (checked
/// here against existing rows, and on every later INSERT/UPDATE).
#[allow(clippy::too_many_arguments)]
pub fn create_index(
    storage: &mut Storage,
    wal: &mut Wal,
    txn: &mut super::txn::TxnState,
    name: &str,
    table: &QualName,
    column_names: &[&str],
    unique: bool,
    responder: &mut Responder,
) -> Outcome {
    use crate::storage::{IndexDef, MAX_INDEX_COLS};
    let table_index = match resolve_dml_table(storage, table, txn.txid) {
        Ok(i) => i,
        Err(e) => return sql_fail(e),
    };
    let tdef = storage.table(table_index).def;
    if column_names.is_empty() || column_names.len() > MAX_INDEX_COLS {
        return sql_fail(sql_err!(
            sqlstate::PROGRAM_LIMIT_EXCEEDED,
            "an index must have 1..={} columns",
            MAX_INDEX_COLS
        ));
    }
    let mut columns = [0u16; MAX_INDEX_COLS];
    for (i, column_name) in column_names.iter().enumerate() {
        let Some(column_index) = tdef.column_index(column_name) else {
            return sql_fail(sql_err!(
                sqlstate::UNDEFINED_COLUMN,
                "column \"{}\" does not exist",
                column_name
            ));
        };
        columns[i] = column_index as u16;
    }
    // The written column list's length — not the fixed array's, whose
    // padding would quietly widen the index's tuple (a UNIQUE index on (b)
    // must not enforce uniqueness of (b, first-column) instead).
    let n_cols = column_names.len();
    let sqlname = match SqlName::parse(name) {
        Ok(n) => n,
        Err(e) => return sql_fail(e),
    };
    let def = IndexDef {
        schema: tdef.schema,
        name: sqlname,
        table: tdef.name,
        columns,
        n_cols,
        unique,
        live: true,
        pending: None,
    };
    // Register first so the UNIQUE validation below sees this index; on any
    // failure the registration is rolled back.
    let slot = match storage.create_index(def, txn.txid) {
        Ok(s) => s,
        Err(e) => return sql_fail(e),
    };
    if unique {
        let mut schema = [ColType::Bool; MAX_COLUMNS];
        tdef.schema(&mut schema);
        // Every existing row is checked against the others via the just-
        // registered index (all borrows shared); a conflict is deferred so the
        // rollback drop_index (a mutable borrow) runs after the scan.
        let mut conflict: Option<SqlError> = None;
        let _ = storage.for_each_row_state(table_index, &mut |rowid, state| {
            use core::ops::ControlFlow;
            let Some(home) = state.committed else {
                return Ok(ControlFlow::Continue(()));
            };
            // The whole check runs inside the fetch: the decoded values borrow
            // the fetched bytes, and the re-scan's own fetches nest into the
            // spill reader's second scratch.
            if let Err(e) = storage.with_row_bytes(table_index, rowid, home, |bytes| {
                let mut values = [Datum::Null; MAX_COLUMNS];
                rowenc::decode(bytes, &schema[..tdef.n_columns], &mut values)?;
                check_unique_indexes(
                    storage,
                    table_index,
                    &tdef,
                    &schema[..tdef.n_columns],
                    &values[..tdef.n_columns],
                    Some(rowid),
                    // The just-registered index is an uncommitted CREATE owned
                    // by this transaction; validation must see it.
                    txn.txid,
                )
            }) {
                conflict = Some(e);
                return Ok(ControlFlow::Break(()));
            }
            Ok(ControlFlow::Continue(()))
        });
        if let Some(e) = conflict {
            storage.rollback_index_create(slot);
            return sql_fail(e);
        }
    }
    let lsn = storage.bump_lsn();
    if let Err(e) = wal.append(
        lsn,
        &WalOp::CreateIndex {
            schema: tdef.schema.as_str(),
            name,
            table: tdef.name.as_str(),
            columns,
            n_cols,
            unique,
        },
    ) {
        storage.rollback_index_create(slot);
        return sql_fail(e);
    }
    if let Err(e) = txn.record_ddl(super::txn::DdlUndo::IndexCreated(slot as u32)) {
        return sql_fail(e);
    }
    responder.command_complete("CREATE INDEX")?;
    sql_ok()
}

/// DROP INDEX [IF EXISTS].
pub fn drop_index(
    storage: &mut Storage,
    wal: &mut Wal,
    txn: &mut super::txn::TxnState,
    names: &[QualName],
    if_exists: bool,
    responder: &mut Responder,
) -> Outcome {
    for name in names {
        if let Some(schema) = name.schema
            && storage.find_schema_visible(schema, txn.txid).is_none()
            && !if_exists
        {
            return sql_fail(sql_err!(
                sqlstate::INVALID_SCHEMA_NAME,
                "schema \"{}\" does not exist",
                schema
            ));
        }
        // A bare index name resolves through the search path; a qualified one
        // looks only in its schema.
        let found: Option<SqlName> = match name.schema {
            Some(schema) => storage
                .index_exists(schema, name.name, txn.txid)
                .then(|| SqlName::parse(schema).ok())
                .flatten(),
            None => storage.path().entries().iter().find_map(|e| match e {
                crate::storage::PathEntry::Schema(slot) => {
                    let schema = storage.schema_def(*slot as usize).name;
                    storage
                        .index_exists(schema.as_str(), name.name, txn.txid)
                        .then_some(schema)
                }
                crate::storage::PathEntry::Catalog => None,
            }),
        };
        if let Some(schema) = found {
            let lsn = storage.bump_lsn();
            if let Err(e) = wal.append(
                lsn,
                &WalOp::DropIndex { schema: schema.as_str(), name: name.name },
            ) {
                return sql_fail(e);
            }
            let dropped = match storage.drop_index(schema.as_str(), name.name, txn.txid) {
                Ok(d) => d,
                Err(e) => return sql_fail(e),
            };
            if let Some(slot) = dropped
                && let Err(e) = txn.record_ddl(super::txn::DdlUndo::IndexDropped(slot as u32))
            {
                return sql_fail(e);
            }
        } else if if_exists {
            responder.notice(
                crate::sql::eval::sqlstate::SUCCESSFUL_COMPLETION,
                stack_format!(128, "index \"{}\" does not exist, skipping", name.name).as_str(),
            )?;
        } else {
            // An index is an object, not a relation, to PostgreSQL's error
            // codes: a missing one is 42704, where a missing table is 42P01.
            return sql_fail(sql_err!(
                sqlstate::UNDEFINED_OBJECT,
                "index \"{}\" does not exist",
                name.name
            ));
        }
    }
    responder.command_complete("DROP INDEX")?;
    sql_ok()
}

/// COPY's per-statement setup: the resolved table and target columns, in
/// COPY's column order. Held by the connection across CopyData messages.
#[derive(Clone, Copy)]
pub struct CopySetup {
    pub table_index: usize,
    pub targets: [usize; MAX_COLUMNS],
    pub n_targets: usize,
    pub fmt: CopyFmt,
}

/// The resolved, owned form of a COPY's format options — owned because a COPY
/// FROM STDIN's [`CopySetup`] outlives the statement arena. The `force_*` fields
/// are bitmasks over table column indices.
#[derive(Clone, Copy)]
pub struct CopyFmt {
    pub csv: bool,
    pub binary: bool,
    pub delimiter: u8,
    pub quote: u8,
    pub escape: u8,
    pub header: bool,
    pub null: StackStr<64>,
    pub force_quote_all: bool,
    pub force_quote: u64,
    pub force_not_null: u64,
    pub force_null: u64,
}

impl CopyFmt {
    fn resolve(
        def: &TableDef,
        table_name: &str,
        options: &crate::sql::ast::CopyOptions,
    ) -> Result<CopyFmt, SqlError> {
        use core::fmt::Write as _;
        let mut null = StackStr::<64>::new();
        let _ = null.write_str(options.null_str());
        if null.is_truncated() {
            return Err(sql_err!(sqlstate::FEATURE_NOT_SUPPORTED, "COPY NULL string is too long"));
        }
        // Resolve a FORCE column list into a bitmask over table columns.
        let mask = |names: &[&str]| -> Result<u64, SqlError> {
            let mut bits = 0u64;
            for name in names {
                let Some(index) = def.column_index(name) else {
                    return Err(sql_err!(
                        sqlstate::UNDEFINED_COLUMN,
                        "column \"{}\" of relation \"{}\" does not exist",
                        name,
                        table_name
                    ));
                };
                bits |= 1u64 << index;
            }
            Ok(bits)
        };
        let delimiter = options.delimiter_byte();
        let quote = options.quote_byte();
        if options.is_csv() && delimiter == quote {
            return Err(sql_err!(
                sqlstate::FEATURE_NOT_SUPPORTED,
                "COPY delimiter and quote must be different"
            ));
        }
        Ok(CopyFmt {
            csv: options.is_csv(),
            binary: options.is_binary(),
            delimiter,
            quote,
            escape: options.escape_byte(),
            header: options.header,
            null,
            force_quote_all: options.force_quote_all,
            force_quote: mask(options.force_quote)?,
            force_not_null: mask(options.force_not_null)?,
            force_null: mask(options.force_null)?,
        })
    }

    fn forced(mask: u64, column: usize) -> bool {
        column < 64 && mask & (1u64 << column) != 0
    }
}

/// Resolves a COPY statement's table, column list, and format options.
pub fn copy_begin(
    storage: &Storage,
    statement: &crate::sql::ast::CopyStmt,
    txid: u32,
) -> Result<CopySetup, SqlError> {
    let table_index = resolve_dml_table(storage, &statement.table, txid)?;
    let def = &storage.table(table_index).def;
    let mut targets = [0usize; MAX_COLUMNS];
    let n_targets = if statement.columns.is_empty() {
        for (i, t) in targets.iter_mut().enumerate().take(def.n_columns) {
            *t = i;
        }
        def.n_columns
    } else {
        for (i, name) in statement.columns.iter().enumerate() {
            let Some(col) = def.column_index(name) else {
                return Err(sql_err!(
                    sqlstate::UNDEFINED_COLUMN,
                    "column \"{}\" of relation \"{}\" does not exist",
                    name,
                    statement.table.name
                ));
            };
            targets[i] = col;
        }
        statement.columns.len()
    };
    // Direction-only options are refused as PostgreSQL does.
    let opts = &statement.options;
    if !statement.to && (opts.force_quote_all || !opts.force_quote.is_empty()) {
        return Err(sql_err!(
            sqlstate::FEATURE_NOT_SUPPORTED,
            "COPY FORCE_QUOTE cannot be used with COPY FROM"
        ));
    }
    if statement.to && !opts.force_not_null.is_empty() {
        return Err(sql_err!(
            sqlstate::INVALID_PARAMETER_VALUE,
            "COPY FORCE_NOT_NULL cannot be used with COPY TO"
        ));
    }
    if statement.to && !opts.force_null.is_empty() {
        return Err(sql_err!(
            sqlstate::INVALID_PARAMETER_VALUE,
            "COPY FORCE_NULL cannot be used with COPY TO"
        ));
    }
    let fmt = CopyFmt::resolve(def, statement.table.name, &statement.options)?;
    // Binary format speaks each type's real binary wire form. The types whose
    // binary format has no stored column representation — only anonymous
    // `record` — are refused loudly rather than emit something a binary
    // consumer would misparse. Every other column type, composites included,
    // has a byte-exact binary send/recv codec.
    if fmt.binary {
        for &target in &targets[..n_targets] {
            let ctype = def.columns()[target].ctype;
            if !binary_copy_supported(ctype) {
                return Err(sql_err!(
                    sqlstate::FEATURE_NOT_SUPPORTED,
                    "COPY BINARY of type {} is not supported yet",
                    ctype.name()
                ));
            }
        }
    }
    Ok(CopySetup { table_index, targets, n_targets, fmt })
}

/// Whether COPY BINARY can round-trip a column of this type (its binary wire
/// format is emitted and decoded faithfully). The text-shortcut types are not
/// yet covered.
fn binary_copy_supported(ctype: ColType) -> bool {
    // Arrays, ranges, multiranges and bit strings all have binary send/recv
    // codecs. Record is anonymous-composite only (never a stored column type)
    // and has no stable binary column representation, so it stays unsupported.
    !matches!(ctype, ColType::Record)
}

/// One COPY FROM data line: text fields decode, coerce through each column's
/// input semantics, and store through the same row core INSERT uses —
/// defaults, sequences, NOT NULL, uniqueness, CHECK and foreign keys all
/// enforced identically.
pub fn copy_row(
    storage: &mut Storage,
    txn: &mut TxnState,
    setup: &CopySetup,
    line: &[u8],
    arena: &Arena,
) -> Result<(), SqlError> {
    let def = storage.table(setup.table_index).def;
    let mut fields: [Option<&str>; MAX_COLUMNS] = [None; MAX_COLUMNS];
    let fmt = &setup.fmt;
    let n_fields = if fmt.csv {
        crate::sql::copy::decode_row_csv(
            line,
            arena,
            &mut fields[..setup.n_targets],
            fmt.delimiter,
            fmt.quote,
            fmt.escape,
            fmt.null.as_str(),
            &|i| CopyFmt::forced(fmt.force_not_null, setup.targets[i]),
            &|i| CopyFmt::forced(fmt.force_null, setup.targets[i]),
        )?
    } else {
        crate::sql::copy::decode_row(line, arena, &mut fields[..setup.n_targets])?
    };
    if n_fields < setup.n_targets {
        return Err(sql_err!(
            sqlstate::BAD_COPY_FILE_FORMAT,
            "missing data for column \"{}\"",
            def.columns()[setup.targets[n_fields]].name.as_str()
        ));
    }
    let checks = parse_checks(&def, arena)?;
    let mut values = [Datum::Null; MAX_COLUMNS];
    for (i, col) in def.columns().iter().enumerate() {
        if let Some(d) = &col.default_value {
            values[i] = d.as_datum();
        }
    }
    let mut explicit = [false; MAX_COLUMNS];
    for (i, field) in fields.iter().enumerate().take(setup.n_targets) {
        let col_index = setup.targets[i];
        let col = &def.columns()[col_index];
        values[col_index] = match field {
            None => Datum::Null,
            Some(text) => coerce(Datum::Text(text), col, storage, arena)?,
        };
        explicit[col_index] = true;
    }
    fill_auto_increment(storage, setup.table_index, &def, &mut values, &explicit)?;
    check_not_null(&def, &values)?;
    let mut schema = [ColType::Bool; MAX_COLUMNS];
    def.schema(&mut schema);
    enforce_row_constraints(
        storage,
        setup.table_index,
        &def,
        &schema[..def.n_columns],
        &values[..def.n_columns],
        None,
        txn.txid,
        &checks,
        arena,
        &[],
    )?;
    store_row(storage, txn, setup.table_index, None, &values[..def.n_columns])
}

/// One COPY FROM binary row: `row` is the int16 field count followed by each
/// field's int32 length (or -1 for NULL) and its binary bytes. Fields decode
/// through each column's binary input, then store through the same row core as
/// INSERT — defaults, sequences, NOT NULL, uniqueness, CHECK and foreign keys.
pub fn copy_row_binary(
    storage: &mut Storage,
    txn: &mut TxnState,
    setup: &CopySetup,
    row: &[u8],
    arena: &Arena,
) -> Result<(), SqlError> {
    let def = storage.table(setup.table_index).def;
    let count = i16::from_be_bytes([row[0], row[1]]);
    if count < 0 || count as usize != setup.n_targets {
        return Err(sql_err!(
            sqlstate::BAD_COPY_FILE_FORMAT,
            "COPY binary row has {} fields, expected {}",
            count,
            setup.n_targets
        ));
    }
    let checks = parse_checks(&def, arena)?;
    let mut values = [Datum::Null; MAX_COLUMNS];
    for (i, col) in def.columns().iter().enumerate() {
        if let Some(d) = &col.default_value {
            values[i] = d.as_datum();
        }
    }
    let mut explicit = [false; MAX_COLUMNS];
    let mut at = 2usize;
    for i in 0..setup.n_targets {
        let flen = i32::from_be_bytes([row[at], row[at + 1], row[at + 2], row[at + 3]]);
        at += 4;
        let col_index = setup.targets[i];
        let col = def.columns()[col_index];
        values[col_index] = if flen == -1 {
            Datum::Null
        } else {
            let field = &row[at..at + flen as usize];
            at += flen as usize;
            let decoded = decode_binary_field(col.ctype, field, arena)?;
            coerce(decoded, &col, storage, arena)?
        };
        explicit[col_index] = true;
    }
    fill_auto_increment(storage, setup.table_index, &def, &mut values, &explicit)?;
    check_not_null(&def, &values)?;
    let mut schema = [ColType::Bool; MAX_COLUMNS];
    def.schema(&mut schema);
    enforce_row_constraints(
        storage,
        setup.table_index,
        &def,
        &schema[..def.n_columns],
        &values[..def.n_columns],
        None,
        txn.txid,
        &checks,
        arena,
        &[],
    )?;
    store_row(storage, txn, setup.table_index, None, &values[..def.n_columns])
}

/// Decodes one COPY-binary field into a datum of `ctype`, per PostgreSQL's
/// per-type binary receive format. Reuses the extended-protocol binary decoder
/// for the shared scalar types; handles the ones it does not (`smallint` as a
/// true int2, `timetz`, the text family, and the composite array / range /
/// multirange / bit formats) directly. Only anonymous `record` is refused at
/// [`copy_begin`], so it never reaches here.
pub(crate) fn decode_binary_field<'a>(
    ctype: ColType,
    bytes: &'a [u8],
    arena: &'a Arena,
) -> Result<Datum<'a>, SqlError> {
    use crate::sql::types::oid as oids;
    let bad = || sql_err!(sqlstate::BAD_COPY_FILE_FORMAT, "invalid binary field for type {}", ctype.name());
    let via = |oid| crate::pg::conn::decode_binary_param(oid, bytes, arena).map_err(|_| bad());
    match ctype {
        ColType::Bool => via(oids::BOOL),
        ColType::Int2 => {
            let b: [u8; 2] = bytes.try_into().map_err(|_| bad())?;
            Ok(Datum::Int2(i16::from_be_bytes(b)))
        }
        ColType::Int4 => via(oids::INT4),
        ColType::Int8 => via(oids::INT8),
        ColType::Float4 => via(oids::FLOAT4),
        ColType::Float8 => via(oids::FLOAT8),
        ColType::Text | ColType::Varchar | ColType::Bpchar | ColType::Name => {
            core::str::from_utf8(bytes).map(Datum::Text).map_err(|_| bad())
        }
        ColType::Date => via(oids::DATE),
        ColType::Timestamp => via(oids::TIMESTAMP),
        ColType::Timestamptz => via(oids::TIMESTAMPTZ),
        ColType::Time => via(oids::TIME),
        ColType::Timetz => {
            // 8-byte time then a 4-byte zone counted west of UTC; the stored
            // offset is the eastward negation, as the send side flips it.
            let b: [u8; 12] = bytes.try_into().map_err(|_| bad())?;
            let time = i64::from_be_bytes(b[0..8].try_into().unwrap());
            let zone = i32::from_be_bytes(b[8..12].try_into().unwrap());
            Ok(Datum::Timetz(time, -zone))
        }
        ColType::Interval => via(oids::INTERVAL),
        ColType::Json => via(oids::JSON),
        ColType::Jsonb => via(oids::JSONB),
        ColType::Uuid => via(oids::UUID),
        ColType::Bytea => via(oids::BYTEA),
        ColType::Numeric => via(oids::NUMERIC),
        ColType::Inet => via(oids::INET),
        ColType::Cidr => via(oids::CIDR),
        ColType::Macaddr => via(oids::MACADDR),
        ColType::Macaddr8 => via(oids::MACADDR8),
        ColType::Array(element) => decode_binary_array(element, bytes, arena),
        ColType::Range(kind) => decode_binary_range(kind, bytes, arena),
        ColType::Multirange(kind) => decode_binary_multirange(kind, bytes, arena),
        ColType::Bit { varying } => decode_binary_bit(varying, bytes, arena),
        ColType::Record => Err(sql_err!(
            sqlstate::FEATURE_NOT_SUPPORTED,
            "COPY BINARY of type {} is not supported yet",
            ctype.name()
        )),
        // An enum's binary field is its label, but resolving the label to a
        // member's sort key needs the catalog, which this codec does not carry.
        // COPY (text) of enum columns works; binary is a loud gap.
        ColType::Enum(_) => Err(sql_err!(
            sqlstate::FEATURE_NOT_SUPPORTED,
            "COPY BINARY of an enum type is not supported yet (use COPY text format)"
        )),
    }
}

/// Decodes the PostgreSQL binary array format: int32 ndim, int32 has-null,
/// int32 element OID, then (for ndim > 0) one dim descriptor {count, lbound}
/// and each element as int32 length (-1 = NULL) + its binary. Only the
/// one-dimensional form this engine stores is accepted.
fn decode_binary_array<'a>(
    element: crate::sql::types::ArrElem,
    bytes: &'a [u8],
    arena: &'a Arena,
) -> Result<Datum<'a>, SqlError> {
    let bad = || sql_err!(sqlstate::BAD_COPY_FILE_FORMAT, "invalid binary array");
    let mut reader = crate::pg::wire::MsgIn::new(bytes);
    let ndim = reader.i32().map_err(|_| bad())?;
    let _has_null = reader.i32().map_err(|_| bad())?;
    let _element_oid = reader.i32().map_err(|_| bad())?;
    if ndim == 0 {
        return Ok(Datum::Array {
            element,
            raw: crate::sql::array::build(&[], arena)?,
        });
    }
    if ndim != 1 {
        return Err(sql_err!(
            sqlstate::FEATURE_NOT_SUPPORTED,
            "COPY BINARY of multi-dimensional arrays is not supported"
        ));
    }
    let count = reader.i32().map_err(|_| bad())?;
    let _lower_bound = reader.i32().map_err(|_| bad())?;
    if !(0..=crate::sql::array::MAX_ELEMENTS as i32).contains(&count) {
        return Err(sql_err!(sqlstate::PROGRAM_LIMIT_EXCEEDED, "array value too large"));
    }
    let element_type = element.to_coltype();
    let mut items = [Datum::Null; crate::sql::array::MAX_ELEMENTS];
    for slot in items.iter_mut().take(count as usize) {
        let len = reader.i32().map_err(|_| bad())?;
        if len < 0 {
            *slot = Datum::Null;
            continue;
        }
        let field = reader.take(len as usize).map_err(|_| bad())?;
        *slot = decode_binary_field(element_type, field, arena)?;
    }
    Ok(Datum::Array {
        element,
        raw: crate::sql::array::build(&items[..count as usize], arena)?,
    })
}

/// Decodes one PostgreSQL binary range body (flags byte + finite bounds) into a
/// canonical `Datum::Range`. Shared by range and multirange decoding.
fn decode_binary_range<'a>(
    kind: crate::sql::types::RangeKind,
    bytes: &'a [u8],
    arena: &'a Arena,
) -> Result<Datum<'a>, SqlError> {
    let mut reader = crate::pg::wire::MsgIn::new(bytes);
    let text = decode_range_body(kind, &mut reader, arena)?;
    Ok(Datum::Range { text, kind })
}

fn decode_range_body<'a>(
    kind: crate::sql::types::RangeKind,
    reader: &mut crate::pg::wire::MsgIn<'a>,
    arena: &'a Arena,
) -> Result<&'a str, SqlError> {
    let bad = || sql_err!(sqlstate::BAD_COPY_FILE_FORMAT, "invalid binary range");
    let flags = reader.u8().map_err(|_| bad())?;
    let mut parsed = crate::sql::range::Parsed {
        empty: flags & 0x01 != 0,
        lower: None,
        upper: None,
        lower_inc: flags & 0x02 != 0,
        upper_inc: flags & 0x04 != 0,
    };
    if parsed.empty {
        return crate::sql::range::canonical(&parsed, kind, arena);
    }
    let element_type = kind.elem_type();
    let read_bound = |reader: &mut crate::pg::wire::MsgIn<'a>| -> Result<Option<&'a str>, SqlError> {
        let len = reader.i32().map_err(|_| bad())?;
        if len < 0 {
            return Ok(None);
        }
        let field = reader.take(len as usize).map_err(|_| bad())?;
        let datum = decode_binary_field(element_type, field, arena)?;
        Ok(Some(arena.alloc_str_display(datum).map_err(|_| {
            sql_err!(sqlstate::PROGRAM_LIMIT_EXCEEDED, "range bound exceeds the statement arena")
        })?))
    };
    if flags & 0x08 == 0 {
        parsed.lower = read_bound(reader)?;
    }
    if flags & 0x10 == 0 {
        parsed.upper = read_bound(reader)?;
    }
    crate::sql::range::canonical(&parsed, kind, arena)
}

/// Decodes the PostgreSQL binary multirange format: int32 range count, then each
/// range as int32 length + its range binary.
fn decode_binary_multirange<'a>(
    kind: crate::sql::types::RangeKind,
    bytes: &'a [u8],
    arena: &'a Arena,
) -> Result<Datum<'a>, SqlError> {
    let bad = || sql_err!(sqlstate::BAD_COPY_FILE_FORMAT, "invalid binary multirange");
    let mut reader = crate::pg::wire::MsgIn::new(bytes);
    let count = reader.i32().map_err(|_| bad())?;
    if !(0..=crate::sql::range::MAX_MULTIRANGE as i32).contains(&count) {
        return Err(sql_err!(sqlstate::PROGRAM_LIMIT_EXCEEDED, "multirange has too many ranges"));
    }
    let mut ranges = [""; crate::sql::range::MAX_MULTIRANGE];
    for slot in ranges.iter_mut().take(count as usize) {
        let len = reader.i32().map_err(|_| bad())?;
        let field = reader.take(len as usize).map_err(|_| bad())?;
        let mut inner = crate::pg::wire::MsgIn::new(field);
        *slot = decode_range_body(kind, &mut inner, arena)?;
    }
    let text = crate::sql::range::canonicalize_multirange(&mut ranges[..count as usize], kind, arena)?;
    Ok(Datum::Multirange { text, kind })
}

/// Decodes the PostgreSQL binary bit-string format: int32 bit length, then
/// ceil(len/8) bytes packed MSB-first, into a `0`/`1` string.
fn decode_binary_bit<'a>(
    varying: bool,
    bytes: &'a [u8],
    arena: &'a Arena,
) -> Result<Datum<'a>, SqlError> {
    let bad = || sql_err!(sqlstate::BAD_COPY_FILE_FORMAT, "invalid binary bit string");
    let mut reader = crate::pg::wire::MsgIn::new(bytes);
    let bit_len = reader.i32().map_err(|_| bad())?;
    if bit_len < 0 {
        return Err(bad());
    }
    let bit_len = bit_len as usize;
    let packed = reader.take(bit_len.div_ceil(8)).map_err(|_| bad())?;
    let bits = arena
        .alloc_slice_with(bit_len, |i| {
            if packed[i / 8] & (0x80 >> (i % 8)) != 0 {
                b'1'
            } else {
                b'0'
            }
        })
        .map_err(|_| sql_err!(sqlstate::PROGRAM_LIMIT_EXCEEDED, "bit string exceeds the statement arena"))?;
    let bits = core::str::from_utf8(bits).map_err(|_| bad())?;
    Ok(Datum::Bit { bits, varying })
}

/// COPY TO STDOUT: every visible row, targets in COPY order, each value in
/// its wire text form with COPY's escapes. Returns the row count for the
/// command tag.
pub fn copy_out(
    storage: &Storage,
    txid: u32,
    setup: &CopySetup,
    arena: &Arena,
    responder: &mut Responder,
) -> Result<u64, SqlError> {
    let def = storage.table(setup.table_index).def;
    let mut schema = [ColType::Bool; MAX_COLUMNS];
    def.schema(&mut schema);
    let fmt = &setup.fmt;
    responder.copy_out_response(setup.n_targets, fmt.binary).map_err(wire_to_sql)?;
    if fmt.binary {
        responder.copy_binary_header().map_err(wire_to_sql)?;
    }
    // A header line of column names, in the same field format as the data.
    if fmt.header {
        responder
            .copy_data_row(&|out| {
                for i in 0..setup.n_targets {
                    if i > 0 {
                        out(&[fmt.delimiter]);
                    }
                    let name = def.columns()[setup.targets[i]].name.as_str();
                    if fmt.csv {
                        crate::sql::copy::encode_field_csv(
                            out, Some(name), fmt.null.as_str(), fmt.delimiter, fmt.quote,
                            fmt.escape, false,
                        );
                    } else {
                        crate::sql::copy::encode_field(out, Some(name));
                    }
                }
            })
            .map_err(wire_to_sql)?;
    }
    // Rowid order is insertion order (rowids are monotonic), which is the
    // order PostgreSQL's COPY TO emits for a freshly-loaded table. Snapshot
    // the visible tokens first, sort, then stream.
    let mut visible = 0usize;
    storage.for_each_row_state(setup.table_index, &mut |_, state| {
        if state.visible_at(txid, storage.read_snapshot()).is_some() {
            visible += 1;
        }
        Ok(core::ops::ControlFlow::Continue(()))
    })?;
    let tokens = arena
        .alloc_slice_with(visible, |_| (0u64, crate::storage::RowHome::Heap(crate::storage::RowLoc { offset: 0, len: 0 })))
        .map_err(|_| sql_err!(sqlstate::PROGRAM_LIMIT_EXCEEDED, "COPY TO snapshot exceeds the statement arena"))?;
    let mut fill = 0usize;
    storage.for_each_row_state(setup.table_index, &mut |rowid, state| {
        if let Some(home) = state.visible_at(txid, storage.read_snapshot()) {
            tokens[fill] = (rowid, home);
            fill += 1;
        }
        Ok(core::ops::ControlFlow::Continue(()))
    })?;
    tokens.sort_unstable_by_key(|(rowid, _)| *rowid);
    let mut count = 0u64;
    for &(rowid, home) in tokens.iter() {
        storage.with_row_bytes(setup.table_index, rowid, home, |bytes| {
            let mut values = [Datum::Null; MAX_COLUMNS];
            rowenc::decode(bytes, &schema[..def.n_columns], &mut values)?;
            if fmt.binary {
                // Each field is its value's binary form (int32 length + bytes,
                // or -1 for NULL). Ranges and multiranges are pre-parsed into
                // arena datums here (fallible, once); every other type encodes
                // arena-free. The emission closure then runs deterministically
                // (it may run twice on the flush-and-retry path).
                let mut plans = [BinaryFieldPlan::Direct; MAX_COLUMNS];
                for (i, plan) in plans.iter_mut().enumerate().take(setup.n_targets) {
                    *plan = binary_field_plan(&values[setup.targets[i]], arena)?;
                }
                responder
                    .copy_binary_row(setup.n_targets, &|m| {
                        for i in 0..setup.n_targets {
                            match plans[i] {
                                BinaryFieldPlan::Direct => {
                                    Responder::encode_value_binary(m, &values[setup.targets[i]]);
                                }
                                BinaryFieldPlan::Range(f, l, u) => {
                                    m.field(|m| encode_range_binary(m, f, l, u));
                                }
                                BinaryFieldPlan::Multirange(ranges) => {
                                    m.field(|m| {
                                        m.i32(ranges.len() as i32);
                                        for &(f, l, u) in ranges {
                                            m.field(|m| encode_range_binary(m, f, l, u));
                                        }
                                    });
                                }
                            }
                        }
                    })
                    .map_err(wire_to_sql)?;
                return Ok(());
            }
            // Render each target into the arena first (fallible), so the
            // wire write below is a deterministic, retry-safe emission.
            let render = responder.render_context();
            let mut texts: [Option<&str>; MAX_COLUMNS] = [None; MAX_COLUMNS];
            for (i, texts_slot) in texts.iter_mut().enumerate().take(setup.n_targets) {
                // The wire-text output function, exactly as a SELECT would
                // render it — styled timestamps, GUC-honoring bytea, `t`
                // for booleans — then COPY's escapes on top below.
                *texts_slot =
                    Responder::datum_wire_text(&values[setup.targets[i]], render, arena)?;
            }
            responder
                .copy_data_row(&|out| {
                    for (i, text) in texts.iter().enumerate().take(setup.n_targets) {
                        if i > 0 {
                            out(&[fmt.delimiter]);
                        }
                        if fmt.csv {
                            let force = fmt.force_quote_all
                                || CopyFmt::forced(fmt.force_quote, setup.targets[i]);
                            crate::sql::copy::encode_field_csv(
                                out, *text, fmt.null.as_str(), fmt.delimiter, fmt.quote,
                                fmt.escape, force,
                            );
                        } else if let Some(value) = text {
                            crate::sql::copy::encode_field(out, Some(value));
                        } else {
                            out(fmt.null.as_str().as_bytes());
                        }
                    }
                })
                .map_err(wire_to_sql)?;
            Ok(())
        })?;
        count += 1;
    }
    if fmt.binary {
        responder.copy_binary_trailer().map_err(wire_to_sql)?;
    }
    responder.copy_done().map_err(wire_to_sql)?;
    Ok(count)
}

fn wire_to_sql(_: crate::pg::wire::WireFull) -> SqlError {
    sql_err!(sqlstate::PROGRAM_LIMIT_EXCEEDED, "COPY output exceeds the send buffer")
}

/// A range's binary parts: the flags byte, and its two bound datums already
/// parsed from the canonical text (`None` = infinite bound, or both `None` for
/// an empty range).
type RangeBinaryParts<'a> = (u8, Option<Datum<'a>>, Option<Datum<'a>>);

/// Per-field plan for binary COPY output. Scalars, arrays and bit strings
/// encode arena-free (`Direct`); ranges and multiranges are pre-parsed into
/// arena datums up front so the retry-safe emission closure never allocates
/// or fails.
#[derive(Clone, Copy)]
enum BinaryFieldPlan<'a> {
    Direct,
    Range(u8, Option<Datum<'a>>, Option<Datum<'a>>),
    Multirange(&'a [RangeBinaryParts<'a>]),
}

fn binary_field_plan<'a>(
    v: &Datum<'a>,
    arena: &'a Arena,
) -> Result<BinaryFieldPlan<'a>, SqlError> {
    match v {
        Datum::Range { text, kind } => {
            let (flags, lower, upper) = parse_range_bounds(text, *kind, arena)?;
            Ok(BinaryFieldPlan::Range(flags, lower, upper))
        }
        Datum::Multirange { text, kind } => {
            let mut components = [""; crate::sql::range::MAX_MULTIRANGE];
            let n = crate::sql::range::split_components(text, &mut components)?;
            let mut parts: [RangeBinaryParts; crate::sql::range::MAX_MULTIRANGE] =
                [(0u8, None, None); crate::sql::range::MAX_MULTIRANGE];
            for (slot, &component) in parts.iter_mut().zip(components.iter()).take(n) {
                *slot = parse_range_bounds(component, *kind, arena)?;
            }
            let stored = arena.alloc_slice_copy(&parts[..n]).map_err(|_| {
                sql_err!(
                    sqlstate::PROGRAM_LIMIT_EXCEEDED,
                    "COPY BINARY multirange exceeds the statement arena"
                )
            })?;
            Ok(BinaryFieldPlan::Multirange(stored))
        }
        _ => Ok(BinaryFieldPlan::Direct),
    }
}

/// Parses a canonical range text into its PostgreSQL binary flags byte plus the
/// two bound datums. Flags: `0x01` empty, `0x02` lower-inclusive, `0x04`
/// upper-inclusive, `0x08` lower-infinite, `0x10` upper-infinite.
fn parse_range_bounds<'a>(
    text: &str,
    kind: crate::sql::types::RangeKind,
    arena: &'a Arena,
) -> Result<RangeBinaryParts<'a>, SqlError> {
    let parsed = crate::sql::range::parse(text)?;
    if parsed.empty {
        return Ok((0x01, None, None));
    }
    let mut flags = 0u8;
    if parsed.lower_inc {
        flags |= 0x02;
    }
    if parsed.upper_inc {
        flags |= 0x04;
    }
    if parsed.lower.is_none() {
        flags |= 0x08;
    }
    if parsed.upper.is_none() {
        flags |= 0x10;
    }
    let lower = match parsed.lower {
        Some(_) => Some(crate::sql::range::lower_datum(text, kind, arena)?),
        None => None,
    };
    let upper = match parsed.upper {
        Some(_) => Some(crate::sql::range::upper_datum(text, kind, arena)?),
        None => None,
    };
    Ok((flags, lower, upper))
}

/// Writes a range's body (after the outer int32 length): the flags byte, then
/// each finite bound as int32 length + binary via `encode_value_binary`.
fn encode_range_binary(
    m: &mut crate::pg::wire::MsgOut,
    flags: u8,
    lower: Option<Datum>,
    upper: Option<Datum>,
) {
    m.u8(flags);
    if let Some(datum) = lower {
        Responder::encode_value_binary(m, &datum);
    }
    if let Some(datum) = upper {
        Responder::encode_value_binary(m, &datum);
    }
}

/// Computes every `GENERATED ALWAYS AS (expr) STORED` column from the row's
/// already-filled other columns, writing the result into `values`. A generated
/// column never references another generated column (enforced at CREATE), so a
/// snapshot of the row before this pass supplies every dependency.
fn compute_generated<'a>(
    def: &TableDef,
    generated: &constraints::ParsedDefaults<'a>,
    values: &mut [Datum<'a>; MAX_COLUMNS],
    storage: &Storage,
    arena: &'a Arena,
) -> Result<(), SqlError> {
    if !generated.iter().any(|g| g.is_some()) {
        return Ok(());
    }
    let snapshot: [Datum<'a>; MAX_COLUMNS] = *values;
    let context = RowCtx { def, values: &snapshot[..def.n_columns] };
    for (i, g) in generated.iter().enumerate() {
        if let Some(expr) = g {
            let v = eval(expr, arena, crate::sql::eval::NO_PARAMS, &context)?;
            values[i] = coerce(v, &def.columns()[i], storage, arena)?;
        }
    }
    Ok(())
}

/// What to do with an explicitly supplied value for a column, given the
/// statement's `OVERRIDING` mode.
#[derive(PartialEq)]
enum IdentityAction {
    /// Use the supplied value (a normal column, or an allowed identity write).
    Accept,
    /// Reject it (428C9) — a `GENERATED ALWAYS` identity without `OVERRIDING
    /// SYSTEM VALUE`.
    Reject,
    /// Ignore the supplied value and use the identity sequence — `OVERRIDING
    /// USER VALUE` on a `GENERATED BY DEFAULT` identity.
    UseSequence,
}

/// Decides how a supplied value for `column` is treated under `overriding`.
fn identity_action(def: &TableDef, column: usize, overriding: Overriding) -> IdentityAction {
    let col = &def.columns()[column];
    if !col.is_identity {
        return IdentityAction::Accept;
    }
    if col.identity_always {
        if overriding == Overriding::System {
            IdentityAction::Accept
        } else {
            IdentityAction::Reject
        }
    } else if overriding == Overriding::User {
        IdentityAction::UseSequence
    } else {
        IdentityAction::Accept
    }
}

/// The 428C9 error PostgreSQL raises for an explicit write to a `GENERATED
/// ALWAYS` identity column without `OVERRIDING SYSTEM VALUE`.
fn reject_identity_write(def: &TableDef, column: usize) -> SqlError {
    sql_err!(
        sqlstate::GENERATED_ALWAYS,
        "cannot insert a non-DEFAULT value into column \"{}\"",
        def.columns()[column].name.as_str()
    )
}

/// Rejects an explicit non-`DEFAULT` value written to a generated column (428C9),
/// the error PostgreSQL raises for `INSERT`/`UPDATE` on such a column.
fn reject_generated_write(def: &TableDef, column: usize) -> Result<(), SqlError> {
    if def.columns()[column].is_generated {
        return Err(sql_err!(
            sqlstate::GENERATED_ALWAYS,
            "cannot insert a non-DEFAULT value into column \"{}\"",
            def.columns()[column].name.as_str()
        ));
    }
    Ok(())
}

/// A two-table column lookup for MERGE's ON / WHEN-condition / SET expressions:
/// a qualified name resolves to the target or source half; an unqualified name
/// searches both (ambiguous if in both, 42702).
struct MergeLookup<'d, 'v> {
    target_def: &'d TableDef,
    target_alias: &'d str,
    target: &'v [Datum<'v>],
    source_def: &'d TableDef,
    source_alias: &'d str,
    source: &'v [Datum<'v>],
}

impl<'v> ColumnLookup<'v> for MergeLookup<'_, 'v> {
    fn lookup(&self, qualifier: Option<&str>, name: &str) -> Result<Datum<'v>, SqlError> {
        match qualifier {
            Some(q) if q == self.target_alias => self
                .target_def
                .column_index(name)
                .map(|i| self.target[i])
                .ok_or_else(|| undefined_column(name)),
            Some(q) if q == self.source_alias => self
                .source_def
                .column_index(name)
                .map(|i| self.source[i])
                .ok_or_else(|| undefined_column(name)),
            Some(q) => Err(sql_err!(
                sqlstate::UNDEFINED_TABLE,
                "missing FROM-clause entry for table \"{}\"",
                q
            )),
            None => match (
                self.target_def.column_index(name),
                self.source_def.column_index(name),
            ) {
                (Some(_), Some(_)) => Err(sql_err!(
                    sqlstate::AMBIGUOUS_COLUMN,
                    "column reference \"{}\" is ambiguous",
                    name
                )),
                (Some(i), None) => Ok(self.target[i]),
                (None, Some(i)) => Ok(self.source[i]),
                (None, None) => Err(undefined_column(name)),
            },
        }
    }
}

/// `MERGE INTO target USING source ON cond WHEN ...`. Source-driven: each source
/// row is matched against the target on `cond`; a match applies the first
/// satisfied WHEN MATCHED clause, a miss the first WHEN NOT MATCHED clause. A
/// target row affected twice is a cardinality error (21000).
#[allow(clippy::too_many_arguments)]
pub fn merge(
    storage: &mut Storage,
    txn: &mut TxnState,
    _scratch: &mut FixedVec<(u64, RowHome)>,
    statement: &crate::sql::ast::Merge,
    arena: &Arena,
    params: &[Datum],
    seq_session: &crate::sql::guc::SeqSession,
    responder: &mut Responder,
) -> Outcome {
    use crate::sql::ast::MergeAction;
    let target_qual = crate::sql::ast::QualName {
        schema: statement.target.schema,
        name: statement.target.name,
    };
    let table_index = match resolve_dml_table(storage, &target_qual, txn.txid) {
        Ok(i) => i,
        Err(e) => return sql_fail(e),
    };
    let def = storage.table(table_index).def;
    let target_alias = statement.target_alias.unwrap_or(statement.target.name);
    let source_alias = statement
        .source
        .alias
        .unwrap_or(if statement.source.table.is_empty() { "" } else { statement.source.table });

    // Materialize the source as `SELECT * FROM <source>`: its column set (a
    // synthesized def) and its rows.
    let source_from = crate::sql::ast::FromClause { base: statement.source, joins: &[] };
    let star = match arena.alloc_slice_copy(&[SelectItem::Wildcard]) {
        Ok(s) => &*s,
        Err(_) => return sql_fail(super::query::arena_full_pub()),
    };
    let source_select = crate::sql::ast::Select {
        items: star,
        distinct: false,
        distinct_on: &[],
        from: Some(source_from),
        where_clause: None,
        group_by: &[],
        grouping_sets: &[],
        having: None,
        order_by: &[],
        limit: None,
        offset: None,
        with_ties: false,
        with: &[],
        set_body: None,
    };
    // Copy the synthesized def out of the borrow (it is tied to `storage`),
    // so the write path below can borrow storage mutably.
    let source_def = match super::query::synth_derived_def(
        storage,
        &source_select,
        source_alias,
        statement.source.col_alias,
        txn.txid,
        arena,
    ) {
        Ok(d) => *d,
        Err(e) => return sql_fail(e),
    };
    let source_def = &source_def;
    // Pass 1: count source rows. Pass 2: encode each to arena bytes.
    let mut n_source = 0usize;
    if let Err(e) = super::query::select_into_rows(
        storage, txn.txid, &source_select, arena, params, None, None, &mut |_| {
            n_source += 1;
            Ok(())
        },
    ) {
        return sql_fail(e);
    }
    let empty: &[u8] = &[];
    let source_rows: &mut [&[u8]] = match arena.alloc_slice_with(n_source, |_| empty) {
        Ok(r) => r,
        Err(_) => return sql_fail(super::query::arena_full_pub()),
    };
    {
        let mut at = 0usize;
        if let Err(e) = super::query::select_into_rows(
            storage, txn.txid, &source_select, arena, params, None, None, &mut |vals| {
                source_rows[at] = encode_projected_pub(vals, arena)?;
                at += 1;
                Ok(())
            },
        ) {
            return sql_fail(e);
        }
    }

    // Collect the target rows once (rowid + decoded values), plus an affected
    // flag per row for the cardinality check.
    let mut target_schema = [ColType::Bool; MAX_COLUMNS];
    def.schema(&mut target_schema);
    let target_schema = &target_schema[..def.n_columns];
    let n_target = match storage.visible_row_count(table_index, txn.txid) {
        Ok(n) => n,
        Err(e) => return sql_fail(e),
    };
    let target_ids: &mut [u64] = match arena.alloc_slice_with(n_target, |_| 0u64) {
        Ok(s) => s,
        Err(_) => return sql_fail(super::query::arena_full_pub()),
    };
    let target_vals: &mut [&[Datum]] = match arena.alloc_slice_with(n_target, |_| &[][..]) {
        Ok(s) => s,
        Err(_) => return sql_fail(super::query::arena_full_pub()),
    };
    let affected: &mut [bool] = match arena.alloc_slice_with(n_target, |_| false) {
        Ok(s) => s,
        Err(_) => return sql_fail(super::query::arena_full_pub()),
    };
    {
        use core::ops::ControlFlow;
        // Snapshot rowid + home first (the closure cannot borrow the arena while
        // `storage` is borrowed), then decode.
        let placeholder = crate::storage::RowHome::Spilled { len: 0, sst: 0 };
        let ids: &mut [u64] = target_ids;
        let hms: &mut [crate::storage::RowHome] =
            match arena.alloc_slice_with(n_target, |_| placeholder) {
                Ok(s) => s,
                Err(_) => return sql_fail(super::query::arena_full_pub()),
            };
        let mut k = 0usize;
        if let Err(e) = storage.for_each_row_state(table_index, &mut |rowid, state| {
            if let Some(home) = state.visible_at(txn.txid, storage.read_snapshot())
                && k < ids.len()
            {
                ids[k] = rowid;
                hms[k] = home;
                k += 1;
            }
            Ok(ControlFlow::Continue(()))
        }) {
            return sql_fail(e);
        }
        for j in 0..k {
            let fetched = match storage.row_bytes(table_index, ids[j], hms[j], arena) {
                Ok(b) => b,
                Err(e) => return sql_fail(e),
            };
            // Copy into the arena so the decoded datums do not borrow storage
            // (the write path below borrows it mutably).
            let bytes = match arena.alloc_slice_copy(fetched) {
                Ok(b) => &*b,
                Err(_) => return sql_fail(super::query::arena_full_pub()),
            };
            let mut vals = [Datum::Null; MAX_COLUMNS];
            if let Err(e) = rowenc::decode(bytes, target_schema, &mut vals) {
                return sql_fail(e);
            }
            let owned = match arena.alloc_slice_copy(&vals[..def.n_columns]) {
                Ok(v) => &*v,
                Err(_) => return sql_fail(super::query::arena_full_pub()),
            };
            target_vals[j] = owned;
        }
    }

    let checks = match parse_checks(&def, arena) {
        Ok(c) => c,
        Err(e) => return sql_fail(e),
    };
    let generated = match parse_generated(&def, arena) {
        Ok(g) => g,
        Err(e) => return sql_fail(e),
    };
    let defaults = match parse_defaults(&def, arena) {
        Ok(d) => d,
        Err(e) => return sql_fail(e),
    };

    let mut affected_count = 0u64;
    for sbytes in source_rows.iter() {
        let n_src_cols = sbytes[0] as usize;
        let mut sv = [Datum::Null; MAX_COLUMNS];
        for (c, slot) in sv.iter_mut().enumerate().take(n_src_cols) {
            *slot = decode_projected_pub(sbytes, c);
        }
        let sv = &sv[..source_def.n_columns.min(n_src_cols)];
        let mut matched = false;
        for j in 0..n_target {
            let lookup = MergeLookup {
                target_def: &def,
                target_alias,
                target: target_vals[j],
                source_def,
                source_alias,
                source: sv,
            };
            match eval(statement.on, arena, params, &lookup) {
                Ok(Datum::Bool(true)) => {}
                Ok(_) => continue,
                Err(e) => return sql_fail(e),
            }
            matched = true;
            // First satisfied WHEN MATCHED clause.
            for when in statement.whens.iter().filter(|w| w.matched) {
                if let Some(cond) = when.cond {
                    match eval(cond, arena, params, &lookup) {
                        Ok(Datum::Bool(true)) => {}
                        Ok(_) => continue,
                        Err(e) => return sql_fail(e),
                    }
                }
                match &when.action {
                    MergeAction::DoNothing => {}
                    MergeAction::Delete => {
                        if affected[j] {
                            return sql_fail(merge_cardinality());
                        }
                        affected[j] = true;
                        match storage.write_pending(table_index, target_ids[j], txn.txid, txn.command_id(), None) {
                            Ok(prior) => {
                                if let Err(e) = txn.touch(table_index as u32, target_ids[j], prior) {
                                    return sql_fail(e);
                                }
                            }
                            Err(e) => return sql_fail(e),
                        }
                        affected_count += 1;
                    }
                    MergeAction::Update(assignments) => {
                        if affected[j] {
                            return sql_fail(merge_cardinality());
                        }
                        let mut new_values = [Datum::Null; MAX_COLUMNS];
                        new_values[..def.n_columns].copy_from_slice(target_vals[j]);
                        for (name, expression) in assignments.iter() {
                            let Some(ci) = def.column_index(name) else {
                                return sql_fail(undefined_column(name));
                            };
                            let v = match eval(expression, arena, params, &lookup) {
                                Ok(v) => v,
                                Err(e) => return sql_fail(e),
                            };
                            match coerce(v, &def.columns()[ci], storage, arena) {
                                Ok(v) => new_values[ci] = v,
                                Err(e) => return sql_fail(e),
                            }
                        }
                        if let Err(e) = compute_generated(&def, &generated, &mut new_values, storage, arena) {
                            return sql_fail(e);
                        }
                        if let Err(e) = check_not_null(&def, &new_values) {
                            return sql_fail(e);
                        }
                        if let Err(e) = enforce_row_constraints(
                            storage, table_index, &def, target_schema,
                            &new_values[..def.n_columns], Some(target_ids[j]), txn.txid,
                            &checks, arena, params,
                        ) {
                            return sql_fail(e);
                        }
                        let len = rowenc::encoded_len(&new_values[..def.n_columns]);
                        let out = match arena.alloc_slice_with(len, |_| 0u8) {
                            Ok(o) => o,
                            Err(_) => return sql_fail(super::query::arena_full_pub()),
                        };
                        rowenc::encode(&new_values[..def.n_columns], out);
                        let (loc, slice) = match storage.heap.append(out.len()) {
                            Ok(x) => x,
                            Err(e) => return sql_fail(e),
                        };
                        slice.copy_from_slice(out);
                        match storage.write_pending(table_index, target_ids[j], txn.txid, txn.command_id(), Some(loc)) {
                            Ok(prior) => {
                                if let Err(e) = txn.touch(table_index as u32, target_ids[j], prior) {
                                    storage.restore_pending(table_index, target_ids[j], txn.txid, prior);
                                    return sql_fail(e);
                                }
                            }
                            Err(e) => return sql_fail(e),
                        }
                        affected[j] = true;
                        affected_count += 1;
                    }
                    MergeAction::Insert { .. } => {
                        return sql_fail(sql_err!(
                            sqlstate::SYNTAX_ERROR,
                            "INSERT is not allowed in a WHEN MATCHED clause"
                        ));
                    }
                }
                break;
            }
        }
        if !matched {
            // First satisfied WHEN NOT MATCHED clause (source columns only).
            let source_ctx = RowCtx { def: source_def, values: sv };
            for when in statement.whens.iter().filter(|w| !w.matched) {
                if let Some(cond) = when.cond {
                    match eval(cond, arena, params, &source_ctx) {
                        Ok(Datum::Bool(true)) => {}
                        Ok(_) => continue,
                        Err(e) => return sql_fail(e),
                    }
                }
                match &when.action {
                    MergeAction::DoNothing => {}
                    MergeAction::Insert { columns, values, default_values } => {
                        if let Err(e) = merge_insert(
                            storage, txn, table_index, &def, columns, values, *default_values,
                            &source_ctx, &generated, &defaults, seq_session, arena, params, &checks,
                        ) {
                            return sql_fail(e);
                        }
                        affected_count += 1;
                    }
                    _ => {
                        return sql_fail(sql_err!(
                            sqlstate::SYNTAX_ERROR,
                            "only INSERT / DO NOTHING is allowed in a WHEN NOT MATCHED clause"
                        ));
                    }
                }
                break;
            }
        }
    }
    responder.command_complete(stack_format!(32, "MERGE {}", affected_count).as_str())?;
    sql_ok()
}

/// The 21000 error PostgreSQL raises when a MERGE would affect a target row a
/// second time.
fn merge_cardinality() -> SqlError {
    sql_err!(
        sqlstate::CARDINALITY_VIOLATION,
        "MERGE command cannot affect row a second time"
    )
}

/// Applies a MERGE `WHEN NOT MATCHED THEN INSERT`: builds the row from the
/// clause (values evaluated with the source row in scope), fills defaults and
/// generated columns, and stores it.
#[allow(clippy::too_many_arguments)]
fn merge_insert(
    storage: &mut Storage,
    txn: &mut TxnState,
    table_index: usize,
    def: &TableDef,
    columns: &[&str],
    values: &[&Expr],
    default_values: bool,
    source_ctx: &RowCtx,
    generated: &constraints::ParsedDefaults,
    defaults: &constraints::ParsedDefaults,
    seq_session: &crate::sql::guc::SeqSession,
    arena: &Arena,
    params: &[Datum],
    checks: &ParsedChecks,
) -> Result<(), SqlError> {
    // Target columns for the supplied values: the named list, or all columns.
    let mut targets = [0usize; MAX_COLUMNS];
    let n_targets = if columns.is_empty() {
        for (i, t) in targets.iter_mut().enumerate().take(def.n_columns) {
            *t = i;
        }
        def.n_columns
    } else {
        for (i, name) in columns.iter().enumerate() {
            let Some(ci) = def.column_index(name) else {
                return Err(undefined_column(name));
            };
            targets[i] = ci;
        }
        columns.len()
    };
    let mut row = [Datum::Null; MAX_COLUMNS];
    let mut explicit = [false; MAX_COLUMNS];
    if !default_values {
        if values.len() != n_targets {
            return Err(sql_err!(
                sqlstate::SYNTAX_ERROR,
                "MERGE INSERT has {} expressions but {} target columns",
                values.len(),
                n_targets
            ));
        }
        let seq = crate::sql::sequence::SeqEval::new(storage, seq_session, txn.txid);
        let hooks = super::eval::EvalHooks { sequences: Some(&seq), ..super::eval::NO_HOOKS };
        for (i, expression) in values.iter().enumerate() {
            reject_generated_write(def, targets[i])?;
            let v = super::eval::eval_full(expression, arena, params, source_ctx, &hooks)?;
            row[targets[i]] = coerce(v, &def.columns()[targets[i]], storage, arena)?;
            explicit[targets[i]] = true;
        }
    }
    // Defaults + auto-increment + generated for the unset columns.
    {
        let seq = crate::sql::sequence::SeqEval::new(storage, seq_session, txn.txid);
        let hooks = super::eval::EvalHooks { sequences: Some(&seq), ..super::eval::NO_HOOKS };
        for (i, col) in def.columns().iter().enumerate() {
            if explicit[i] {
                continue;
            }
            if let Some(d) = &col.default_value {
                row[i] = d.as_datum();
            } else if let Some(expr) = defaults[i] {
                let v = super::eval::eval_full(expr, arena, crate::sql::eval::NO_PARAMS, &NoColumns, &hooks)?;
                row[i] = coerce(v, col, storage, arena)?;
            }
        }
    }
    fill_auto_increment(storage, table_index, def, &mut row, &explicit)?;
    let mut row_arr = row;
    compute_generated(def, generated, &mut row_arr, storage, arena)?;
    check_not_null(def, &row_arr)?;
    let mut schema = [ColType::Bool; MAX_COLUMNS];
    def.schema(&mut schema);
    enforce_row_constraints(
        storage, table_index, def, &schema[..def.n_columns],
        &row_arr[..def.n_columns], None, txn.txid, checks, arena, params,
    )?;
    store_row(storage, txn, table_index, None, &row_arr[..def.n_columns])
}

#[allow(clippy::too_many_arguments, clippy::type_complexity)]
pub fn insert(
    storage: &mut Storage,
    txn: &mut TxnState,
    statement: &Insert,
    arena: &Arena,
    params: &[Datum],
    seq_session: &crate::sql::guc::SeqSession,
    responder: &mut Responder,
    mut capture: Option<&mut dyn FnMut(&[Datum]) -> Result<(), SqlError>>,
) -> Outcome {
    let capturing = capture.is_some();
    let table_index = match resolve_dml_table(storage, &statement.table, txn.txid) {
        Ok(i) => i,
        Err(e) => return sql_fail(e),
    };
    let def = storage.table(table_index).def;
    let checks = match parse_checks(&def, arena) {
        Ok(c) => c,
        Err(e) => return sql_fail(e),
    };

    // Column list → target indices.
    let mut targets = [0usize; MAX_COLUMNS];
    let n_targets = if statement.columns.is_empty() {
        for (i, t) in targets.iter_mut().enumerate().take(def.n_columns) {
            *t = i;
        }
        def.n_columns
    } else {
        for (i, name) in statement.columns.iter().enumerate() {
            let Some(col) = def.column_index(name) else {
                return sql_fail(sql_err!(
                    sqlstate::UNDEFINED_COLUMN,
                    "column \"{}\" of relation \"{}\" does not exist",
                    name,
                    statement.table.name
                ));
            };
            targets[i] = col;
        }
        statement.columns.len()
    };

    // RETURNING sends its RowDescription before any rows — unless the rows are
    // being captured for a data-modifying CTE, which describes them itself.
    if !statement.returning.is_empty() && !capturing {
        let mut columns = [ColDesc::new("", 0, 0); MAX_PROJ];
        match describe_items(statement.returning, Some(&def), &mut columns) {
            Ok(n) => responder.row_description(&columns[..n])?,
            Err(e) => return sql_fail(e),
        }
    }

    // INSERT ... SELECT: materialize the source rows into the arena first
    // (reading storage immutably), then insert them (mutably) — the source may
    // read the very table being written, so the two phases must not overlap.
    if let Some(sel) = statement.select {
        // Pass 1: count. A "dry" sequence evaluator resolves names (so errors
        // still surface) but does not advance any generator — the real advance
        // happens once, in the encoding pass.
        let mut count = 0usize;
        {
            let dry = crate::sql::sequence::SeqEval::dry(storage, seq_session, txn.txid);
            if let Err(e) = super::query::select_into_rows(
                storage, txn.txid, sel, arena, params, None, Some(&dry), &mut |_| {
                    count += 1;
                    Ok(())
                },
            ) {
                return sql_fail(e);
            }
        }
        // Pass 2: encode each projected row to self-describing arena bytes.
        let empty: &[u8] = &[];
        let rows_bytes: &mut [&[u8]] = match arena.alloc_slice_with(count, |_| empty) {
            Ok(r) => r,
            Err(_) => return sql_fail(sql_err!(
                sqlstate::PROGRAM_LIMIT_EXCEEDED,
                "INSERT ... SELECT result exceeds the statement arena"
            )),
        };
        let mut at = 0usize;
        let mut fill = |vals: &[Datum]| -> Result<(), SqlError> {
            rows_bytes[at] = encode_projected_pub(vals, arena)?;
            at += 1;
            Ok(())
        };
        {
            let live = crate::sql::sequence::SeqEval::new(storage, seq_session, txn.txid);
            if let Err(e) = super::query::select_into_rows(storage, txn.txid, sel, arena, params, None, Some(&live), &mut fill) {
                return sql_fail(e);
            }
        }

        let default_exprs = match parse_defaults(&def, arena) {
            Ok(d) => d,
            Err(e) => return sql_fail(e),
        };
        let generated_exprs = match parse_generated(&def, arena) {
            Ok(g) => g,
            Err(e) => return sql_fail(e),
        };
        let mut inserted = 0u64;
        for bytes in rows_bytes.iter() {
            let n_src = bytes[0] as usize;
            if n_src != n_targets {
                let msg = if n_src > n_targets {
                    "INSERT has more expressions than target columns"
                } else {
                    "INSERT has more target columns than expressions"
                };
                return sql_fail(sql_err!(sqlstate::SYNTAX_ERROR, "{}", msg));
            }
            let mut values = [Datum::Null; MAX_COLUMNS];
            let mut explicit = [false; MAX_COLUMNS];
            for i in 0..n_src {
                // A generated column cannot be a target of INSERT ... SELECT.
                if let Err(e) = reject_generated_write(&def, targets[i]) {
                    return sql_fail(e);
                }
                match identity_action(&def, targets[i], statement.overriding) {
                    IdentityAction::Reject => return sql_fail(reject_identity_write(&def, targets[i])),
                    // OVERRIDING USER VALUE: skip the query's value, use identity.
                    IdentityAction::UseSequence => continue,
                    IdentityAction::Accept => {}
                }
                let v = decode_projected_pub(bytes, i);
                let col = &def.columns()[targets[i]];
                match coerce(v, col, storage, arena) {
                    Ok(v) => values[targets[i]] = v,
                    Err(e) => return sql_fail(e),
                }
                explicit[targets[i]] = true;
            }
            // Defaults for columns the query does not supply: a folded constant,
            // or a per-row DEFAULT expression (evaluated under a scoped sequence
            // evaluator, so a `nextval` default advances once per inserted row).
            {
                let seq = crate::sql::sequence::SeqEval::new(storage, seq_session, txn.txid);
                let hooks =
                    super::eval::EvalHooks { sequences: Some(&seq), ..super::eval::NO_HOOKS };
                for (i, col) in def.columns().iter().enumerate() {
                    if explicit[i] {
                        continue;
                    }
                    if let Some(d) = &col.default_value {
                        values[i] = d.as_datum();
                    } else if let Some(expr) = default_exprs[i] {
                        let v = match super::eval::eval_full(
                            expr,
                            arena,
                            crate::sql::eval::NO_PARAMS,
                            &NoColumns,
                            &hooks,
                        ) {
                            Ok(v) => v,
                            Err(e) => return sql_fail(e),
                        };
                        match coerce(v, col, storage, arena) {
                            Ok(v) => values[i] = v,
                            Err(e) => return sql_fail(e),
                        }
                    }
                }
            }
            if let Err(e) = fill_auto_increment(storage, table_index, &def, &mut values, &explicit) {
                return sql_fail(e);
            }
            if let Err(e) = compute_generated(&def, &generated_exprs, &mut values, storage, arena) {
                return sql_fail(e);
            }
            if let Err(e) = check_not_null(&def, &values) {
                return sql_fail(e);
            }
            {
                let mut sch = [ColType::Bool; MAX_COLUMNS];
                def.schema(&mut sch);
                match handle_conflict(storage, txn, table_index, &def, &sch[..def.n_columns], &values[..def.n_columns], &statement.on_conflict, &checks, arena, params) {
                    Ok(ConflictOutcome::Store) => {}
                    Ok(ConflictOutcome::Skip) => continue,
                    Ok(ConflictOutcome::Updated) => { inserted += 1; continue; }
                    Err(e) => return sql_fail(e),
                }
            }
            let mut schema_buf = [ColType::Bool; MAX_COLUMNS];
            def.schema(&mut schema_buf);
            if let Err(e) = enforce_row_constraints(
                storage,
                table_index,
                &def,
                &schema_buf[..def.n_columns],
                &values[..def.n_columns],
                None,
                txn.txid,
                &checks,
                arena,
                params,
            ) {
                return sql_fail(e);
            }
            if let Err(e) = store_row(storage, txn, table_index, None, &values[..def.n_columns]) {
                return sql_fail(e);
            }
            if !statement.returning.is_empty()
                && let Err(e) = emit_projected(&def, &values[..def.n_columns], statement.returning, arena, params, responder, &mut capture)? {
                    return sql_fail(e);
                }
            inserted += 1;
        }
        let tag = stack_format!(48, "INSERT 0 {}", inserted);
        if !capturing { responder.command_complete(tag.as_str())?; }
        return sql_ok();
    }

    // Non-constant DEFAULT expressions (now(), nextval(...), …) and GENERATED
    // expressions, re-parsed once and evaluated per row below.
    let default_exprs = match parse_defaults(&def, arena) {
        Ok(d) => d,
        Err(e) => return sql_fail(e),
    };
    let generated_exprs = match parse_generated(&def, arena) {
        Ok(g) => g,
        Err(e) => return sql_fail(e),
    };
    let mut inserted = 0u64;
    for row_exprs in statement.rows {
        if row_exprs.len() > n_targets {
            return sql_fail(sql_err!(
                sqlstate::SYNTAX_ERROR,
                "INSERT has more expressions than target columns"
            ));
        }
        // A column is "explicit" when the row supplies a non-DEFAULT value for
        // it; only the others take their default (so a supplied value does not
        // waste a `nextval`). The datums borrow `def`, which outlives the row.
        let mut values = [Datum::Null; MAX_COLUMNS];
        let mut explicit = [false; MAX_COLUMNS];
        // `ignore[i]` marks a supplied value that OVERRIDING USER VALUE discards
        // in favor of the identity sequence.
        let mut ignore = [false; MAX_COLUMNS];
        for (i, expression) in row_exprs.iter().enumerate() {
            if !matches!(expression, Expr::DefaultMarker) {
                // A generated column rejects any explicit non-DEFAULT value.
                if let Err(e) = reject_generated_write(&def, targets[i]) {
                    return sql_fail(e);
                }
                match identity_action(&def, targets[i], statement.overriding) {
                    IdentityAction::Reject => return sql_fail(reject_identity_write(&def, targets[i])),
                    IdentityAction::UseSequence => ignore[targets[i]] = true,
                    IdentityAction::Accept => explicit[targets[i]] = true,
                }
            }
        }
        {
            // A per-row sequence evaluator (`nextval`/`setval` in a VALUES item
            // or a DEFAULT expression advance once per row). Scoped so its shared
            // `&storage` borrow ends before the row is written mutably below.
            let seq = crate::sql::sequence::SeqEval::new(storage, seq_session, txn.txid);
            let hooks = super::eval::EvalHooks { sequences: Some(&seq), ..super::eval::NO_HOOKS };
            for (i, expression) in row_exprs.iter().enumerate() {
                if matches!(expression, Expr::DefaultMarker) || ignore[targets[i]] {
                    continue; // filled from the default / identity below
                }
                let v = match super::eval::eval_full(expression, arena, params, &NoColumns, &hooks) {
                    Ok(v) => v,
                    Err(e) => return sql_fail(e),
                };
                let col = &def.columns()[targets[i]];
                match coerce(v, col, storage, arena) {
                    Ok(v) => values[targets[i]] = v,
                    Err(e) => return sql_fail(e),
                }
            }
            // Defaults for the columns the row did not set explicitly.
            for (i, col) in def.columns().iter().enumerate() {
                if explicit[i] {
                    continue;
                }
                if let Some(d) = &col.default_value {
                    values[i] = d.as_datum();
                } else if let Some(expr) = default_exprs[i] {
                    let v = match super::eval::eval_full(
                        expr,
                        arena,
                        crate::sql::eval::NO_PARAMS,
                        &NoColumns,
                        &hooks,
                    ) {
                        Ok(v) => v,
                        Err(e) => return sql_fail(e),
                    };
                    match coerce(v, col, storage, arena) {
                        Ok(v) => values[i] = v,
                        Err(e) => return sql_fail(e),
                    }
                }
            }
        }
        if let Err(e) = fill_auto_increment(storage, table_index, &def, &mut values, &explicit) {
            return sql_fail(e);
        }
        // Generated columns are computed last, from the now-filled row.
        if let Err(e) = compute_generated(&def, &generated_exprs, &mut values, storage, arena) {
            return sql_fail(e);
        }
        if let Err(e) = check_not_null(&def, &values) {
            return sql_fail(e);
        }
        {
            let mut sch = [ColType::Bool; MAX_COLUMNS];
            def.schema(&mut sch);
            match handle_conflict(storage, txn, table_index, &def, &sch[..def.n_columns], &values[..def.n_columns], &statement.on_conflict, &checks, arena, params) {
                Ok(ConflictOutcome::Store) => {}
                Ok(ConflictOutcome::Skip) => continue,
                Ok(ConflictOutcome::Updated) => { inserted += 1; continue; }
                Err(e) => return sql_fail(e),
            }
        }
        let mut schema_buf = [ColType::Bool; MAX_COLUMNS];
        def.schema(&mut schema_buf);
        if let Err(e) = enforce_row_constraints(
            storage,
            table_index,
            &def,
            &schema_buf[..def.n_columns],
            &values[..def.n_columns],
            None,
            txn.txid,
            &checks,
            arena,
            params,
        ) {
            return sql_fail(e);
        }
        if let Err(e) = store_row(storage, txn, table_index, None, &values[..def.n_columns]) {
            return sql_fail(e);
        }
        if !statement.returning.is_empty()
            && let Err(e) = emit_projected(&def, &values[..def.n_columns], statement.returning, arena, params, responder, &mut capture)? {
                return sql_fail(e);
            }
        inserted += 1;
    }
    let tag = stack_format!(48, "INSERT 0 {}", inserted);
    if !capturing {
        responder.command_complete(tag.as_str())?;
    }
    sql_ok()
}

/// Projects `values` through `items` and emits one DataRow.
#[allow(clippy::too_many_arguments, clippy::type_complexity)]
fn emit_projected(
    def: &TableDef,
    values: &[Datum],
    items: &[SelectItem],
    arena: &Arena,
    params: &[Datum],
    responder: &mut Responder,
    capture: &mut Option<&mut dyn FnMut(&[Datum]) -> Result<(), SqlError>>,
) -> Result<Result<(), SqlError>, WireFull> {
    let context = RowCtx { def, values };
    let mut projected = [Datum::Null; MAX_PROJ];
    let mut n = 0;
    for item in items {
        match item {
            SelectItem::Wildcard => {
                for v in context.values {
                    projected[n] = *v;
                    n += 1;
                }
            }
            SelectItem::TableWildcard(q) => {
                if !crate::sql::eval::qualifier_answers_single(def, q) {
                    return Ok(Err(sql_err!(
                        sqlstate::UNDEFINED_TABLE,
                        "missing FROM-clause entry for table \"{}\"",
                        q
                    )));
                }
                for v in context.values {
                    projected[n] = *v;
                    n += 1;
                }
            }
            SelectItem::RecordStar(base) => {
                match super::eval::record_star_expand(base, arena, params, &context, &super::eval::NO_HOOKS) {
                    Ok(fields) => {
                        for f in fields {
                            projected[n] = f.value;
                            n += 1;
                        }
                    }
                    Err(e) => return Ok(Err(e)),
                }
            }
            SelectItem::Expr { expression, .. } => match eval(expression, arena, params, &context) {
                Ok(v) => {
                    projected[n] = v;
                    n += 1;
                }
                Err(e) => return Ok(Err(e)),
            },
        }
    }
    // A data-modifying CTE captures its RETURNING rows in memory instead of
    // streaming them to the client.
    if let Some(sink) = capture.as_deref_mut() {
        if let Err(e) = sink(&projected[..n]) {
            return Ok(Err(e));
        }
    } else {
        responder.data_row(&projected[..n])?;
    }
    Ok(Ok(()))
}

#[allow(clippy::too_many_arguments, clippy::type_complexity)]
pub fn update(
    storage: &mut Storage,
    txn: &mut TxnState,
    scratch: &mut FixedVec<(u64, RowHome)>,
    statement: &Update,
    arena: &Arena,
    params: &[Datum],
    seq_session: &crate::sql::guc::SeqSession,
    responder: &mut Responder,
    mut capture: Option<&mut dyn FnMut(&[Datum]) -> Result<(), SqlError>>,
) -> Outcome {
    let capturing = capture.is_some();
    let table_index = match resolve_dml_table(storage, &statement.table, txn.txid) {
        Ok(i) => i,
        Err(e) => return sql_fail(e),
    };
    let def = storage.table(table_index).def;
    let checks = match parse_checks(&def, arena) {
        Ok(c) => c,
        Err(e) => return sql_fail(e),
    };
    let mut schema = [ColType::Bool; MAX_COLUMNS];
    def.schema(&mut schema);
    let schema = &schema[..def.n_columns];

    // Resolve assignment targets once.
    let mut targets = [0usize; MAX_COLUMNS];
    for (i, (name, _)) in statement.assignments.iter().enumerate() {
        let Some(col) = def.column_index(name) else {
            return sql_fail(sql_err!(
                sqlstate::UNDEFINED_COLUMN,
                "column \"{}\" of relation \"{}\" does not exist",
                name,
                statement.table.name
            ));
        };
        targets[i] = col;
    }
    // A generated column can only be updated to DEFAULT (which recomputes it).
    for (a, (_, expression)) in statement.assignments.iter().enumerate() {
        if def.columns()[targets[a]].is_generated && !matches!(expression, Expr::DefaultMarker) {
            return sql_fail(sql_err!(
                sqlstate::GENERATED_ALWAYS,
                "column \"{}\" can only be updated to DEFAULT",
                def.columns()[targets[a]].name.as_str()
            ));
        }
    }
    let generated_exprs = match parse_generated(&def, arena) {
        Ok(g) => g,
        Err(e) => return sql_fail(e),
    };

    let subs = match super::query::subquery_hooks(
        &[statement.where_clause],
        storage,
        txn.txid,
        arena,
        params,
    ) {
        Ok(s) => s,
        Err(e) => return sql_fail(e),
    };
    let hooks = super::eval::EvalHooks { group: None, aggs: None, subs: Some(&subs) , windows: None, catalog: None, srf_index: None, sequences: None };
    let collect = if let Some(from) = statement.from {
        collect_join_matches(storage, table_index, &def, schema, from, statement.where_clause, arena, params, txn.txid, scratch)
    } else {
        collect_matches(storage, table_index, txn.txid, schema, statement.where_clause, arena, params, &hooks, scratch)
    };
    if let Err(e) = collect {
        return sql_fail(e);
    }

    if !statement.returning.is_empty() && !capturing {
        let mut columns = [ColDesc::new("", 0, 0); MAX_PROJ];
        match describe_items(statement.returning, Some(&def), &mut columns) {
            Ok(n) => responder.row_description(&columns[..n])?,
            Err(e) => return sql_fail(e),
        }
    }

    let mut updated = 0u64;
    for i in 0..scratch.len() {
        let (rowid, home) = scratch[i];
        // Build the new row image in the statement arena so the heap
        // borrow ends before the heap is appended to.
        // An arena-owned copy of the old row bytes: the referential-action
        // pass below needs the old values after storage mutates.
        let fetched = match storage.row_bytes(table_index, rowid, home, arena) {
            Ok(b) => b,
            Err(e) => return sql_fail(e),
        };
        let row_bytes = match arena.alloc_slice_copy(fetched) {
            Ok(b) => &*b,
            Err(_) => {
                return sql_fail(sql_err!(
                    sqlstate::PROGRAM_LIMIT_EXCEEDED,
                    "updated rows exceed the statement arena"
                ))
            }
        };
        let new_bytes = {
            let mut values = [Datum::Null; MAX_COLUMNS];
            if let Err(e) = rowenc::decode(row_bytes, schema, &mut values) {
                return sql_fail(e);
            }
            let mut new_values = [Datum::Null; MAX_COLUMNS];
            new_values[..def.n_columns].copy_from_slice(&values[..def.n_columns]);
            let context = RowCtx { def: &def, values: &values[..def.n_columns] };
            if let Some(from) = statement.from {
                // UPDATE ... FROM: evaluate the assignments against the target
                // row joined with the first matching FROM row.
                let mut set_err: Option<SqlError> = None;
                let r = super::query::first_from_match(
                    storage, from, txn.txid, statement.where_clause, arena, params, &context,
                    &mut |combined| {
                        for (a, (_, expression)) in statement.assignments.iter().enumerate() {
                            // A generated target's `= DEFAULT` is a no-op here; it
                            // is recomputed from the finished row below.
                            if def.columns()[targets[a]].is_generated {
                                continue;
                            }
                            let v = eval(expression, arena, params, &combined)?;
                            new_values[targets[a]] = coerce(v, &def.columns()[targets[a]], storage, arena)?;
                        }
                        Ok(())
                    },
                );
                match r {
                    Ok(_) => {}
                    Err(e) => set_err = Some(e),
                }
                if let Some(e) = set_err {
                    return sql_fail(e);
                }
            } else {
                // `nextval`/`setval` in a SET expression advance once per updated
                // row; a scoped sequence evaluator (shared `&storage`) supplies
                // them and is dropped before the row is written back mutably.
                let seq = crate::sql::sequence::SeqEval::new(storage, seq_session, txn.txid);
                let hooks =
                    super::eval::EvalHooks { sequences: Some(&seq), ..super::eval::NO_HOOKS };
                for (a, (_, expression)) in statement.assignments.iter().enumerate() {
                    if def.columns()[targets[a]].is_generated {
                        continue; // recomputed from the finished row below
                    }
                    let v = match super::eval::eval_full(expression, arena, params, &context, &hooks) {
                        Ok(v) => v,
                        Err(e) => return sql_fail(e),
                    };
                    let col = &def.columns()[targets[a]];
                    match coerce(v, col, storage, arena) {
                        Ok(v) => new_values[targets[a]] = v,
                        Err(e) => return sql_fail(e),
                    }
                }
            }
            // Every generated column is recomputed from the updated row (a change
            // to any dependency must flow through).
            if let Err(e) = compute_generated(&def, &generated_exprs, &mut new_values, storage, arena) {
                return sql_fail(e);
            }
            if let Err(e) = check_not_null(&def, &new_values) {
                return sql_fail(e);
            }
            if let Err(e) = enforce_row_constraints(
                storage,
                table_index,
                &def,
                schema,
                &new_values[..def.n_columns],
                Some(rowid),
                txn.txid,
                &checks,
                arena,
                params,
            ) {
                return sql_fail(e);
            }
            let len = rowenc::encoded_len(&new_values[..def.n_columns]);
            let out = match arena.alloc_slice_with(len, |_| 0u8) {
                Ok(o) => o,
                Err(_) => {
                    return sql_fail(sql_err!(
                        sqlstate::PROGRAM_LIMIT_EXCEEDED,
                        "updated rows exceed the statement arena"
                    ))
                }
            };
            rowenc::encode(&new_values[..def.n_columns], out);
            &*out
        };
        let (new_loc, slice) = match storage.heap.append(new_bytes.len()) {
            Ok(x) => x,
            Err(e) => return sql_fail(e),
        };
        slice.copy_from_slice(new_bytes);
        match storage.write_pending(table_index, rowid, txn.txid, txn.command_id(), Some(new_loc)) {
            Ok(prior) => {
                if let Err(e) = txn.touch(table_index as u32, rowid, prior) {
                    storage.restore_pending(table_index, rowid, txn.txid, prior);
                    return sql_fail(e);
                }
            }
            Err(e) => return sql_fail(e),
        }
        // With the new parent row in place, apply each referencing key's
        // ON UPDATE action when a referenced column changed (NO ACTION /
        // RESTRICT block; CASCADE / SET NULL / SET DEFAULT rewrite the
        // referencing rows — their own constraints re-check against the new
        // key). Both row images are arena-owned, so the cascade may mutate
        // storage.
        {
            let mut old_row = [Datum::Null; MAX_COLUMNS];
            if let Err(e) = rowenc::decode(row_bytes, schema, &mut old_row) {
                return sql_fail(e);
            }
            let mut new_row = [Datum::Null; MAX_COLUMNS];
            if let Err(e) = rowenc::decode(new_bytes, schema, &mut new_row) {
                return sql_fail(e);
            }
            if referenced_key_changed(
                storage,
                def.schema.as_str(),
                def.name.as_str(),
                &old_row[..def.n_columns],
                &new_row[..def.n_columns],
                txn.txid,
            ) && let Err(e) = apply_fk_parent_actions(
                storage,
                txn,
                def.schema.as_str(),
                def.name.as_str(),
                &old_row[..def.n_columns],
                Some(&new_row[..def.n_columns]),
                arena,
                params,
                MAX_FK_CASCADE_DEPTH,
            ) {
                return sql_fail(e);
            }
        }
        if !statement.returning.is_empty() {
            let mut new_values = [Datum::Null; MAX_COLUMNS];
            if let Err(e) = rowenc::decode(storage.heap.get(new_loc), schema, &mut new_values) {
                return sql_fail(e);
            }
            if let Err(e) = emit_projected(
                &def,
                &new_values[..def.n_columns],
                statement.returning,
                arena,
                params,
                responder,
                &mut capture,
            )? {
                return sql_fail(e);
            }
        }
        updated += 1;
    }
    let tag = stack_format!(48, "UPDATE {}", updated);
    if !capturing { responder.command_complete(tag.as_str())?; }
    sql_ok()
}

#[allow(clippy::too_many_arguments, clippy::type_complexity)]
pub fn delete(
    storage: &mut Storage,
    txn: &mut TxnState,
    scratch: &mut FixedVec<(u64, RowHome)>,
    statement: &Delete,
    arena: &Arena,
    params: &[Datum],
    responder: &mut Responder,
    mut capture: Option<&mut dyn FnMut(&[Datum]) -> Result<(), SqlError>>,
) -> Outcome {
    let capturing = capture.is_some();
    let table_index = match resolve_dml_table(storage, &statement.table, txn.txid) {
        Ok(i) => i,
        Err(e) => return sql_fail(e),
    };
    let def = storage.table(table_index).def;
    let mut schema = [ColType::Bool; MAX_COLUMNS];
    def.schema(&mut schema);
    let schema = &schema[..def.n_columns];

    let subs = match super::query::subquery_hooks(
        &[statement.where_clause],
        storage,
        txn.txid,
        arena,
        params,
    ) {
        Ok(s) => s,
        Err(e) => return sql_fail(e),
    };
    let hooks = super::eval::EvalHooks { group: None, aggs: None, subs: Some(&subs) , windows: None, catalog: None, srf_index: None, sequences: None };
    let collect = if let Some(using) = statement.using {
        collect_join_matches(storage, table_index, &def, schema, using, statement.where_clause, arena, params, txn.txid, scratch)
    } else {
        collect_matches(storage, table_index, txn.txid, schema, statement.where_clause, arena, params, &hooks, scratch)
    };
    if let Err(e) = collect {
        return sql_fail(e);
    }
    if !statement.returning.is_empty() && !capturing {
        let mut columns = [ColDesc::new("", 0, 0); MAX_PROJ];
        match describe_items(statement.returning, Some(&def), &mut columns) {
            Ok(n) => responder.row_description(&columns[..n])?,
            Err(e) => return sql_fail(e),
        }
    }
    let referenced = table_is_referenced(storage, def.schema.as_str(), def.name.as_str(), txn.txid);
    for i in 0..scratch.len() {
        let (rowid, old_home) = scratch[i];
        if !statement.returning.is_empty() || referenced {
            // The cascade below mutates storage, so the row image is decoded
            // from an arena-owned copy.
            let fetched = match storage.row_bytes(table_index, rowid, old_home, arena) {
                Ok(b) => b,
                Err(e) => return sql_fail(e),
            };
            let old_copy = match arena.alloc_slice_copy(fetched) {
                Ok(c) => c,
                Err(_) => {
                    return sql_fail(sql_err!(
                        sqlstate::PROGRAM_LIMIT_EXCEEDED,
                        "deleted rows exceed the statement arena"
                    ))
                }
            };
            let mut old_values = [Datum::Null; MAX_COLUMNS];
            if let Err(e) = rowenc::decode(old_copy, schema, &mut old_values) {
                return sql_fail(e);
            }
            // Apply each referencing key's ON DELETE action (NO ACTION /
            // RESTRICT block; CASCADE / SET NULL / SET DEFAULT rewrite the
            // referencing rows).
            if referenced
                && let Err(e) = apply_fk_parent_actions(
                    storage,
                    txn,
                    def.schema.as_str(),
                    def.name.as_str(),
                    &old_values[..def.n_columns],
                    None,
                    arena,
                    params,
                    MAX_FK_CASCADE_DEPTH,
                )
            {
                return sql_fail(e);
            }
            if !statement.returning.is_empty()
                && let Err(e) = emit_projected(
                    &def,
                    &old_values[..def.n_columns],
                    statement.returning,
                    arena,
                    params,
                    responder,
                    &mut capture,
                )?
            {
                return sql_fail(e);
            }
        }
        match storage.write_pending(table_index, rowid, txn.txid, txn.command_id(), None) {
            Ok(prior) => {
                if let Err(e) = txn.touch(table_index as u32, rowid, prior) {
                    storage.restore_pending(table_index, rowid, txn.txid, prior);
                    return sql_fail(e);
                }
            }
            Err(e) => return sql_fail(e),
        }
    }
    let tag = stack_format!(48, "DELETE {}", scratch.len());
    if !capturing {
        responder.command_complete(tag.as_str())?;
    }
    sql_ok()
}

/// TRUNCATE: removes every visible row of the listed tables through the
/// transactional delete machinery (so a rolled-back TRUNCATE restores them),
/// with PostgreSQL's structural foreign-key rule — a table referenced by a
/// table outside the list cannot be truncated; CASCADE pulls the referencing
/// tables in transitively, with a NOTICE per addition. RESTART IDENTITY
/// resets each serial column's sequence, transactionally (an undo entry
/// restores the prior position on rollback).
pub fn truncate(
    storage: &mut Storage,
    txn: &mut TxnState,
    tables: &[QualName],
    restart_identity: bool,
    cascade: bool,
    responder: &mut Responder,
) -> Outcome {
    // Resolve the listed tables (views are not truncatable).
    let mut list: [usize; MAX_TRUNCATE_TABLES] = [0; MAX_TRUNCATE_TABLES];
    let mut n = 0usize;
    for name in tables {
        let index = match storage.resolve_relation(name.schema, name.name, txn.txid) {
            Some(crate::storage::ResolvedRelation::View(_)) => {
                return sql_fail(sql_err!(
                    crate::sql::eval::sqlstate::WRONG_OBJECT_TYPE,
                    "\"{}\" is not a table",
                    name.name
                ));
            }
            Some(crate::storage::ResolvedRelation::Table(index)) => index,
            _ => return sql_fail(undefined_qual(name)),
        };
        if !list[..n].contains(&index) {
            if n == MAX_TRUNCATE_TABLES {
                return sql_fail(sql_err!(
                    sqlstate::PROGRAM_LIMIT_EXCEEDED,
                    "too many tables in TRUNCATE"
                ));
            }
            list[n] = index;
            n += 1;
        }
    }
    // Foreign-key closure: a table outside the list referencing a listed one
    // blocks the truncate — or joins it under CASCADE.
    loop {
        let mut grew = false;
        for other in 0..storage.table_count() {
            if !storage.table(other).live || list[..n].contains(&other) {
                continue;
            }
            let refs_listed = storage.table(other).def.fkeys().iter().any(|fk| {
                list[..n].iter().any(|&t| {
                    let tdef = &storage.table(t).def;
                    tdef.schema.as_str() == fk.parent_schema.as_str()
                        && tdef.name.as_str() == fk.parent.as_str()
                })
            });
            if !refs_listed {
                continue;
            }
            if !cascade {
                return sql_fail(sql_err!(
                    sqlstate::FEATURE_NOT_SUPPORTED,
                    "cannot truncate a table referenced in a foreign key constraint"
                ));
            }
            if n == MAX_TRUNCATE_TABLES {
                return sql_fail(sql_err!(
                    sqlstate::PROGRAM_LIMIT_EXCEEDED,
                    "too many tables in TRUNCATE"
                ));
            }
            let name = storage.table(other).def.name;
            responder.notice(
                crate::sql::eval::sqlstate::SUCCESSFUL_COMPLETION,
                stack_format!(160, "truncate cascades to table \"{}\"", name.as_str()).as_str(),
            )?;
            list[n] = other;
            n += 1;
            grew = true;
        }
        if !grew {
            break;
        }
    }
    // Remove every visible row, transactionally.
    for &table_index in &list[..n] {
        let mut rowids: [u64; 4096] = [0; 4096];
        loop {
            let mut count = 0usize;
            let _ = storage.for_each_row_state(table_index, &mut |rowid, state| {
                use core::ops::ControlFlow;
                if state.visible_at(txn.txid, storage.read_snapshot()).is_none() {
                    return Ok(ControlFlow::Continue(()));
                }
                if count == rowids.len() {
                    return Ok(ControlFlow::Break(()));
                }
                rowids[count] = rowid;
                count += 1;
                Ok(ControlFlow::Continue(()))
            });
            if count == 0 {
                break;
            }
            for &rowid in &rowids[..count] {
                match storage.write_pending(table_index, rowid, txn.txid, txn.command_id(), None) {
                    Ok(prior) => {
                        if let Err(e) = txn.touch(table_index as u32, rowid, prior) {
                            storage.restore_pending(table_index, rowid, txn.txid, prior);
                            return sql_fail(e);
                        }
                    }
                    Err(e) => return sql_fail(e),
                }
            }
        }
        if restart_identity {
            let def = storage.table(table_index).def;
            for c in 0..def.n_columns {
                if !def.columns()[c].auto_increment {
                    continue;
                }
                let prior = storage.table(table_index).serial_last[c];
                if let Err(e) = txn.record_ddl(crate::sql::txn::DdlUndo::SequenceReset {
                    table: table_index as u32,
                    column: c as u16,
                    prior,
                }) {
                    return sql_fail(e);
                }
                let t = storage.table_mut(table_index);
                t.serial_last[c] = 0;
                t.serial_dirty = true;
            }
        }
    }
    responder.command_complete("TRUNCATE TABLE")?;
    sql_ok()
}

/// The most tables one TRUNCATE can name, its CASCADE closure included.
const MAX_TRUNCATE_TABLES: usize = 16;

/// ALTER TABLE, autocommit-only: rewrites are journaled as DROP, CREATE,
/// full re-UPSERT within one WAL batch, so replay reproduces the new
/// shape atomically. Two-phase in memory: all new row images are prepared
/// first, then the definition and row map swap; a failure part-way leaves
/// the table untouched (only heap bytes leak until compaction).
/// Whether `ALTER COLUMN ... TYPE` may cast `from` to `to` without a `USING`
/// clause — i.e. PostgreSQL has an assignment (or implicit) cast for the pair.
/// Mirrors `pg_cast` (castcontext in {'a','i'}) over this engine's types, plus
/// the I/O rule: any type casts *to* a string type as an assignment, but *from*
/// a string type is explicit-only (needs USING).
fn alter_type_auto_castable(from: ColType, to: ColType) -> bool {
    use ColType::*;
    if from == to {
        return true;
    }
    let is_string = |t| matches!(t, Text | Varchar | Bpchar | Name);
    // to-string: assignment via the output function; from-string: explicit.
    if is_string(to) {
        return true;
    }
    if is_string(from) {
        return false;
    }
    let numeric = |t| matches!(t, Int2 | Int4 | Int8 | Float4 | Float8 | Numeric);
    if numeric(from) && numeric(to) {
        return true;
    }
    // The remaining assignment/implicit pairs among the date/time and json
    // families, per pg_cast.
    matches!(
        (from, to),
        (Date, Timestamp | Timestamptz)
            | (Timestamp, Date | Time | Timestamptz)
            | (Timestamptz, Date | Time | Timestamp | Timetz)
            | (Time, Timetz | Interval)
            | (Timetz, Time)
            | (Interval, Time)
            | (Json, Jsonb)
            | (Jsonb, Json)
    )
}

/// Validates every committed row against a table's whole constraint set, for
/// ALTER TABLE ADD CONSTRAINT — the just-added constraint is the only one that
/// can fail, and its violation surfaces with the same SQLSTATE the INSERT path
/// would give (23514 CHECK, 23505 UNIQUE, 23503 FK, 23502 the PK's NOT NULL).
fn validate_all_rows(
    storage: &Storage,
    table_index: usize,
    new_def: &TableDef,
    arena: &Arena,
) -> Result<(), SqlError> {
    let checks = parse_checks(new_def, arena)?;
    let mut schema = [ColType::Bool; MAX_COLUMNS];
    new_def.schema(&mut schema);
    let schema = &schema[..new_def.n_columns];
    let mut result = Ok(());
    storage.for_each_row_state(table_index, &mut |rowid, state| {
        use core::ops::ControlFlow;
        let Some(home) = state.committed else {
            return Ok(ControlFlow::Continue(()));
        };
        let bytes = storage.row_bytes(table_index, rowid, home, arena)?;
        let mut values = [Datum::Null; MAX_COLUMNS];
        rowenc::decode(bytes, schema, &mut values)?;
        let values = &values[..new_def.n_columns];
        let check = check_not_null(new_def, values).and_then(|()| {
            enforce_row_constraints(
                storage, table_index, new_def, schema, values, Some(rowid), u32::MAX, &checks, arena, &[],
            )
        });
        if let Err(e) = check {
            result = Err(e);
            return Ok(ControlFlow::Break(()));
        }
        Ok(ControlFlow::Continue(()))
    })?;
    result
}

/// Removes a named constraint from `def` for ALTER TABLE DROP CONSTRAINT:
/// a CHECK, a table-level UNIQUE/PRIMARY KEY, or an FK by its stored name, plus
/// the generated names of a single-column primary key (`<table>_pkey`) or
/// unique column (`<table>_<column>_key`). Returns whether one was found.
fn drop_named_constraint(def: &mut TableDef, name: &str) -> bool {
    for i in 0..def.n_checks {
        if def.checks[i].name.as_str() == name {
            for j in i..def.n_checks - 1 {
                def.checks[j] = def.checks[j + 1];
            }
            def.n_checks -= 1;
            return true;
        }
    }
    for i in 0..def.n_uniques {
        if def.uniques[i].name.as_str() == name {
            for j in i..def.n_uniques - 1 {
                def.uniques[j] = def.uniques[j + 1];
            }
            def.n_uniques -= 1;
            return true;
        }
    }
    for i in 0..def.n_fkeys {
        if def.fkeys[i].name.as_str() == name {
            for j in i..def.n_fkeys - 1 {
                def.fkeys[j] = def.fkeys[j + 1];
            }
            def.n_fkeys -= 1;
            return true;
        }
    }
    // A single-column primary key is a column flag named "<table>_pkey".
    let pkey = crate::stack_format!(96, "{}_pkey", def.name.as_str());
    if name == pkey.as_str()
        && let Some(c) = def.columns[..def.n_columns].iter_mut().find(|c| c.primary)
    {
        c.primary = false;
        c.unique = false;
        return true;
    }
    // A single-column UNIQUE is "<table>_<column>_key".
    for i in 0..def.n_columns {
        if def.columns[i].unique && !def.columns[i].primary {
            let key = crate::stack_format!(128, "{}_{}_key", def.name.as_str(), def.columns[i].name.as_str());
            if name == key.as_str() {
                def.columns[i].unique = false;
                return true;
            }
        }
    }
    false
}

/// If two rewritten rows `a` and `b` collide on a uniqueness constraint of
/// `new_def` — a single-column UNIQUE/PRIMARY KEY flag or a multi-column key,
/// with every key column non-NULL and equal — returns the constraint's
/// PostgreSQL name. Used to validate a uniqueness constraint added alongside an
/// ALTER row rewrite, against the transformed images, before anything is
/// journaled (a NULL in any key column makes the rows distinct).
fn rewritten_dup_name(new_def: &TableDef, a: &[Datum], b: &[Datum]) -> Option<StackStr<128>> {
    use core::fmt::Write as _;
    let eq = |i: usize| {
        !a[i].is_null()
            && !b[i].is_null()
            && compare_datums(&a[i], &b[i]).map(|o| o.is_eq()).unwrap_or(false)
    };
    for (i, c) in new_def.columns().iter().enumerate() {
        if c.unique && eq(i) {
            let mut name = StackStr::<128>::new();
            if c.primary {
                let _ = write!(name, "{}_pkey", new_def.name.as_str());
            } else {
                let _ = write!(name, "{}_{}_key", new_def.name.as_str(), c.name.as_str());
            }
            return Some(name);
        }
    }
    for uk in new_def.uniques() {
        if uk.columns().iter().all(|&col| eq(col as usize)) {
            let mut name = StackStr::<128>::new();
            let _ = write!(name, "{}", uk.name.as_str());
            return Some(name);
        }
    }
    None
}

/// How each column of a rewritten row is produced from the old row, composed
/// across every subcommand of one ALTER TABLE so a single rewrite pass applies
/// all of them.
#[derive(Clone, Copy)]
enum ColSource<'a> {
    /// Copy the old row's column at this original index unchanged.
    Keep(usize),
    /// Cast the old row's column (or a USING expression over the old row) to a
    /// new type; `orig` is the source column's original index.
    Cast { orig: usize, target: ColType, type_mod: i32, using: Option<&'a Expr<'a>> },
    /// A column added by this statement; its value is the new column's default
    /// (or NULL). The index is into the *new* definition.
    FillDefault(usize),
}

#[allow(clippy::too_many_arguments)]
pub fn alter_table(
    storage: &mut Storage,
    wal: &mut Wal,
    scratch: &mut FixedVec<(u64, RowHome)>,
    statement: &AlterTable,
    arena: &Arena,
    seq_session: &crate::sql::guc::SeqSession,
    responder: &mut Responder,
) -> Outcome {
    // ALTER runs autocommitted, so resolution sees only the committed
    // catalog (no transaction owns pending DDL here).
    let table_index = match storage.resolve_relation(
        statement.table.schema,
        statement.table.name,
        u32::MAX,
    ) {
        Some(crate::storage::ResolvedRelation::Table(i)) => i,
        _ => return sql_fail(undefined_qual(&statement.table)),
    };
    let def = storage.table(table_index).def;

    // Any in-flight change on this table blocks ALTER (fail fast).
    if storage
        .table(table_index)
        .rows
        .iter()
        .any(|(_, state)| state.pending.is_some())
    {
        return sql_fail(sql_err!(
            crate::sql::eval::sqlstate::LOCK_NOT_AVAILABLE,
            "table \"{}\" has uncommitted changes; retry when idle",
            statement.table.name
        ));
    }

    // SET SCHEMA is a definition-only move with its own journal record — no
    // row images change, and inbound foreign keys follow the table. It is a
    // standalone form (never combined), so it is the whole action list.
    if let [AlterAction::SetSchema(new_schema)] = statement.actions {
        let new_schema = *new_schema;
        let Some(_) = storage.find_schema(new_schema) else {
            return sql_fail(sql_err!(
                sqlstate::INVALID_SCHEMA_NAME,
                "schema \"{}\" does not exist",
                new_schema
            ));
        };
        if new_schema == def.schema.as_str() {
            // Already there: PostgreSQL treats this as a no-op success.
            responder.command_complete("ALTER TABLE")?;
            return sql_ok();
        }
        if storage.find_table(new_schema, def.name.as_str()).is_some() {
            return sql_fail(sql_err!(
                sqlstate::DUPLICATE_TABLE,
                "relation \"{}\" already exists in schema \"{}\"",
                def.name.as_str(),
                new_schema
            ));
        }
        let new_name = match SqlName::parse(new_schema) {
            Ok(n) => n,
            Err(e) => return sql_fail(e),
        };
        let lsn = storage.bump_lsn();
        if let Err(e) = wal.append(
            lsn,
            &WalOp::SetTableSchema {
                schema: def.schema.as_str(),
                name: def.name.as_str(),
                new_schema,
            },
        ) {
            return sql_fail(e);
        }
        storage.move_table_schema(table_index, new_name);
        responder.command_complete("ALTER TABLE")?;
        return sql_ok();
    }

    // Collect every committed row up front: the row count decides whether an
    // added NOT NULL column needs a fill (a spilled table has rows even when
    // the overlay map has evicted them, so `rows.is_empty()` cannot answer
    // this), and the same list drives the rewrite below.
    scratch.clear();
    {
        let mut overflow = false;
        let _ = storage.for_each_row_state(table_index, &mut |rowid, state| {
            use core::ops::ControlFlow;
            let Some(loc) = state.committed else {
                return Ok(ControlFlow::Continue(()));
            };
            if scratch.push((rowid, loc)).is_err() {
                overflow = true;
                return Ok(ControlFlow::Break(()));
            }
            Ok(ControlFlow::Continue(()))
        });
        if overflow {
            return sql_fail(sql_err!(
                sqlstate::PROGRAM_LIMIT_EXCEEDED,
                "ALTER touches more than {} rows",
                scratch.capacity()
            ));
        }
    }
    let has_rows = !scratch.is_empty();

    // Build the new definition and the composed per-column rewrite source.
    // Names resolve against the running definition, so an ADD CONSTRAINT can
    // reference a column ADDed earlier in the pass-ordered list.
    let mut new_def = def;
    let mut source = [ColSource::Keep(0usize); MAX_COLUMNS];
    for (i, s) in source.iter_mut().enumerate().take(def.n_columns) {
        *s = ColSource::Keep(i);
    }
    let mut added_any = false;
    let mut dropped_any = false;
    let mut retyped_any = false;
    let mut has_added_unique = false;
    // (column, start) for ADD IDENTITY with a START WITH, applied after the new
    // definition is installed.
    let mut identity_seeds = [(0usize, 0i64); MAX_COLUMNS];
    let mut n_identity_seeds = 0usize;

    for action in statement.actions {
        match action {
            AlterAction::SetSchema(_) => unreachable!("SET SCHEMA is a standalone action"),
            AlterAction::RenameTable(new_name) => {
                if storage.find_table(def.schema.as_str(), new_name).is_some() {
                    return sql_fail(sql_err!(
                        sqlstate::DUPLICATE_TABLE,
                        "relation \"{}\" already exists",
                        new_name
                    ));
                }
                new_def.name = match SqlName::parse(new_name) {
                    Ok(n) => n,
                    Err(e) => return sql_fail(e),
                };
            }
            AlterAction::RenameColumn { from, to } => {
                let Some(i) = new_def.column_index(from) else {
                    return sql_fail(undefined_column(from));
                };
                if new_def.column_index(to).is_some() {
                    return sql_fail(sql_err!(sqlstate::DUPLICATE_COLUMN, "column \"{}\" already exists", to));
                }
                new_def.columns[i].name = match SqlName::parse(to) {
                    Ok(n) => n,
                    Err(e) => return sql_fail(e),
                };
            }
            AlterAction::AddColumn(c) => {
                if new_def.column_index(c.name).is_some() {
                    return sql_fail(sql_err!(sqlstate::DUPLICATE_COLUMN, "column \"{}\" already exists", c.name));
                }
                if new_def.n_columns == MAX_COLUMNS {
                    return sql_fail(sql_err!(
                        sqlstate::PROGRAM_LIMIT_EXCEEDED,
                        "tables can have at most {} columns",
                        MAX_COLUMNS
                    ));
                }
                // ALTER runs autocommitted, so a domain type resolves against
                // the committed catalog (txid 0).
                let meta = match build_column(c, &*storage, 0, arena) {
                    Ok(m) => m,
                    Err(e) => return sql_fail(e),
                };
                // NOT NULL without a default over a non-empty table is a
                // constraint violation, as in PostgreSQL.
                if meta.default_value.is_none() && meta.not_null && has_rows {
                    return sql_fail(sql_err!(
                        sqlstate::NOT_NULL_VIOLATION,
                        "column \"{}\" of relation \"{}\" contains null values",
                        c.name,
                        statement.table.name
                    ));
                }
                let index = new_def.n_columns;
                new_def.columns[index] = meta;
                new_def.n_columns += 1;
                source[index] = ColSource::FillDefault(index);
                added_any = true;
            }
            AlterAction::DropColumn(name) => {
                let Some(i) = new_def.column_index(name) else {
                    return sql_fail(undefined_column(name));
                };
                if new_def.n_columns == 1 {
                    return sql_fail(sql_err!(
                        sqlstate::FEATURE_NOT_SUPPORTED,
                        "cannot drop the only column of a table"
                    ));
                }
                for j in i..new_def.n_columns - 1 {
                    new_def.columns[j] = new_def.columns[j + 1];
                    source[j] = source[j + 1];
                }
                new_def.n_columns -= 1;
                dropped_any = true;
            }
            AlterAction::SetDefault { column, value, value_text } => {
                let Some(i) = new_def.column_index(column) else {
                    return sql_fail(undefined_column(column));
                };
                let ctype = new_def.columns[i].ctype;
                let type_mod = new_def.columns[i].type_mod;
                // A literal-only default folds to a constant; a call-bearing one
                // is stored as text and evaluated per row — CREATE TABLE's path.
                let (default_value, default_expr) =
                    match ddl::resolve_default(Some(value), Some(value_text), ctype, type_mod, arena) {
                        Ok(d) => d,
                        Err(e) => return sql_fail(e),
                    };
                new_def.columns[i].default_value = default_value;
                new_def.columns[i].default_expr = default_expr;
            }
            AlterAction::DropDefault { column } => {
                let Some(i) = new_def.column_index(column) else {
                    return sql_fail(undefined_column(column));
                };
                new_def.columns[i].default_value = None;
                new_def.columns[i].default_expr = None;
                // Dropping a serial column's default detaches its auto-increment.
                new_def.columns[i].auto_increment = false;
            }
            AlterAction::AddIdentity { column, spec } => {
                let Some(i) = new_def.column_index(column) else {
                    return sql_fail(undefined_column(column));
                };
                let col = &new_def.columns[i];
                if col.is_identity {
                    return sql_fail(sql_err!(
                        sqlstate::OBJECT_NOT_IN_PREREQUISITE_STATE,
                        "column \"{}\" of relation \"{}\" is already an identity column",
                        column,
                        statement.table.name
                    ));
                }
                if !col.not_null {
                    return sql_fail(sql_err!(
                        sqlstate::OBJECT_NOT_IN_PREREQUISITE_STATE,
                        "column \"{}\" of relation \"{}\" must be declared NOT NULL before identity can be added",
                        column,
                        statement.table.name
                    ));
                }
                let step = spec.increment.unwrap_or(1);
                new_def.columns[i].is_identity = true;
                new_def.columns[i].identity_always = spec.always;
                new_def.columns[i].auto_increment = true;
                new_def.columns[i].auto_increment_step = step;
                if let Some(start) = spec.start {
                    identity_seeds[n_identity_seeds] = (i, start.wrapping_sub(step));
                    n_identity_seeds += 1;
                }
            }
            AlterAction::DropIdentity { column, if_exists } => {
                let Some(i) = new_def.column_index(column) else {
                    return sql_fail(undefined_column(column));
                };
                if !new_def.columns[i].is_identity {
                    if *if_exists {
                        responder.notice(
                            crate::sql::eval::sqlstate::SUCCESSFUL_COMPLETION,
                            stack_format!(
                                160,
                                "column \"{}\" of relation \"{}\" is not an identity column, skipping",
                                column,
                                statement.table.name
                            )
                            .as_str(),
                        )?;
                        continue;
                    }
                    return sql_fail(sql_err!(
                        sqlstate::OBJECT_NOT_IN_PREREQUISITE_STATE,
                        "column \"{}\" of relation \"{}\" is not an identity column",
                        column,
                        statement.table.name
                    ));
                }
                new_def.columns[i].is_identity = false;
                new_def.columns[i].identity_always = false;
                new_def.columns[i].auto_increment = false;
                new_def.columns[i].auto_increment_step = 1;
            }
            AlterAction::SetNotNull { column } => {
                let Some(i) = new_def.column_index(column) else {
                    return sql_fail(undefined_column(column));
                };
                // A NULL is caught by the content validation below, against the
                // rewritten image so it composes with a type change in the same
                // statement.
                new_def.columns[i].not_null = true;
            }
            AlterAction::DropNotNull { column } => {
                let Some(i) = new_def.column_index(column) else {
                    return sql_fail(undefined_column(column));
                };
                // A primary key implies NOT NULL, whether it rides a column flag
                // or an explicitly named single-/multi-column key.
                let in_primary_key = new_def.columns[i].primary
                    || new_def.uniques[..new_def.n_uniques]
                        .iter()
                        .any(|k| k.is_primary && k.columns().contains(&(i as u16)));
                if in_primary_key {
                    return sql_fail(sql_err!(
                        sqlstate::INVALID_TABLE_DEFINITION,
                        "column \"{}\" is in a primary key",
                        column
                    ));
                }
                new_def.columns[i].not_null = false;
            }
            AlterAction::AlterColumnType { column, type_name, type_mod, using } => {
                let Some(i) = new_def.column_index(column) else {
                    return sql_fail(undefined_column(column));
                };
                let Some(target) = ColType::from_sql_name(type_name) else {
                    return sql_fail(sql_err!(
                        sqlstate::UNDEFINED_OBJECT,
                        "type \"{}\" does not exist",
                        type_name
                    ));
                };
                // Without USING, the stored value casts through the assignment
                // cast; a cast that is explicit-only (e.g. text→int) is refused
                // with PostgreSQL's 42804, telling the user to add USING.
                if using.is_none() && !alter_type_auto_castable(new_def.columns[i].ctype, target) {
                    return sql_fail(sql_err!(
                        sqlstate::DATATYPE_MISMATCH,
                        "column \"{}\" cannot be cast automatically to type {}",
                        column,
                        target.name()
                    ));
                }
                // Compose with whatever already produces this column: a kept or
                // cast original column becomes a (re)cast of its original index;
                // a column added in this same statement re-casts its default.
                match source[i] {
                    ColSource::Keep(orig) | ColSource::Cast { orig, .. } => {
                        source[i] = ColSource::Cast { orig, target, type_mod: *type_mod, using: *using };
                        retyped_any = true;
                    }
                    ColSource::FillDefault(fi) => {
                        if let Some(od) = new_def.columns[fi].default_value.as_ref() {
                            match cast_to(od.as_datum(), target, arena)
                                .and_then(|v| apply_typmod(v, target, *type_mod, arena))
                                .and_then(|v| crate::storage::OwnedDatum::from_datum(&v))
                            {
                                Ok(od) => new_def.columns[fi].default_value = Some(od),
                                Err(e) => return sql_fail(e),
                            }
                        }
                    }
                }
                new_def.columns[i].ctype = target;
                new_def.columns[i].type_mod = *type_mod;
            }
            AlterAction::AddConstraint(constraint) => {
                // Build the constraint into the new definition (u32::MAX sees
                // all committed catalog, e.g. an FK's parent). CHECK/NOT NULL/FK
                // are validated per rewritten image below; an added uniqueness
                // constraint is validated across the rewritten images before
                // anything is journaled.
                if let Err(e) = crate::sql::exec::ddl::attach_constraints(
                    storage,
                    &mut new_def,
                    core::slice::from_ref(constraint),
                    u32::MAX,
                    arena,
                ) {
                    return sql_fail(e);
                }
                if matches!(
                    constraint,
                    crate::sql::ast::TableConstraint::PrimaryKey { .. }
                        | crate::sql::ast::TableConstraint::Unique { .. }
                ) {
                    has_added_unique = true;
                }
            }
            AlterAction::DropConstraint { name, if_exists } => {
                if !drop_named_constraint(&mut new_def, name) {
                    if !*if_exists {
                        return sql_fail(sql_err!(
                            sqlstate::UNDEFINED_OBJECT,
                            "constraint \"{}\" of relation \"{}\" does not exist",
                            name,
                            def.name.as_str()
                        ));
                    }
                    // IF EXISTS: PostgreSQL emits a skip notice (SQLSTATE 00000).
                    responder.notice(
                        crate::sql::eval::sqlstate::SUCCESSFUL_COMPLETION,
                        stack_format!(
                            160,
                            "constraint \"{}\" of relation \"{}\" does not exist, skipping",
                            name,
                            def.name.as_str()
                        )
                        .as_str(),
                    )?;
                }
            }
            AlterAction::RenameConstraint { from, to } => {
                // The new name must be free among this table's constraints.
                let taken = new_def.checks[..new_def.n_checks].iter().any(|c| c.name.as_str() == *to)
                    || new_def.uniques[..new_def.n_uniques].iter().any(|k| k.name.as_str() == *to)
                    || new_def.fkeys[..new_def.n_fkeys].iter().any(|f| f.name.as_str() == *to);
                if taken {
                    return sql_fail(sql_err!(
                        sqlstate::DUPLICATE_OBJECT,
                        "constraint \"{}\" for relation \"{}\" already exists",
                        to,
                        def.name.as_str()
                    ));
                }
                let new_name = match SqlName::parse(to) {
                    Ok(n) => n,
                    Err(e) => return sql_fail(e),
                };
                let renamed = new_def.checks[..new_def.n_checks]
                    .iter_mut()
                    .find(|c| c.name.as_str() == *from)
                    .map(|c| c.name = new_name)
                    .or_else(|| {
                        new_def.uniques[..new_def.n_uniques]
                            .iter_mut()
                            .find(|k| k.name.as_str() == *from)
                            .map(|k| k.name = new_name)
                    })
                    .or_else(|| {
                        new_def.fkeys[..new_def.n_fkeys]
                            .iter_mut()
                            .find(|f| f.name.as_str() == *from)
                            .map(|f| f.name = new_name)
                    })
                    .is_some();
                // A single-column key on a column flag has a synthesized name;
                // renaming it materializes the flag into a named key.
                if !renamed {
                    match rename_flag_key(&mut new_def, from, new_name) {
                        Ok(true) => {}
                        Ok(false) => {
                            return sql_fail(sql_err!(
                                sqlstate::UNDEFINED_OBJECT,
                                "constraint \"{}\" for table \"{}\" does not exist",
                                from,
                                def.name.as_str()
                            ))
                        }
                        Err(e) => return sql_fail(e),
                    }
                }
            }
        }
    }

    let has_rewrite = added_any || dropped_any || retyped_any;

    let mut old_schema = [ColType::Bool; MAX_COLUMNS];
    def.schema(&mut old_schema);
    let old_schema = &old_schema[..def.n_columns];

    let mut new_schema = [ColType::Bool; MAX_COLUMNS];
    new_def.schema(&mut new_schema);
    let new_schema = &new_schema[..new_def.n_columns];

    // When no row image changes (rename / constraint / default / drop-not-null /
    // set-not-null over an unchanged column), the new definition — including its
    // uniqueness — is validated against the stored rows, before anything is
    // journaled. A rewrite validates each transformed image instead, below.
    if !has_rewrite
        && let Err(e) = validate_all_rows(storage, table_index, &new_def, arena)
    {
        return sql_fail(e);
    }

    // Phase 1: build every rewritten image and validate its content
    // (NOT NULL / CHECK / foreign keys), all before any journaling, so a
    // failure — a cast error included — leaves the table untouched.
    let checks = match parse_checks(&new_def, arena) {
        Ok(c) => c,
        Err(e) => return sql_fail(e),
    };
    // Non-constant DEFAULTs of columns added by this ALTER: each existing row is
    // backfilled by evaluating the default, so a `nextval` default consumes a
    // value per existing row, exactly as PostgreSQL rewrites the table.
    let new_defaults = match parse_defaults(&new_def, arena) {
        Ok(d) => d,
        Err(e) => return sql_fail(e),
    };
    // Generated columns are recomputed for every rewritten row — an ADD COLUMN
    // of a generated column backfills, and a type change of a dependency reflows.
    let new_generated = match parse_generated(&new_def, arena) {
        Ok(g) => g,
        Err(e) => return sql_fail(e),
    };
    for i in 0..scratch.len() {
        let (rowid, old_home) = scratch[i];
        let new_loc = if has_rewrite {
            // Build the new image in the statement arena so the heap borrow
            // (decoded text refs) ends before the heap append.
            let new_bytes: &[u8] = {
                let old_bytes = match storage.row_bytes(table_index, rowid, old_home, arena) {
                    Ok(b) => b,
                    Err(e) => return sql_fail(e),
                };
                let mut old_values = [Datum::Null; MAX_COLUMNS];
                if let Err(e) = rowenc::decode(old_bytes, old_schema, &mut old_values) {
                    return sql_fail(e);
                }
                let mut out = [Datum::Null; MAX_COLUMNS];
                for c in 0..new_def.n_columns {
                    out[c] = match source[c] {
                        ColSource::Keep(orig) => old_values[orig],
                        ColSource::Cast { orig, target, type_mod, using } => {
                            // USING is evaluated with the old row's columns in
                            // scope; otherwise the old value is the cast source.
                            let cast_source = match using {
                                Some(expr) => {
                                    let ctx = RowCtx { def: &def, values: &old_values[..def.n_columns] };
                                    match eval(expr, arena, crate::sql::eval::NO_PARAMS, &ctx) {
                                        Ok(v) => v,
                                        Err(e) => return sql_fail(e),
                                    }
                                }
                                None => old_values[orig],
                            };
                            match cast_to(cast_source, target, arena)
                                .and_then(|v| apply_typmod(v, target, type_mod, arena))
                            {
                                Ok(v) => v,
                                Err(e) => return sql_fail(e),
                            }
                        }
                        ColSource::FillDefault(fi) => {
                            if let Some(d) = new_def.columns[fi].default_value.as_ref() {
                                d.as_datum()
                            } else if let Some(expr) = new_defaults[fi] {
                                // Evaluate the non-constant default for this row
                                // (advancing a `nextval` default once per row),
                                // scoped so the borrow ends before the append.
                                let seq = crate::sql::sequence::SeqEval::new(
                                    storage,
                                    seq_session,
                                    u32::MAX,
                                );
                                let hooks = super::eval::EvalHooks {
                                    sequences: Some(&seq),
                                    ..super::eval::NO_HOOKS
                                };
                                let v = match super::eval::eval_full(
                                    expr,
                                    arena,
                                    crate::sql::eval::NO_PARAMS,
                                    &NoColumns,
                                    &hooks,
                                ) {
                                    Ok(v) => v,
                                    Err(e) => return sql_fail(e),
                                };
                                match coerce(v, &new_def.columns[fi], storage, arena) {
                                    Ok(v) => v,
                                    Err(e) => return sql_fail(e),
                                }
                            } else {
                                Datum::Null
                            }
                        }
                    };
                }
                if let Err(e) = compute_generated(&new_def, &new_generated, &mut out, storage, arena) {
                    return sql_fail(e);
                }
                let values = &out[..new_def.n_columns];
                if let Err(e) = crate::sql::exec::constraints::check_row_content(
                    storage, &new_def, values, &checks, arena, &[], u32::MAX,
                ) {
                    return sql_fail(e);
                }
                let len = rowenc::encoded_len(values);
                let buffer = match arena.alloc_slice_with(len, |_| 0u8) {
                    Ok(b) => b,
                    Err(_) => {
                        return sql_fail(sql_err!(
                            sqlstate::PROGRAM_LIMIT_EXCEEDED,
                            "ALTER rewrite exceeds the statement arena"
                        ))
                    }
                };
                rowenc::encode(values, buffer);
                &*buffer
            };
            let (loc, slice) = match storage.heap.append(new_bytes.len()) {
                Ok(x) => x,
                Err(e) => return sql_fail(e),
            };
            slice.copy_from_slice(new_bytes);
            loc
        } else {
            match old_home {
                RowHome::Heap(loc) => loc,
                RowHome::Spilled { .. } => {
                    // The ALTER journals a full re-upsert of every row, so a
                    // spilled row's bytes come back into the heap here; the
                    // next checkpoint spills them again.
                    let bytes = match storage.row_bytes(table_index, rowid, old_home, arena) {
                        Ok(b) => b,
                        Err(e) => return sql_fail(e),
                    };
                    let copied = match arena.alloc_slice_copy(bytes) {
                        Ok(c) => c,
                        Err(_) => {
                            return sql_fail(sql_err!(
                                sqlstate::PROGRAM_LIMIT_EXCEEDED,
                                "ALTER rewrite exceeds the statement arena"
                            ))
                        }
                    };
                    let (loc, slice) = match storage.heap.append(copied.len()) {
                        Ok(x) => x,
                        Err(e) => return sql_fail(e),
                    };
                    slice.copy_from_slice(copied);
                    loc
                }
            }
        };
        scratch[i] = (rowid, RowHome::Heap(new_loc));
    }

    // A uniqueness constraint added alongside a rewrite is validated across the
    // transformed images now in the heap, before journaling — a cross-row check
    // that cannot run against the stored (still old) rows. NULLs are distinct.
    if has_rewrite && has_added_unique {
        for a in 0..scratch.len() {
            let RowHome::Heap(la) = scratch[a].1 else { unreachable!() };
            let abytes = storage.heap.get(la);
            let mut avals = [Datum::Null; MAX_COLUMNS];
            if let Err(e) = rowenc::decode(abytes, new_schema, &mut avals) {
                return sql_fail(e);
            }
            for b in (a + 1)..scratch.len() {
                let RowHome::Heap(lb) = scratch[b].1 else { unreachable!() };
                let bbytes = storage.heap.get(lb);
                let mut bvals = [Datum::Null; MAX_COLUMNS];
                if let Err(e) = rowenc::decode(bbytes, new_schema, &mut bvals) {
                    return sql_fail(e);
                }
                if let Some(name) = rewritten_dup_name(
                    &new_def,
                    &avals[..new_def.n_columns],
                    &bvals[..new_def.n_columns],
                ) {
                    return sql_fail(sql_err!(
                        sqlstate::UNIQUE_VIOLATION,
                        "duplicate key value violates unique constraint \"{}\"",
                        name.as_str()
                    ));
                }
            }
        }
    }

    // Phase 2: journal the shape change and the re-homed rows. Every fallible
    // content step is already done; only WAL append can fail here, and it does
    // so before any in-memory swap.
    let lsn = storage.bump_lsn();
    if let Err(e) = wal.append(
        lsn,
        &WalOp::DropTable { schema: def.schema.as_str(), name: def.name.as_str() },
    ) {
        return sql_fail(e);
    }
    let lsn = storage.bump_lsn();
    if let Err(e) = wal.append(lsn, &WalOp::CreateTable(new_def)) {
        return sql_fail(e);
    }
    for i in 0..scratch.len() {
        let (rowid, new_home) = scratch[i];
        let RowHome::Heap(new_loc) = new_home else {
            unreachable!("phase 1 re-homes every row to the heap");
        };
        let lsn = storage.bump_lsn();
        if let Err(e) = wal.append(
            lsn,
            &WalOp::Upsert {
                schema: new_def.schema.as_str(),
                table: new_def.name.as_str(),
                rowid,
                row: storage.heap.get(new_loc),
            },
        ) {
            return sql_fail(e);
        }
    }

    // Phase 3: swap in memory. Nothing here can fail. Every row now has a heap
    // image, so the old spill SST no longer serves this table.
    storage.set_table_def(table_index, new_def);
    // Seed the counter for any ADD IDENTITY ... (START WITH n).
    for &(col, seed) in &identity_seeds[..n_identity_seeds] {
        let table = storage.table_mut(table_index);
        table.serial_last[col] = seed;
        table.serial_dirty = true;
    }
    for i in 0..scratch.len() {
        let (rowid, new_home) = scratch[i];
        let state = storage
            .table_mut(table_index)
            .rows
            .get_mut(&rowid)
            .expect("row existed in phase 1");
        state.committed = Some(new_home);
    }
    storage.set_spill_list(table_index, &[]);
    // The column layout and/or constraint set changed and the rows were rehomed
    // outside the per-row commit path, so rebuild this table's value indexes
    // from the new committed image.
    if let Err(e) = storage.refresh_enforcers(table_index) {
        return sql_fail(e);
    }
    responder.command_complete("ALTER TABLE")?;
    sql_ok()
}

fn undefined_column(name: &str) -> SqlError {
    sql_err!(
        sqlstate::UNDEFINED_COLUMN,
        "column \"{}\" does not exist",
        name
    )
}

/// Renames a single-column UNIQUE/PRIMARY KEY that rides a column flag: when
/// `from` is the flag's synthesized name (`<table>_pkey` / `<table>_<col>_key`),
/// clears the flag and adds a named key with `new_name` so the constraint keeps
/// its new name. Returns whether a flag key matched.
fn rename_flag_key(def: &mut TableDef, from: &str, new_name: SqlName) -> Result<bool, SqlError> {
    for i in 0..def.n_columns {
        let col = def.columns[i];
        if !(col.unique || col.primary) {
            continue;
        }
        let synthesized = if col.primary {
            stack_format!(128, "{}_pkey", def.name.as_str())
        } else {
            stack_format!(128, "{}_{}_key", def.name.as_str(), col.name.as_str())
        };
        if synthesized.as_str() == from {
            let was_primary = col.primary;
            def.columns[i].unique = false;
            def.columns[i].primary = false;
            let mut indices = [0u16; crate::storage::MAX_INDEX_COLS];
            indices[0] = i as u16;
            crate::sql::exec::ddl::add_unique_key(
                def,
                Some(new_name.as_str()),
                if was_primary { "pkey" } else { "key" },
                &indices,
                1,
                was_primary,
            )?;
            return Ok(true);
        }
    }
    Ok(false)
}

pub fn eval_offset_pub(offset: Option<&Expr>, arena: &Arena, params: &[Datum]) -> Result<u64, SqlError> {
    let Some(expression) = offset else {
        return Ok(0);
    };
    match eval(expression, arena, params, &NoColumns)? {
        Datum::Null => Ok(0),
        Datum::Int2(v) if v >= 0 => Ok(v as u64),
        Datum::Int4(v) if v >= 0 => Ok(v as u64),
        Datum::Int8(v) if v >= 0 => Ok(v as u64),
        Datum::Int2(_) | Datum::Int4(_) | Datum::Int8(_) => {
            Err(sql_err!(sqlstate::INVALID_ROW_COUNT_IN_RESULT_OFFSET, "OFFSET must not be negative"))
        }
        _ => Err(sql_err!(
            sqlstate::DATATYPE_MISMATCH,
            "argument of OFFSET must be an integer"
        )),
    }
}

/// ORDER BY <n> refers to the n-th select item, as in PostgreSQL.
pub fn resolve_order_expr_pub<'a>(
    expression: &'a Expr<'a>,
    items: &'a [SelectItem<'a>],
) -> Result<&'a Expr<'a>, SqlError> {
    // An unqualified name that matches a SELECT-list output column binds to
    // that output column, as in PostgreSQL (output names win over input
    // columns for a simple ORDER BY name). Matching two or more output columns
    // is ambiguous (42702), matching PostgreSQL's findTargetlistEntrySQL92 —
    // e.g. `SELECT (CASE .. ELSE b END), b ... ORDER BY b`, where the CASE
    // inherits the name `b` from its ELSE column.
    if let Expr::Column { qualifier: None, name } = expression {
        let mut found: Option<&'a Expr<'a>> = None;
        for item in items {
            if let SelectItem::Expr { expression: item_expr, alias } = item {
                let out_name = alias.unwrap_or(derived_name(item_expr));
                if out_name == *name {
                    match found {
                        // Two output columns share the name but resolve to
                        // different expressions — ambiguous (`SELECT s, s` is
                        // not, both being the same column).
                        Some(f) if *f != **item_expr => {
                            return Err(sql_err!(
                                crate::sql::eval::sqlstate::AMBIGUOUS_COLUMN,
                                "ORDER BY \"{}\" is ambiguous",
                                name
                            ));
                        }
                        Some(_) => {}
                        None => found = Some(item_expr),
                    }
                }
            }
        }
        if let Some(item_expr) = found {
            return Ok(item_expr);
        }
    }
    // Ordinal positions (`ORDER BY 2`) are resolved by the caller against the
    // expanded output columns (stars count per column, as in PostgreSQL).
    Ok(expression)
}


pub fn eval_limit_pub(limit: Option<&Expr>, arena: &Arena, params: &[Datum]) -> Result<u64, SqlError> {
    let Some(expression) = limit else {
        return Ok(u64::MAX);
    };
    match eval(expression, arena, params, &NoColumns)? {
        Datum::Null => Ok(u64::MAX),
        Datum::Int2(v) if v >= 0 => Ok(v as u64),
        Datum::Int4(v) if v >= 0 => Ok(v as u64),
        Datum::Int8(v) if v >= 0 => Ok(v as u64),
        Datum::Int2(_) | Datum::Int4(_) | Datum::Int8(_) => Err(sql_err!(
            crate::sql::eval::sqlstate::INVALID_ROW_COUNT_IN_LIMIT_CLAUSE,
            "LIMIT must not be negative"
        )),
        _ => Err(sql_err!(
            sqlstate::DATATYPE_MISMATCH,
            "argument of LIMIT must be an integer"
        )),
    }
}

#[expect(clippy::too_many_arguments, reason = "row pipeline plumbing")]
fn row_matches<'a>(
    storage: &Storage,
    table_index: usize,
    rowid: u64,
    def: &TableDef,
    schema: &[ColType],
    home: RowHome,
    where_clause: Option<&Expr<'a>>,
    arena: &'a Arena,
    params: &[Datum<'a>],
    hooks: &super::eval::EvalHooks<'_, 'a>,
) -> Result<bool, SqlError> {
    let Some(w) = where_clause else {
        return Ok(true);
    };
    // Consume-in-place: the row's bytes live only for this predicate — a
    // WHERE over thousands of spilled rows must not fill the arena with
    // rows it rejects. (A WHERE subquery's own spilled reads take the
    // arena path, not this scratch, so nesting holds.)
    storage.with_row_bytes(table_index, rowid, home, |bytes| {
        let mut values = [Datum::Null; MAX_COLUMNS];
        rowenc::decode(bytes, schema, &mut values)?;
        row_matches_values(def, &values, w, arena, params, hooks)
    })
}

fn row_matches_values<'a>(
    def: &TableDef,
    values: &[Datum<'_>],
    w: &Expr<'a>,
    arena: &'a Arena,
    params: &[Datum<'a>],
    hooks: &super::eval::EvalHooks<'_, 'a>,
) -> Result<bool, SqlError> {
    let context = RowCtx { def, values: &values[..def.n_columns] };
    match super::eval::eval_full(w, arena, params, &context, hooks)? {
        Datum::Bool(true) => Ok(true),
        Datum::Bool(false) | Datum::Null => Ok(false),
        _ => Err(sql_err!(
            sqlstate::DATATYPE_MISMATCH,
            "argument of WHERE must be type boolean"
        )),
    }
}

#[expect(clippy::too_many_arguments, reason = "row pipeline plumbing")]
fn collect_matches<'a>(
    storage: &Storage,
    table_index: usize,
    txid: u32,
    schema: &[ColType],
    where_clause: Option<&Expr<'a>>,
    arena: &'a Arena,
    params: &[Datum<'a>],
    hooks: &super::eval::EvalHooks<'_, 'a>,
    scratch: &mut FixedVec<(u64, RowHome)>,
) -> Result<(), SqlError> {
    scratch.clear();
    let def = &storage.table(table_index).def;
    storage.for_each_row_state(table_index, &mut |rowid, state| {
        use core::ops::ControlFlow;
        let Some(loc) = state.visible_at(txid, storage.read_snapshot()) else {
            return Ok(ControlFlow::Continue(()));
        };
        if row_matches(storage, table_index, rowid, def, schema, loc, where_clause, arena, params, hooks)? {
            scratch.push((rowid, loc)).map_err(|_| {
                sql_err!(
                    sqlstate::PROGRAM_LIMIT_EXCEEDED,
                    "statement touches more than {} rows",
                    scratch.capacity()
                )
            })?;
        }
        Ok(ControlFlow::Continue(()))
    })?;
    Ok(())
}

/// Collects target rows that join at least one row of the extra `from` tables
/// satisfying `where_clause` — for `UPDATE ... FROM` / `DELETE ... USING`. The
/// target row supplies its columns as the outer scope of the FROM scan.
#[allow(clippy::too_many_arguments)]
fn collect_join_matches<'a>(
    storage: &'a Storage,
    table_index: usize,
    def: &TableDef,
    schema: &[ColType],
    from: &'a super::ast::FromClause<'a>,
    where_clause: Option<&'a Expr<'a>>,
    arena: &'a Arena,
    params: &[Datum<'a>],
    txid: u32,
    scratch: &mut FixedVec<(u64, RowHome)>,
) -> Result<(), SqlError> {
    scratch.clear();
    storage.for_each_row_state(table_index, &mut |rowid, state| {
        use core::ops::ControlFlow;
        let Some(loc) = state.visible_at(txid, storage.read_snapshot()) else {
            return Ok(ControlFlow::Continue(()));
        };
        // Consume-in-place, as in row_matches: the joined-row probe reads
        // this row's values only while it runs.
        let found = storage.with_row_bytes(table_index, rowid, loc, |bytes| {
            let mut tv = [Datum::Null; MAX_COLUMNS];
            rowenc::decode(bytes, schema, &mut tv)?;
            let context = RowCtx { def, values: &tv[..def.n_columns] };
            super::query::first_from_match(
                storage, from, txid, where_clause, arena, params, &context, &mut |_| Ok(()),
            )
        })?;
        if found {
            scratch.push((rowid, loc)).map_err(|_| {
                sql_err!(
                    sqlstate::PROGRAM_LIMIT_EXCEEDED,
                    "statement touches more than {} rows",
                    scratch.capacity()
                )
            })?;
        }
        Ok(ControlFlow::Continue(()))
    })?;
    Ok(())
}

fn store_row(
    storage: &mut Storage,
    txn: &mut TxnState,
    table_index: usize,
    rowid: Option<u64>,
    values: &[Datum],
) -> Result<(), SqlError> {
    let len = rowenc::encoded_len(values);
    // Encode straight into the heap: values may borrow the arena but not
    // the heap (they come from INSERT expressions), so this is borrow-safe.
    let (loc, slice) = storage.heap.append(len)?;
    rowenc::encode(values, slice);
    let rowid = rowid.unwrap_or_else(|| storage.next_rowid());
    let prior = storage.write_pending(table_index, rowid, txn.txid, txn.command_id(), Some(loc))?;
    if let Err(e) = txn.touch(table_index as u32, rowid, prior) {
        storage.restore_pending(table_index, rowid, txn.txid, prior);
        return Err(e);
    }
    Ok(())
}

fn coerce<'a>(
    v: Datum<'a>,
    col: &ColumnMeta,
    storage: &Storage,
    arena: &'a Arena,
) -> Result<Datum<'a>, SqlError> {
    // An enum column resolves a text (or already-typed enum) value to a member
    // of its type, validating the label against the catalog (22P02 otherwise).
    if let ColType::Enum(slot) = col.ctype {
        return coerce_enum_value(v, slot, storage, arena);
    }
    let v = cast_to(v, col.ctype, arena).map_err(|e| {
        // Data errors (out of range, bad input syntax — class 22) keep their
        // specific message; only a genuine type mismatch is rewritten with the
        // column context.
        if e.sqlstate.starts_with("22") {
            e
        } else {
            sql_err!(
                sqlstate::DATATYPE_MISMATCH,
                "column \"{}\" is of type {} but expression is of incompatible type",
                col.name.as_str(),
                col.ctype.name()
            )
        }
    })?;
    apply_typmod(v, col.ctype, col.type_mod, arena)
}

/// Coerces a value into an enum column: a NULL passes through; a text or
/// already-typed enum value must name a member of the enum at `slot`, else
/// PostgreSQL's 22P02 `invalid input value for enum <type>: "..."`.
fn coerce_enum_value<'a>(
    v: Datum<'a>,
    slot: u16,
    storage: &Storage,
    arena: &'a Arena,
) -> Result<Datum<'a>, SqlError> {
    if v.is_null() {
        return Ok(Datum::Null);
    }
    let def = storage.enum_def(slot as usize);
    let label = match v {
        Datum::Enum { label, .. } | Datum::Text(label) => label,
        Datum::Bpchar(s) => s.trim_end_matches(' '),
        _ => {
            return Err(sql_err!(
                sqlstate::DATATYPE_MISMATCH,
                "column is of type {} but expression is of incompatible type",
                def.name.as_str()
            ))
        }
    };
    let Some(sort) = def.sort_of(label) else {
        return Err(sql_err!(
            sqlstate::INVALID_TEXT_REPRESENTATION,
            "invalid input value for enum {}: \"{}\"",
            def.name.as_str(),
            label
        ));
    };
    Ok(Datum::Enum {
        slot,
        sort,
        label: arena.alloc_str(label).map_err(|_| super::query::arena_full_pub())?,
    })
}

/// Applies a type modifier to an explicit cast result. Differs from column
/// assignment ([`apply_typmod`]) in one way that matches PostgreSQL: an
/// over-length `varchar(n)`/`char(n)` cast TRUNCATES rather than erroring.
/// Numeric precision/scale still round or overflow as in a column.
pub fn apply_cast_typmod<'a>(
    v: Datum<'a>,
    ctype: ColType,
    type_mod: i32,
    arena: &'a Arena,
) -> Result<Datum<'a>, SqlError> {
    // Decoded once; the arms below match on meaning, so no site here can read
    // the modifier under the wrong encoding.
    let modifier = TypeMod::decode(ctype, type_mod);
    if modifier == TypeMod::None || v.is_null() {
        return Ok(v);
    }
    match (ctype, modifier, v) {
        (ColType::Text | ColType::Varchar, TypeMod::Length(max), Datum::Text(s)) => {
            if s.chars().count() > max {
                let end = s.char_indices().nth(max).map_or(s.len(), |(i, _)| i);
                let t = arena.alloc_str(&s[..end]).map_err(|_| {
                    sql_err!(sqlstate::PROGRAM_LIMIT_EXCEEDED, "cast result too large")
                })?;
                return Ok(Datum::Text(t));
            }
            Ok(v)
        }
        (ColType::Bpchar, TypeMod::Length(n), Datum::Text(s) | Datum::Bpchar(s)) => {
            // The padded result is a *bpchar* value: the padding is part of it
            // (output functions and LIKE see it), while comparisons and text
            // casts strip it — which is what the variant encodes. A bpchar
            // source re-fits from its stripped text (`c::char(3)`).
            match bpchar_fit(s.trim_end_matches(' '), n, true, arena)? {
                Datum::Text(padded) => Ok(Datum::Bpchar(padded)),
                other => Ok(other),
            }
        }
        (ColType::Bit { varying }, TypeMod::Length(n), Datum::Bit { bits, .. }) => {
            super::eval::fit_bits(bits, n, varying, arena)
        }
        _ => apply_typmod(v, ctype, type_mod, arena),
    }
}

/// Fits a string into `char(n)`: over-length truncates (cast) or errors
/// (column), and a short value is blank-padded to `n` characters.
fn bpchar_fit<'a>(
    s: &'a str,
    n: usize,
    truncate: bool,
    arena: &'a Arena,
) -> Result<Datum<'a>, SqlError> {
    let clen = s.chars().count();
    if clen > n {
        if truncate {
            let end = s.char_indices().nth(n).map_or(s.len(), |(i, _)| i);
            let t = arena
                .alloc_str(&s[..end])
                .map_err(|_| sql_err!(sqlstate::PROGRAM_LIMIT_EXCEEDED, "cast result too large"))?;
            return Ok(Datum::Text(t));
        }
        return Err(sql_err!(sqlstate::STRING_DATA_RIGHT_TRUNCATION, "value too long for type character({})", n));
    }
    if clen == n {
        return Ok(Datum::Text(s));
    }
    // Blank-pad to n characters (a space is one byte).
    let total = s.len() + (n - clen);
    let buffer = arena
        .alloc_slice_with(total, |_| b' ')
        .map_err(|_| sql_err!(sqlstate::PROGRAM_LIMIT_EXCEEDED, "padded value too large"))?;
    buffer[..s.len()].copy_from_slice(s.as_bytes());
    Ok(Datum::Text(unsafe { core::str::from_utf8_unchecked(buffer) }))
}

/// Enforces a PostgreSQL atttypmod on an already-cast value: varchar(n) length
/// (22001) and numeric(p,s) rounding to scale + precision (22003). Values with
/// no modifier, and NULLs, pass through unchanged.
pub fn apply_typmod<'a>(
    v: Datum<'a>,
    ctype: ColType,
    type_mod: i32,
    arena: &'a Arena,
) -> Result<Datum<'a>, SqlError> {
    let modifier = TypeMod::decode(ctype, type_mod);
    if modifier == TypeMod::None || v.is_null() {
        return Ok(v);
    }
    match (ctype, modifier, v) {
        (ColType::Text | ColType::Varchar, TypeMod::Length(max), Datum::Text(s)) => {
            if s.chars().count() > max {
                // Excess that is entirely spaces truncates silently, the same
                // allowance PostgreSQL gives both varchar and char columns.
                let end = s.char_indices().nth(max).map_or(s.len(), |(i, _)| i);
                if s[end..].chars().all(|c| c == ' ') {
                    return Ok(Datum::Text(&s[..end]));
                }
                return Err(sql_err!(
                    sqlstate::STRING_DATA_RIGHT_TRUNCATION,
                    "value too long for type character varying({})",
                    max
                ));
            }
            Ok(v)
        }
        (_, TypeMod::Length(n), Datum::Text(s) | Datum::Bpchar(s)) => {
            match bpchar_fit(s.trim_end_matches(' '), n, false, arena)? {
                Datum::Text(padded) => Ok(Datum::Bpchar(padded)),
                other => Ok(other),
            }
        }
        (_, TypeMod::NumericPS { precision, scale }, Datum::Numeric(n)) => {
            apply_numeric_typmod(&n, precision as usize, scale as usize, arena)
                .map(Datum::Numeric)
        }
        (ColType::Bit { varying }, TypeMod::Length(n), Datum::Bit { bits, .. }) => {
            super::eval::fit_bits(bits, n, varying, arena)
        }
        // Fractional-second precision: micros round half-away-from-zero in
        // integer arithmetic, as PostgreSQL's AdjustTimestampForTypmod.
        (_, TypeMod::TemporalPrecision(p), Datum::Timestamp(t)) => {
            Ok(Datum::Timestamp(round_micros(t, p)))
        }
        (_, TypeMod::TemporalPrecision(p), Datum::Timestamptz(t)) => {
            Ok(Datum::Timestamptz(round_micros(t, p)))
        }
        (_, TypeMod::TemporalPrecision(p), Datum::Time(t)) => Ok(Datum::Time(round_micros(t, p))),
        (_, TypeMod::TemporalPrecision(p), Datum::Timetz(t, zone)) => {
            Ok(Datum::Timetz(round_micros(t, p), zone))
        }
        // An interval range form with no precision (`interval hour to minute`)
        // rounds nothing — its `precision: None` cannot be mistaken for a
        // number, where the packed 0xFFFF once could.
        (_, TypeMod::IntervalMod { precision: Some(p), .. }, Datum::Interval(iv)) => {
            Ok(Datum::Interval(crate::sql::types::Interval {
                months: iv.months,
                days: iv.days,
                micros: round_micros(iv.micros, p),
            }))
        }
        _ => Ok(v),
    }
}

/// Rounds microseconds to `p` (0..=6) fractional-second digits,
/// half-away-from-zero in integer arithmetic (PostgreSQL's
/// `AdjustTimestampForTypmod`).
fn round_micros(micros: i64, p: u8) -> i64 {
    let p = u32::from(p.min(6));
    let scale = 10i64.pow(6 - p);
    if scale == 1 {
        return micros;
    }
    let offset = scale / 2;
    if micros >= 0 {
        (micros + offset) / scale * scale
    } else {
        -((-micros + offset) / scale * scale)
    }
}

/// Rounds to `scale` fractional digits (half away from zero) and checks that
/// the result fits in `precision` significant digits, as PostgreSQL does when
/// storing into numeric(precision, scale). Works on the decimal text so the
/// base-10000 carry logic lives in one place (Numeric::parse). NaN carries no
/// scale.
fn apply_numeric_typmod<'a>(
    n: &super::numeric::Numeric,
    precision: usize,
    scale: usize,
    arena: &'a Arena,
) -> Result<super::numeric::Numeric<'a>, SqlError> {
    use super::numeric::Numeric;
    if n.is_nan() {
        return Numeric::parse("NaN", arena);
    }
    const DIG: usize = 2100;
    let text = stack_format!(2100, "{}", n);
    let s = text.as_str();
    let (neg, body) = match s.strip_prefix('-') {
        Some(r) => (true, r),
        None => (false, s),
    };
    let (int_part, frac_part) = body.split_once('.').unwrap_or((body, ""));
    let (int_b, frac_b) = (int_part.as_bytes(), frac_part.as_bytes());
    let int_len = int_b.len();
    if int_len + scale + 2 >= DIG {
        return Err(sql_err!(sqlstate::NUMERIC_OUT_OF_RANGE, "numeric field overflow"));
    }

    // Kept digits: every integer digit, then `scale` fractional digits (padded
    // with zeros), then round based on the first dropped fractional digit.
    let mut digits = [b'0'; DIG];
    digits[..int_len].copy_from_slice(int_b);
    for i in 0..scale {
        digits[int_len + i] = *frac_b.get(i).unwrap_or(&b'0');
    }
    let mut carry = frac_b.get(scale).is_some_and(|&d| d >= b'5');
    let mut i = int_len + scale;
    while carry && i > 0 {
        i -= 1;
        if digits[i] == b'9' {
            digits[i] = b'0';
        } else {
            digits[i] += 1;
            carry = false;
        }
    }

    // Significant integer digits: a carry out of the integer part means the
    // value rolled up to 1 followed by `int_len` zeros.
    let sig_int = if carry {
        int_len + 1
    } else {
        let lead_zeros = digits[..int_len].iter().take_while(|&&d| d == b'0').count();
        int_len - lead_zeros
    };
    if sig_int > precision - scale {
        return Err(sql_err!(sqlstate::NUMERIC_OUT_OF_RANGE, "numeric field overflow"));
    }

    // Reassemble and re-parse (parse sets dscale = scale, matching PostgreSQL).
    let mut out = [0u8; DIG + 8];
    let mut k = 0;
    if neg {
        out[k] = b'-';
        k += 1;
    }
    if carry {
        out[k] = b'1';
        k += 1;
    }
    out[k..k + int_len].copy_from_slice(&digits[..int_len]);
    k += int_len;
    if scale > 0 {
        out[k] = b'.';
        k += 1;
        out[k..k + scale].copy_from_slice(&digits[int_len..int_len + scale]);
        k += scale;
    }
    let rounded = core::str::from_utf8(&out[..k]).expect("ascii digits");
    Numeric::parse(rounded, arena)
}

fn check_not_null(def: &TableDef, values: &[Datum]) -> Result<(), SqlError> {
    for (i, c) in def.columns().iter().enumerate() {
        if c.not_null && values[i].is_null() {
            return Err(sql_err!(
                sqlstate::NOT_NULL_VIOLATION,
                "null value in column \"{}\" of relation \"{}\" violates not-null constraint",
                c.name.as_str(),
                def.name.as_str()
            ));
        }
    }
    Ok(())
}

fn undefined_table(name: &str) -> SqlError {
    sql_err!(
        sqlstate::UNDEFINED_TABLE,
        "relation \"{}\" does not exist",
        name
    )
}

/// PostgreSQL's 42P01 echoes the spelling used: a qualified reference names
/// its qualifier, a bare one does not.
fn undefined_qual(name: &QualName) -> SqlError {
    match name.schema {
        Some(schema) => sql_err!(
            sqlstate::UNDEFINED_TABLE,
            "relation \"{}.{}\" does not exist",
            schema,
            name.name
        ),
        None => undefined_table(name.name),
    }
}

/// Resolves a DML/DDL target through the search path to a table slot.
pub(crate) fn resolve_dml_table(
    storage: &Storage,
    name: &QualName,
    txid: u32,
) -> Result<usize, SqlError> {
    match storage.resolve_relation(name.schema, name.name, txid) {
        Some(crate::storage::ResolvedRelation::Table(slot)) => Ok(slot),
        _ => Err(undefined_qual(name)),
    }
}


/// Public view of the OID-to-ColType mapping for value-level renderers
/// (`oid::regtype`).
pub fn coltype_of_oid_pub(o: i32) -> Option<crate::sql::types::ColType> {
    describe::coltype_of_oid(o)
}
