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
use crate::storage::{
    ColumnMeta, RowLoc, SqlName, Storage, TableDef, MAX_COLUMNS,
};
use super::txn::TxnState;
use crate::storage::rowenc;
use crate::wal::{Wal, WalOp};

use super::ast::{
    AlterAction, AlterTable, CreateTable, Delete, DropTable, Expr, Insert, LikeClause, SelectItem,
    Update,
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
            && q != self.def.name.as_str() {
                return Err(sql_err!(
                    "42P01",
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
                "42P01",
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
}

type Outcome = Result<Result<(), SqlError>, WireFull>;

fn sql_ok() -> Outcome {
    Ok(Ok(()))
}

fn sql_fail(e: SqlError) -> Outcome {
    Ok(Err(e))
}

mod describe;
pub use describe::{
    check_row_field_types, derived_name, describe_items, infer_type_pub, infer_type_res,
    record_field_type, record_shape, typeof_static, ColTypeResolver, DefCols, NoCols,
    RECORD_FIELD_NAMES,
};
pub(crate) use describe::{coltype_of_oid, json_each_value_type_pub, unify_numeric_tower};

mod projected;
pub use projected::{
    decode_projected_pub, decode_projected_value, encode_projected_pub, projected_prefix_len,
    projected_value_len,
};

mod ddl;
use ddl::{add_unique_key, attach_constraints, auto_key_name, build_column, build_def};

mod constraints;
pub use constraints::{check_all_unique, check_unique, check_unique_indexes};
use constraints::{
    apply_fk_parent_actions, enforce_row_constraints, parse_checks, referenced_key_changed,
    table_is_referenced, ParsedChecks, MAX_FK_CASCADE_DEPTH,
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
        }
        Err(e) if e.sqlstate == sqlstate::DUPLICATE_TABLE && statement.if_not_exists => {
            responder.notice(
                "42P07",
                stack_format!(128, "relation \"{}\" already exists, skipping", statement.name).as_str(),
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
            "42P16",
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
    match storage.find_visible(like.source, txid) {
        Some(i) => Ok(&storage.table(i).def),
        None => Err(undefined_table(like.source)),
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
        return build_def(statement.name, statement.columns, arena);
    }
    let mut def = TableDef { name: SqlName::parse(statement.name)?, ..TableDef::empty() };
    let mut n = 0usize;
    for position in 0..=statement.columns.len() {
        for like in statement.likes.iter().filter(|l| l.at == position) {
            let source = like_source(storage, like, txid)?;
            for column in source.columns() {
                let mut copied = *column;
                if !like.defaults {
                    copied.default_value = None;
                }
                if !like.indexes {
                    copied.unique = false;
                    copied.primary = false;
                }
                if !like.identity {
                    copied.auto_increment = false;
                }
                push_column(&mut def, &mut n, copied)?;
            }
        }
        if let Some(column) = statement.columns.get(position) {
            push_column(&mut def, &mut n, build_column(column, arena)?)?;
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
            "42701",
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
        for index in storage.indexes_for(like.source, txn.txid) {
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
        let source = match storage.find_visible(like.source, txn.txid) {
            Some(i) => storage.table(i).def,
            None => return Err(undefined_table(like.source)),
        };
        for index in &copied[..n_copied] {
            let columns = remap_columns(def, &source, &index.columns[..index.n_cols])?;
            let name = auto_key_name(def, &columns[..index.n_cols], "idx", true)?;
            let slot = storage.create_index(
                IndexDef {
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


/// One past the current maximum value of an auto-increment (serial) column in
/// the rows visible to `txid`, or 1 when empty. Rows this transaction already
/// inserted are visible, so a multi-row INSERT assigns increasing values.
fn next_auto_value<'x>(
    storage: &Storage,
    table_index: usize,
    col: usize,
    ctype: ColType,
    schema: &[ColType],
    txid: u32,
) -> Datum<'x> {
    let table = storage.table(table_index);
    let mut max: i64 = 0;
    for (_, state) in table.rows.iter() {
        let Some(loc) = state.visible_to(txid) else {
            continue;
        };
        let mut row = [Datum::Null; MAX_COLUMNS];
        if rowenc::decode(storage.heap.get(loc), schema, &mut row).is_err() {
            continue;
        }
        let v = match row.get(col) {
            Some(Datum::Int4(x)) => i64::from(*x),
            Some(Datum::Int8(x)) => *x,
            _ => continue,
        };
        max = max.max(v);
    }
    let next = max + 1;
    if ctype == ColType::Int8 {
        Datum::Int8(next)
    } else {
        Datum::Int4(next as i32)
    }
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
    let table = storage.table(table_index);
    let table_name = def.name.as_str();
    for (&rowid, state) in table.rows.iter() {
        let Some(loc) = state.visible_to(txid) else {
            continue;
        };
        let mut other = [Datum::Null; MAX_COLUMNS];
        if rowenc::decode(storage.heap.get(loc), schema, &mut other).is_err() {
            continue;
        }
        let eq = |a: &Datum, b: &Datum| {
            !a.is_null()
                && !b.is_null()
                && compare_datums(a, b).map(|o| o.is_eq()).unwrap_or(false)
        };
        for (i, c) in def.columns().iter().enumerate() {
            if c.unique && eq(&values[i], &other[i]) {
                return Some(rowid);
            }
        }
        for index in storage.unique_indexes_for(table_name, txid) {
            let icols = &index.columns[..index.n_cols];
            if !icols.iter().any(|&c| values[c as usize].is_null())
                && icols.iter().all(|&c| eq(&values[c as usize], &other[c as usize]))
            {
                return Some(rowid);
            }
        }
    }
    None
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
                && q != self.def.name.as_str()
            {
                return Err(sql_err!("42P01", "missing FROM-clause entry for table \"{}\"", q));
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
        let loc = storage
            .table(table_index)
            .rows
            .get(&rowid)
            .and_then(|s| s.visible_to(txn.txid))
            .ok_or_else(|| sql_err!("XX000", "conflict row vanished"))?;
        rowenc::decode(storage.heap.get(loc), schema, &mut existing)?;
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
            new_values[target] = coerce(v, &def.columns()[target], arena)?;
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
    let prior = storage.write_pending(table_index, rowid, txn.txid, Some(new_loc))?;
    if let Err(e) = txn.touch(table_index as u32, rowid, prior) {
        storage.restore_pending(table_index, rowid, txn.txid, prior);
        return Err(e);
    }
    Ok(ConflictOutcome::Updated)
}

/// Assigns each omitted/defaulted auto-increment column its next value.
fn fill_auto_increment(
    storage: &Storage,
    table_index: usize,
    def: &TableDef,
    values: &mut [Datum],
    txid: u32,
) {
    if !def.columns().iter().any(|c| c.auto_increment) {
        return;
    }
    let mut sch = [ColType::Bool; MAX_COLUMNS];
    def.schema(&mut sch);
    for (i, col) in def.columns().iter().enumerate() {
        if col.auto_increment && values[i].is_null() {
            values[i] =
                next_auto_value(storage, table_index, i, col.ctype, &sch[..def.n_columns], txid);
        }
    }
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
    match storage.find_visible(statement.name, txn.txid) {
        Some(index) => {
            if let Some(other) = storage.table(index).ddl_locked_by_other(txn.txid) {
                let _ = other;
                return sql_fail(sql_err!(
                    "40001",
                    "could not serialize access due to concurrent DDL on \"{}\"",
                    statement.name
                ));
            }
            let lsn = storage.bump_lsn();
            if let Err(e) = wal.append(lsn, &WalOp::DropTable(statement.name)) {
                return sql_fail(e);
            }
            if let Err(e) = txn.record_ddl(super::txn::DdlUndo::Dropped(index as u32)) {
                return sql_fail(e);
            }
            storage.drop_table_in(index, txn.txid);
            // A table's indexes are dropped with it (no separate journal record;
            // DropTable replay re-applies this).
            storage.drop_indexes_for(statement.name, txn.txid);
        }
        None if statement.if_exists => {
            // PostgreSQL's skip notice carries SQLSTATE 00000.
            responder.notice(
                "00000",
                stack_format!(128, "table \"{}\" does not exist, skipping", statement.name).as_str(),
            )?;
        }
        None => return sql_fail(undefined_kind("table", statement.name)),
    }
    responder.command_complete("DROP TABLE")?;
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
    name: &str,
    or_replace: bool,
    sql: &str,
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
    let sqlname = match SqlName::parse(name) {
        Ok(n) => n,
        Err(e) => return sql_fail(e),
    };
    match storage.create_view(sqlname, buffer, or_replace, txn.txid) {
        Ok((new_slot, old_slot)) => {
            let lsn = storage.bump_lsn();
            if let Err(e) = wal.append(lsn, &WalOp::CreateView { name, sql }) {
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

/// DROP VIEW [IF EXISTS].
pub fn drop_view(
    storage: &mut Storage,
    wal: &mut Wal,
    txn: &mut super::txn::TxnState,
    name: &str,
    if_exists: bool,
    responder: &mut Responder,
) -> Outcome {
    if storage.find_view(name, txn.txid).is_some() {
        let lsn = storage.bump_lsn();
        if let Err(e) = wal.append(lsn, &WalOp::DropView(name)) {
            return sql_fail(e);
        }
        let dropped = match storage.drop_view(name, txn.txid) {
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
            "42P01",
            stack_format!(128, "view \"{}\" does not exist, skipping", name).as_str(),
        )?;
    } else {
        return sql_fail(sql_err!("42P01", "view \"{}\" does not exist", name));
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
    table: &str,
    column_names: &[&str],
    unique: bool,
    responder: &mut Responder,
) -> Outcome {
    use crate::storage::{IndexDef, MAX_INDEX_COLS};
    let Some(table_index) = storage.find_visible(table, txn.txid) else {
        return sql_fail(undefined_table(table));
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
    let n_cols = columns.len();
    let sqlname = match SqlName::parse(name) {
        Ok(n) => n,
        Err(e) => return sql_fail(e),
    };
    let table_name = match SqlName::parse(table) {
        Ok(n) => n,
        Err(e) => return sql_fail(e),
    };
    let def = IndexDef { name: sqlname, table: table_name, columns, n_cols, unique, live: true, pending: None };
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
        for (&rowid, state) in storage.table(table_index).rows.iter() {
            let Some(loc) = state.committed else { continue };
            let mut values = [Datum::Null; MAX_COLUMNS];
            if let Err(e) =
                rowenc::decode(storage.heap.get(loc), &schema[..tdef.n_columns], &mut values)
            {
                conflict = Some(e);
                break;
            }
            if let Err(e) = check_unique_indexes(
                storage,
                table_index,
                &tdef,
                &schema[..tdef.n_columns],
                &values[..tdef.n_columns],
                Some(rowid),
                // The just-registered index is an uncommitted CREATE owned by
                // this transaction; validation must see it.
                txn.txid,
            ) {
                conflict = Some(e);
                break;
            }
        }
        if let Some(e) = conflict {
            storage.rollback_index_create(slot);
            return sql_fail(e);
        }
    }
    let lsn = storage.bump_lsn();
    if let Err(e) = wal.append(lsn, &WalOp::CreateIndex { name, table, columns, n_cols, unique }) {
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
    name: &str,
    if_exists: bool,
    responder: &mut Responder,
) -> Outcome {
    if storage.index_exists(name, txn.txid) {
        let lsn = storage.bump_lsn();
        if let Err(e) = wal.append(lsn, &WalOp::DropIndex(name)) {
            return sql_fail(e);
        }
        let dropped = match storage.drop_index(name, txn.txid) {
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
            "42P01",
            stack_format!(128, "index \"{}\" does not exist, skipping", name).as_str(),
        )?;
    } else {
        // An index is an object, not a relation, to PostgreSQL's error codes:
        // a missing one is 42704, where a missing table is 42P01.
        return sql_fail(sql_err!(sqlstate::UNDEFINED_OBJECT, "index \"{}\" does not exist", name));
    }
    responder.command_complete("DROP INDEX")?;
    sql_ok()
}

pub fn insert(
    storage: &mut Storage,
    txn: &mut TxnState,
    statement: &Insert,
    arena: &Arena,
    params: &[Datum],
    responder: &mut Responder,
) -> Outcome {
    let Some(table_index) = storage.find_visible(statement.table, txn.txid) else {
        return sql_fail(undefined_table(statement.table));
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
                    statement.table
                ));
            };
            targets[i] = col;
        }
        statement.columns.len()
    };

    // RETURNING sends its RowDescription before any rows.
    if !statement.returning.is_empty() {
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
        // Pass 1: count.
        let mut count = 0usize;
        if let Err(e) = super::query::select_into_rows(
            storage, txn.txid, sel, arena, params, None, &mut |_| {
                count += 1;
                Ok(())
            },
        ) {
            return sql_fail(e);
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
        if let Err(e) = super::query::select_into_rows(storage, txn.txid, sel, arena, params, None, &mut fill) {
            return sql_fail(e);
        }

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
            for (i, col) in def.columns().iter().enumerate() {
                if let Some(d) = &col.default_value {
                    values[i] = d.as_datum();
                }
            }
            for i in 0..n_src {
                let v = decode_projected_pub(bytes, i);
                let col = &def.columns()[targets[i]];
                match coerce(v, col, arena) {
                    Ok(v) => values[targets[i]] = v,
                    Err(e) => return sql_fail(e),
                }
            }
            fill_auto_increment(storage, table_index, &def, &mut values, txn.txid);
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
                && let Err(e) = emit_projected(&def, &values[..def.n_columns], statement.returning, arena, params, responder)? {
                    return sql_fail(e);
                }
            inserted += 1;
        }
        let tag = stack_format!(48, "INSERT 0 {}", inserted);
        responder.command_complete(tag.as_str())?;
        return sql_ok();
    }

    let mut inserted = 0u64;
    for row_exprs in statement.rows {
        if row_exprs.len() > n_targets {
            return sql_fail(sql_err!(
                sqlstate::SYNTAX_ERROR,
                "INSERT has more expressions than target columns"
            ));
        }
        // Every column starts at its default; explicit values overwrite.
        // The datums borrow `def`, which outlives the row.
        let mut values = [Datum::Null; MAX_COLUMNS];
        for (i, col) in def.columns().iter().enumerate() {
            if let Some(d) = &col.default_value {
                values[i] = d.as_datum();
            }
        }
        for (i, expression) in row_exprs.iter().enumerate() {
            if matches!(expression, Expr::DefaultMarker) {
                continue; // keep the default already in place
            }
            let v = match eval(expression, arena, params, &NoColumns) {
                Ok(v) => v,
                Err(e) => return sql_fail(e),
            };
            let col = &def.columns()[targets[i]];
            match coerce(v, col, arena) {
                Ok(v) => values[targets[i]] = v,
                Err(e) => return sql_fail(e),
            }
        }
        fill_auto_increment(storage, table_index, &def, &mut values, txn.txid);
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
            && let Err(e) = emit_projected(&def, &values[..def.n_columns], statement.returning, arena, params, responder)? {
                return sql_fail(e);
            }
        inserted += 1;
    }
    let tag = stack_format!(48, "INSERT 0 {}", inserted);
    responder.command_complete(tag.as_str())?;
    sql_ok()
}

/// Projects `values` through `items` and emits one DataRow.
fn emit_projected(
    def: &TableDef,
    values: &[Datum],
    items: &[SelectItem],
    arena: &Arena,
    params: &[Datum],
    responder: &mut Responder,
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
                if *q != def.name.as_str() {
                    return Ok(Err(sql_err!(
                        "42P01",
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
    responder.data_row(&projected[..n])?;
    Ok(Ok(()))
}

pub fn update(
    storage: &mut Storage,
    txn: &mut TxnState,
    scratch: &mut FixedVec<(u64, RowLoc)>,
    statement: &Update,
    arena: &Arena,
    params: &[Datum],
    responder: &mut Responder,
) -> Outcome {
    let Some(table_index) = storage.find_visible(statement.table, txn.txid) else {
        return sql_fail(undefined_table(statement.table));
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
                statement.table
            ));
        };
        targets[i] = col;
    }

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
    let hooks = super::eval::EvalHooks { group: None, aggs: None, subs: Some(&subs) , windows: None, catalog: None, srf_index: None };
    let collect = if let Some(from) = statement.from {
        collect_join_matches(storage, table_index, &def, schema, from, statement.where_clause, arena, params, txn.txid, scratch)
    } else {
        collect_matches(storage, table_index, txn.txid, schema, statement.where_clause, arena, params, &hooks, scratch)
    };
    if let Err(e) = collect {
        return sql_fail(e);
    }

    if !statement.returning.is_empty() {
        let mut columns = [ColDesc::new("", 0, 0); MAX_PROJ];
        match describe_items(statement.returning, Some(&def), &mut columns) {
            Ok(n) => responder.row_description(&columns[..n])?,
            Err(e) => return sql_fail(e),
        }
    }

    let mut updated = 0u64;
    for i in 0..scratch.len() {
        let (rowid, loc) = scratch[i];
        // Build the new row image in the statement arena so the heap
        // borrow ends before the heap is appended to.
        // An arena-owned copy of the old row bytes: the referential-action
        // pass below needs the old values after storage mutates.
        let row_bytes = match arena.alloc_slice_copy(storage.heap.get(loc)) {
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
                            let v = eval(expression, arena, params, &combined)?;
                            new_values[targets[a]] = coerce(v, &def.columns()[targets[a]], arena)?;
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
                for (a, (_, expression)) in statement.assignments.iter().enumerate() {
                    let v = match eval(expression, arena, params, &context) {
                        Ok(v) => v,
                        Err(e) => return sql_fail(e),
                    };
                    let col = &def.columns()[targets[a]];
                    match coerce(v, col, arena) {
                        Ok(v) => new_values[targets[a]] = v,
                        Err(e) => return sql_fail(e),
                    }
                }
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
        match storage.write_pending(table_index, rowid, txn.txid, Some(new_loc)) {
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
                statement.table,
                &old_row[..def.n_columns],
                &new_row[..def.n_columns],
                txn.txid,
            ) && let Err(e) = apply_fk_parent_actions(
                storage,
                txn,
                statement.table,
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
            )? {
                return sql_fail(e);
            }
        }
        updated += 1;
    }
    let tag = stack_format!(48, "UPDATE {}", updated);
    responder.command_complete(tag.as_str())?;
    sql_ok()
}

pub fn delete(
    storage: &mut Storage,
    txn: &mut TxnState,
    scratch: &mut FixedVec<(u64, RowLoc)>,
    statement: &Delete,
    arena: &Arena,
    params: &[Datum],
    responder: &mut Responder,
) -> Outcome {
    let Some(table_index) = storage.find_visible(statement.table, txn.txid) else {
        return sql_fail(undefined_table(statement.table));
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
    let hooks = super::eval::EvalHooks { group: None, aggs: None, subs: Some(&subs) , windows: None, catalog: None, srf_index: None };
    let collect = if let Some(using) = statement.using {
        collect_join_matches(storage, table_index, &def, schema, using, statement.where_clause, arena, params, txn.txid, scratch)
    } else {
        collect_matches(storage, table_index, txn.txid, schema, statement.where_clause, arena, params, &hooks, scratch)
    };
    if let Err(e) = collect {
        return sql_fail(e);
    }
    if !statement.returning.is_empty() {
        let mut columns = [ColDesc::new("", 0, 0); MAX_PROJ];
        match describe_items(statement.returning, Some(&def), &mut columns) {
            Ok(n) => responder.row_description(&columns[..n])?,
            Err(e) => return sql_fail(e),
        }
    }
    let referenced = table_is_referenced(storage, statement.table, txn.txid);
    for i in 0..scratch.len() {
        let (rowid, old_loc) = scratch[i];
        if !statement.returning.is_empty() || referenced {
            // The cascade below mutates storage, so the row image is decoded
            // from an arena-owned copy.
            let old_copy = match arena.alloc_slice_copy(storage.heap.get(old_loc)) {
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
                    statement.table,
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
                )?
            {
                return sql_fail(e);
            }
        }
        match storage.write_pending(table_index, rowid, txn.txid, None) {
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
    responder.command_complete(tag.as_str())?;
    sql_ok()
}

/// ALTER TABLE, autocommit-only: rewrites are journaled as DROP, CREATE,
/// full re-UPSERT within one WAL batch, so replay reproduces the new
/// shape atomically. Two-phase in memory: all new row images are prepared
/// first, then the definition and row map swap; a failure part-way leaves
/// the table untouched (only heap bytes leak until compaction).
pub fn alter_table(
    storage: &mut Storage,
    wal: &mut Wal,
    scratch: &mut FixedVec<(u64, RowLoc)>,
    statement: &AlterTable,
    arena: &Arena,
    responder: &mut Responder,
) -> Outcome {
    let Some(table_index) = storage.find_table(statement.table) else {
        return sql_fail(undefined_table(statement.table));
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
            "55P03",
            "table \"{}\" has uncommitted changes; retry when idle",
            statement.table
        ));
    }

    // Build the new definition and the per-row transform.
    let mut new_def = def;
    let mut added: Option<(usize, Datum)> = None; // (index, fill value)
    let mut dropped: Option<usize> = None;
    match &statement.action {
        AlterAction::RenameTable(new_name) => {
            if storage.find_table(new_name).is_some() {
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
            let Some(i) = def.column_index(from) else {
                return sql_fail(undefined_column(from));
            };
            if def.column_index(to).is_some() {
                return sql_fail(sql_err!("42701", "column \"{}\" already exists", to));
            }
            new_def.columns[i].name = match SqlName::parse(to) {
                Ok(n) => n,
                Err(e) => return sql_fail(e),
            };
        }
        AlterAction::AddColumn(c) => {
            if def.column_index(c.name).is_some() {
                return sql_fail(sql_err!("42701", "column \"{}\" already exists", c.name));
            }
            if def.n_columns == MAX_COLUMNS {
                return sql_fail(sql_err!(
                    sqlstate::PROGRAM_LIMIT_EXCEEDED,
                    "tables can have at most {} columns",
                    MAX_COLUMNS
                ));
            }
            let meta = match build_column(c, arena) {
                Ok(m) => m,
                Err(e) => return sql_fail(e),
            };
            let fill = Datum::Null;
            let _ = fill;
            new_def.columns[def.n_columns] = meta;
            new_def.n_columns += 1;
            let index = def.n_columns;
            // NOT NULL without a default over a non-empty table is a
            // constraint violation, as in PostgreSQL.
            let has_rows = !storage.table(table_index).rows.is_empty();
            let fill_value = match &new_def.columns[index].default_value {
                Some(d) => d.as_datum(),
                None if meta.not_null && has_rows => {
                    return sql_fail(sql_err!(
                        sqlstate::NOT_NULL_VIOLATION,
                        "column \"{}\" of relation \"{}\" contains null values",
                        c.name,
                        statement.table
                    ))
                }
                None => Datum::Null,
            };
            added = Some((index, fill_value));
        }
        AlterAction::DropColumn(name) => {
            let Some(i) = def.column_index(name) else {
                return sql_fail(undefined_column(name));
            };
            if def.n_columns == 1 {
                return sql_fail(sql_err!(
                    "0A000",
                    "cannot drop the only column of a table"
                ));
            }
            for j in i..def.n_columns - 1 {
                new_def.columns[j] = def.columns[j + 1];
            }
            new_def.n_columns -= 1;
            dropped = Some(i);
        }
    }

    let mut old_schema = [ColType::Bool; MAX_COLUMNS];
    def.schema(&mut old_schema);
    let old_schema = &old_schema[..def.n_columns];

    // Phase 1: journal the shape change and prepare every rewritten row.
    let lsn = storage.bump_lsn();
    if let Err(e) = wal.append(lsn, &WalOp::DropTable(def.name.as_str())) {
        return sql_fail(e);
    }
    let lsn = storage.bump_lsn();
    if let Err(e) = wal.append(lsn, &WalOp::CreateTable(new_def)) {
        return sql_fail(e);
    }
    scratch.clear();
    let rewrite = added.is_some() || dropped.is_some();
    // Collect (rowid, old committed loc).
    let mut row_count = 0usize;
    {
        let table = storage.table(table_index);
        for (&rowid, state) in table.rows.iter() {
            let Some(loc) = state.committed else { continue };
            if scratch.push((rowid, loc)).is_err() {
                return sql_fail(sql_err!(
                    sqlstate::PROGRAM_LIMIT_EXCEEDED,
                    "ALTER touches more than {} rows",
                    scratch.capacity()
                ));
            }
            row_count += 1;
        }
    }
    let _ = row_count;
    for i in 0..scratch.len() {
        let (rowid, old_loc) = scratch[i];
        let new_loc = if rewrite {
            // Build the new image in the statement arena so the heap
            // borrow (decoded text refs) ends before the heap append.
            let new_bytes: &[u8] = {
                let mut values = [Datum::Null; MAX_COLUMNS];
                if let Err(e) =
                    rowenc::decode(storage.heap.get(old_loc), old_schema, &mut values)
                {
                    return sql_fail(e);
                }
                let mut out = [Datum::Null; MAX_COLUMNS];
                let n_out = new_def.n_columns;
                if let Some((index, ref fill)) = added {
                    out[..def.n_columns].copy_from_slice(&values[..def.n_columns]);
                    out[index] = *fill;
                } else if let Some(d) = dropped {
                    let mut w = 0;
                    for (j, v) in values[..def.n_columns].iter().enumerate() {
                        if j != d {
                            out[w] = *v;
                            w += 1;
                        }
                    }
                }
                let len = rowenc::encoded_len(&out[..n_out]);
                let buffer = match arena.alloc_slice_with(len, |_| 0u8) {
                    Ok(b) => b,
                    Err(_) => {
                        return sql_fail(sql_err!(
                            sqlstate::PROGRAM_LIMIT_EXCEEDED,
                            "ALTER rewrite exceeds the statement arena"
                        ))
                    }
                };
                rowenc::encode(&out[..n_out], buffer);
                &*buffer
            };
            let (loc, slice) = match storage.heap.append(new_bytes.len()) {
                Ok(x) => x,
                Err(e) => return sql_fail(e),
            };
            slice.copy_from_slice(new_bytes);
            loc
        } else {
            old_loc
        };
        let lsn = storage.bump_lsn();
        if let Err(e) = wal.append(
            lsn,
            &WalOp::Upsert {
                table: new_def.name.as_str(),
                rowid,
                row: storage.heap.get(new_loc),
            },
        ) {
            return sql_fail(e);
        }
        scratch[i] = (rowid, new_loc);
    }

    // Phase 2: swap in memory. Nothing below can fail.
    storage.set_table_def(table_index, new_def);
    if rewrite {
        for i in 0..scratch.len() {
            let (rowid, new_loc) = scratch[i];
            let state = storage
                .table_mut(table_index)
                .rows
                .get_mut(&rowid)
                .expect("row existed in phase 1");
            state.committed = Some(new_loc);
        }
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

pub fn eval_offset_pub(offset: Option<&Expr>, arena: &Arena, params: &[Datum]) -> Result<u64, SqlError> {
    let Some(expression) = offset else {
        return Ok(0);
    };
    match eval(expression, arena, params, &NoColumns)? {
        Datum::Null => Ok(0),
        Datum::Int4(v) if v >= 0 => Ok(v as u64),
        Datum::Int8(v) if v >= 0 => Ok(v as u64),
        Datum::Int4(_) | Datum::Int8(_) => {
            Err(sql_err!("2201X", "OFFSET must not be negative"))
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
                                "42702",
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
        Datum::Int4(v) if v >= 0 => Ok(v as u64),
        Datum::Int8(v) if v >= 0 => Ok(v as u64),
        Datum::Int4(_) | Datum::Int8(_) => Err(sql_err!(
            "2201W",
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
    def: &TableDef,
    schema: &[ColType],
    loc: RowLoc,
    where_clause: Option<&Expr<'a>>,
    arena: &'a Arena,
    params: &[Datum<'a>],
    hooks: &super::eval::EvalHooks<'_, 'a>,
) -> Result<bool, SqlError> {
    let Some(w) = where_clause else {
        return Ok(true);
    };
    let mut values = [Datum::Null; MAX_COLUMNS];
    rowenc::decode(storage.heap.get(loc), schema, &mut values)?;
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
    scratch: &mut FixedVec<(u64, RowLoc)>,
) -> Result<(), SqlError> {
    scratch.clear();
    let table = storage.table(table_index);
    for (&rowid, state) in table.rows.iter() {
        let Some(loc) = state.visible_to(txid) else {
            continue;
        };
        if row_matches(storage, &table.def, schema, loc, where_clause, arena, params, hooks)? {
            scratch.push((rowid, loc)).map_err(|_| {
                sql_err!(
                    sqlstate::PROGRAM_LIMIT_EXCEEDED,
                    "statement touches more than {} rows",
                    scratch.capacity()
                )
            })?;
        }
    }
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
    scratch: &mut FixedVec<(u64, RowLoc)>,
) -> Result<(), SqlError> {
    scratch.clear();
    let table = storage.table(table_index);
    for (&rowid, state) in table.rows.iter() {
        let Some(loc) = state.visible_to(txid) else {
            continue;
        };
        let mut tv = [Datum::Null; MAX_COLUMNS];
        rowenc::decode(storage.heap.get(loc), schema, &mut tv)?;
        let context = RowCtx { def, values: &tv[..def.n_columns] };
        let found = super::query::first_from_match(
            storage, from, txid, where_clause, arena, params, &context, &mut |_| Ok(()),
        )?;
        if found {
            scratch.push((rowid, loc)).map_err(|_| {
                sql_err!(
                    sqlstate::PROGRAM_LIMIT_EXCEEDED,
                    "statement touches more than {} rows",
                    scratch.capacity()
                )
            })?;
        }
    }
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
    let prior = storage.write_pending(table_index, rowid, txn.txid, Some(loc))?;
    if let Err(e) = txn.touch(table_index as u32, rowid, prior) {
        storage.restore_pending(table_index, rowid, txn.txid, prior);
        return Err(e);
    }
    Ok(())
}

fn coerce<'a>(v: Datum<'a>, col: &ColumnMeta, arena: &'a Arena) -> Result<Datum<'a>, SqlError> {
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
        (ColType::Bpchar, TypeMod::Length(n), Datum::Text(s)) => bpchar_fit(s, n, true, arena),
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
        return Err(sql_err!("22001", "value too long for type character({})", n));
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
                return Err(sql_err!(
                    "22001",
                    "value too long for type character varying({})",
                    max
                ));
            }
            Ok(v)
        }
        (_, TypeMod::Length(n), Datum::Text(s)) => bpchar_fit(s, n, false, arena),
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
        return Err(sql_err!("22003", "numeric field overflow"));
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
        return Err(sql_err!("22003", "numeric field overflow"));
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
                "null value in column \"{}\" violates not-null constraint",
                c.name.as_str()
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

