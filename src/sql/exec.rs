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
    CheckConstraint, ColumnMeta, ForeignKey, OwnedDatum, RowLoc, SqlName, Storage, TableDef,
    UniqueKey, MAX_COLUMNS, MAX_INDEX_COLS,
};
use super::txn::TxnState;
use crate::storage::rowenc;
use crate::wal::{Wal, WalOp};

use super::ast::{
    AlterAction, AlterTable, ColumnDef, CreateTable, Delete, DropTable, Expr, FkAction, Insert,
    LikeClause, SelectItem, TableConstraint, Update,
};
use super::eval::{cast_to, compare_datums, eval, sqlstate, ColumnLookup, NoColumns, SqlError};
use super::types::{ColDesc, ColType, Datum, oid};

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

fn build_def(name: &str, columns: &[ColumnDef], arena: &Arena) -> Result<TableDef, SqlError> {
    if columns.len() > MAX_COLUMNS {
        return Err(sql_err!(
            sqlstate::PROGRAM_LIMIT_EXCEEDED,
            "tables can have at most {} columns",
            MAX_COLUMNS
        ));
    }
    let mut def = TableDef {
        name: SqlName::parse(name)?,
        columns: [empty_meta(); MAX_COLUMNS],
        n_columns: columns.len(),
        ..TableDef::empty()
    };
    for (i, c) in columns.iter().enumerate() {
        if columns[..i].iter().any(|prev| prev.name == c.name) {
            return Err(sql_err!(
                "42701",
                "column \"{}\" specified more than once",
                c.name
            ));
        }
        def.columns[i] = build_column(c, arena)?;
    }
    Ok(def)
}

fn empty_meta() -> ColumnMeta {
    ColumnMeta {
        name: SqlName::parse("").expect("empty fits"),
        ctype: ColType::Bool,
        type_mod: -1,
        not_null: false,
        unique: false,
        primary: false,
        auto_increment: false,
        default_value: None,
    }
}

/// Resolves one column definition, evaluating its DEFAULT (which must be a
/// constant) and coercing it to the column type.
fn build_column(c: &ColumnDef, arena: &Arena) -> Result<ColumnMeta, SqlError> {
    let Some(ctype) = ColType::from_sql_name(c.type_name) else {
        return Err(sql_err!(
            sqlstate::UNDEFINED_OBJECT,
            "type \"{}\" does not exist",
            c.type_name
        ));
    };
    let default_value = match c.default {
        None => None,
        Some(expression) => {
            let v = eval(expression, arena, super::eval::NO_PARAMS, &NoColumns).map_err(|_| {
                sql_err!(
                    sqlstate::FEATURE_NOT_SUPPORTED,
                    "DEFAULT must be a constant expression"
                )
            })?;
            let v = cast_to(v, ctype, arena)?;
            let v = apply_typmod(v, ctype, c.type_mod, arena)?;
            Some(OwnedDatum::from_datum(&v)?)
        }
    };
    // serial/bigserial/smallserial are int4/int8/int2 with an auto-increment
    // default and an implicit NOT NULL.
    let auto_increment = matches!(
        c.type_name,
        "serial" | "serial4" | "bigserial" | "serial8" | "smallserial" | "serial2"
    );
    Ok(ColumnMeta {
        name: SqlName::parse(c.name)?,
        ctype,
        type_mod: c.type_mod,
        not_null: c.not_null || auto_increment,
        unique: c.unique,
        primary: c.primary,
        auto_increment,
        default_value,
    })
}

fn fk_action_of(a: FkAction) -> super::super::storage::FkAction {
    use super::super::storage::FkAction as S;
    match a {
        FkAction::NoAction => S::NoAction,
        FkAction::Restrict => S::Restrict,
        FkAction::Cascade => S::Cascade,
        FkAction::SetNull => S::SetNull,
        FkAction::SetDefault => S::SetDefault,
    }
}

/// Resolves a constraint's column names to indices in `def` (42703 if absent).
fn resolve_cols(def: &TableDef, names: &[&str]) -> Result<([u16; MAX_INDEX_COLS], usize), SqlError> {
    if names.len() > MAX_INDEX_COLS {
        return Err(sql_err!(
            sqlstate::PROGRAM_LIMIT_EXCEEDED,
            "a constraint can span at most {} columns",
            MAX_INDEX_COLS
        ));
    }
    let mut out = [0u16; MAX_INDEX_COLS];
    for (i, name) in names.iter().enumerate() {
        let Some(index) = def.column_index(name) else {
            return Err(sql_err!(
                sqlstate::UNDEFINED_COLUMN,
                "column \"{}\" named in key does not exist",
                name
            ));
        };
        out[i] = index as u16;
    }
    Ok((out, names.len()))
}

/// Validates that every column reference in a CHECK predicate names a real
/// column of the table being defined, and that the predicate uses no subquery
/// (which PostgreSQL forbids in CHECK).
fn validate_check_refs(expression: &Expr, def: &TableDef) -> Result<(), SqlError> {
    match expression {
        Expr::WholeRow(t) => {
            return Err(sql_err!(
                "0A000",
                "whole-row reference to \"{}\" is not supported in CHECK",
                t
            ))
        }
        Expr::Column { name, .. } => {
            if def.column_index(name).is_none() {
                return Err(sql_err!(
                    sqlstate::UNDEFINED_COLUMN,
                    "column \"{}\" does not exist",
                    name
                ));
            }
        }
        Expr::Subquery(_) | Expr::InSubquery { .. } | Expr::Exists(_)
        | Expr::ArraySubquery(_) => {
            return Err(sql_err!(
                sqlstate::FEATURE_NOT_SUPPORTED,
                "cannot use subquery in check constraint"
            ));
        }
        Expr::Unary { operand, .. }
        | Expr::Cast { operand, .. }
        | Expr::IsNull { operand, .. } => validate_check_refs(operand, def)?,
        Expr::Binary { left, right, .. } => {
            validate_check_refs(left, def)?;
            validate_check_refs(right, def)?;
        }
        Expr::Call { args, .. } => {
            for a in *args {
                validate_check_refs(a, def)?;
            }
        }
        Expr::InList { operand, list, .. } => {
            validate_check_refs(operand, def)?;
            for a in *list {
                validate_check_refs(a, def)?;
            }
        }
        Expr::Between { operand, low, high, .. } => {
            validate_check_refs(operand, def)?;
            validate_check_refs(low, def)?;
            validate_check_refs(high, def)?;
        }
        Expr::Like { operand, pattern, .. } | Expr::Match { operand, pattern, .. } => {
            validate_check_refs(operand, def)?;
            validate_check_refs(pattern, def)?;
        }
        Expr::Case { operand, whens, otherwise } => {
            if let Some(o) = operand {
                validate_check_refs(o, def)?;
            }
            for (w, t) in *whens {
                validate_check_refs(w, def)?;
                validate_check_refs(t, def)?;
            }
            if let Some(o) = otherwise {
                validate_check_refs(o, def)?;
            }
        }
        Expr::Null
        | Expr::Bool(_)
        | Expr::Int(_)
        | Expr::Float(_)
        | Expr::NumericLit(_)
        | Expr::Str(_)
        | Expr::BitLit(_)
        | Expr::Param(_)
        | Expr::DefaultMarker => {}
        Expr::Array(items) => {
            for e in *items {
                validate_check_refs(e, def)?;
            }
        }
        Expr::Subscript { base, index } => {
            validate_check_refs(base, def)?;
            validate_check_refs(index, def)?;
        }
        Expr::Field { base, .. } => validate_check_refs(base, def)?,
        Expr::AnyAll { operand, array, .. } => {
            validate_check_refs(operand, def)?;
            validate_check_refs(array, def)?;
        }
    }
    Ok(())
}

/// Applies each parsed table constraint to `def`: single-column PK/UNIQUE set
/// column flags; multi-column PK/UNIQUE become entries in `def.uniques`; CHECK
/// predicates and FOREIGN KEYs are validated and recorded.
fn attach_constraints(
    storage: &Storage,
    def: &mut TableDef,
    constraints: &[TableConstraint],
    txid: u32,
    arena: &Arena,
) -> Result<(), SqlError> {
    // A multi-column primary key lives in `uniques`, not on a column flag, so
    // looking only at the columns would miss one a `LIKE ... INCLUDING INDEXES`
    // had already copied in and let the table end up with two.
    let mut has_primary = def.columns().iter().any(|c| c.primary)
        || def.uniques[..def.n_uniques].iter().any(|k| k.is_primary);
    for con in constraints {
        match con {
            TableConstraint::PrimaryKey { name, columns } => {
                if has_primary {
                    return Err(sql_err!(
                        "42P16",
                        "multiple primary keys for table \"{}\" are not allowed",
                        def.name.as_str()
                    ));
                }
                has_primary = true;
                let (indices, n) = resolve_cols(def, columns)?;
                for &column_index in &indices[..n] {
                    def.columns[column_index as usize].not_null = true;
                }
                if n == 1 {
                    def.columns[indices[0] as usize].primary = true;
                    def.columns[indices[0] as usize].unique = true;
                } else {
                    add_unique_key(def, *name, "pkey", &indices, n, true)?;
                }
            }
            TableConstraint::Unique { name, columns } => {
                let (indices, n) = resolve_cols(def, columns)?;
                if n == 1 {
                    def.columns[indices[0] as usize].unique = true;
                } else {
                    add_unique_key(def, *name, "key", &indices, n, false)?;
                }
            }
            TableConstraint::Check { name, expression, text } => {
                validate_check_refs(expression, def)?;
                if text.len() > crate::storage::CHECK_SQL_MAX {
                    return Err(sql_err!(
                        sqlstate::PROGRAM_LIMIT_EXCEEDED,
                        "CHECK predicate is too long (max {} bytes)",
                        crate::storage::CHECK_SQL_MAX
                    ));
                }
                if def.n_checks == crate::storage::MAX_CHECKS {
                    return Err(sql_err!(
                        sqlstate::PROGRAM_LIMIT_EXCEEDED,
                        "a table can have at most {} CHECK constraints",
                        crate::storage::MAX_CHECKS
                    ));
                }
                let constraint_name = match name {
                    Some(n) => SqlName::parse(n)?,
                    None => SqlName::parse(
                        stack_format!(64, "{}_check", def.name.as_str()).as_str(),
                    )?,
                };
                let mut c = CheckConstraint { name: constraint_name, expression: crate::util::StackStr::new() };
                let _ = core::fmt::Write::write_str(&mut c.expression, text);
                if c.expression.is_truncated() {
                    return Err(sql_err!(
                        sqlstate::PROGRAM_LIMIT_EXCEEDED,
                        "CHECK predicate is too long"
                    ));
                }
                def.checks[def.n_checks] = c;
                def.n_checks += 1;
            }
            TableConstraint::ForeignKey {
                name,
                columns,
                parent,
                parent_cols,
                on_delete,
                on_update,
            } => {
                attach_fkey(
                    storage, def, *name, columns, parent, parent_cols, *on_delete, *on_update, txid,
                    arena,
                )?;
            }
        }
    }
    Ok(())
}

/// PostgreSQL's auto-generated constraint name: `<table>_pkey` for a primary
/// key, otherwise `<table>_<col1>_<col2>_<suffix>` over every key column.
fn auto_key_name(
    def: &TableDef,
    columns: &[u16],
    suffix: &str,
    include_cols: bool,
) -> Result<SqlName, SqlError> {
    use core::fmt::Write as _;
    let mut nm = crate::util::StackStr::<64>::new();
    let _ = write!(nm, "{}", def.name.as_str());
    if include_cols {
        for &c in columns {
            let _ = write!(nm, "_{}", def.columns()[c as usize].name.as_str());
        }
    }
    let _ = write!(nm, "_{}", suffix);
    if nm.is_truncated() {
        return Err(sql_err!(
            sqlstate::PROGRAM_LIMIT_EXCEEDED,
            "generated constraint name is too long"
        ));
    }
    SqlName::parse(nm.as_str())
}

fn add_unique_key(
    def: &mut TableDef,
    name: Option<&str>,
    suffix: &str,
    indices: &[u16; MAX_INDEX_COLS],
    n: usize,
    is_primary: bool,
) -> Result<(), SqlError> {
    if def.n_uniques == crate::storage::MAX_UNIQUES {
        return Err(sql_err!(
            sqlstate::PROGRAM_LIMIT_EXCEEDED,
            "a table can have at most {} multi-column key constraints",
            crate::storage::MAX_UNIQUES
        ));
    }
    let kname = match name {
        Some(nm) => SqlName::parse(nm)?,
        // A primary key is `<table>_pkey`; a unique key lists every column.
        None => auto_key_name(def, &indices[..n], suffix, !is_primary)?,
    };
    let mut k = UniqueKey::EMPTY;
    k.name = kname;
    k.columns[..n].copy_from_slice(&indices[..n]);
    k.n_cols = n;
    k.is_primary = is_primary;
    def.uniques[def.n_uniques] = k;
    def.n_uniques += 1;
    Ok(())
}

/// Validates and records a FOREIGN KEY: the parent table must exist and have a
/// PRIMARY KEY or UNIQUE constraint matching the referenced columns, and the
/// child/parent column types must agree.
#[allow(clippy::too_many_arguments)]
fn attach_fkey(
    storage: &Storage,
    def: &mut TableDef,
    name: Option<&str>,
    child_cols: &[&str],
    parent: &str,
    parent_cols: &[&str],
    on_delete: FkAction,
    on_update: FkAction,
    txid: u32,
    _arena: &Arena,
) -> Result<(), SqlError> {
    if def.n_fkeys == crate::storage::MAX_FKEYS {
        return Err(sql_err!(
            sqlstate::PROGRAM_LIMIT_EXCEEDED,
            "a table can have at most {} foreign keys",
            crate::storage::MAX_FKEYS
        ));
    }
    let (child_idxs, n_child) = resolve_cols(def, child_cols)?;

    // The parent may be this very table (self-reference), not yet in storage.
    let self_ref = parent == def.name.as_str();
    let parent_def: TableDef = if self_ref {
        *def
    } else {
        let Some(pi) = storage.find_visible(parent, txid) else {
            return Err(sql_err!(
                sqlstate::UNDEFINED_TABLE,
                "relation \"{}\" does not exist",
                parent
            ));
        };
        storage.table(pi).def
    };

    // Referenced columns default to the parent's primary key.
    let mut pcol_names: [&str; MAX_INDEX_COLS] = [""; MAX_INDEX_COLS];
    let n_parent;
    if parent_cols.is_empty() {
        let (pk, pk_n) = primary_key_cols(&parent_def);
        if pk_n == 0 {
            return Err(sql_err!(
                "42830",
                "there is no primary key for referenced table \"{}\"",
                parent
            ));
        }
        n_parent = pk_n;
        for (i, &column_index) in pk[..pk_n].iter().enumerate() {
            pcol_names[i] = parent_def.columns()[column_index as usize].name.as_str();
        }
    } else {
        n_parent = parent_cols.len();
        pcol_names[..n_parent].copy_from_slice(parent_cols);
    }
    if n_parent != n_child {
        return Err(sql_err!(
            "42830",
            "number of referencing and referenced columns for foreign key disagree"
        ));
    }
    let (parent_idxs, _) = resolve_cols(&parent_def, &pcol_names[..n_parent])?;

    // The referenced columns must be a unique key of the parent (PG 42830).
    if !is_unique_key(&parent_def, &parent_idxs[..n_parent]) {
        return Err(sql_err!(
            "42830",
            "there is no unique constraint matching given keys for referenced table \"{}\"",
            parent
        ));
    }
    // Types must match between each child and parent column.
    for i in 0..n_child {
        let column_type = def.columns()[child_idxs[i] as usize].ctype;
        let parent_type = parent_def.columns()[parent_idxs[i] as usize].ctype;
        if column_type.storage() != parent_type.storage() {
            return Err(sql_err!(
                "42804",
                "foreign key constraint cannot be implemented: column types {} and {} are incompatible",
                column_type.name(),
                parent_type.name()
            ));
        }
    }

    let fname = match name {
        Some(n) => SqlName::parse(n)?,
        None => auto_key_name(def, &child_idxs[..n_child], "fkey", true)?,
    };
    let mut fk = ForeignKey::EMPTY;
    fk.name = fname;
    fk.columns[..n_child].copy_from_slice(&child_idxs[..n_child]);
    fk.n_cols = n_child;
    fk.parent = SqlName::parse(parent)?;
    fk.parent_cols[..n_parent].copy_from_slice(&parent_idxs[..n_parent]);
    fk.n_parent_cols = n_parent;
    fk.on_delete = fk_action_of(on_delete);
    fk.on_update = fk_action_of(on_update);
    def.fkeys[def.n_fkeys] = fk;
    def.n_fkeys += 1;
    Ok(())
}

/// The column indices forming the table's primary key (column flags or a
/// multi-column PRIMARY KEY constraint); the count is 0 if none.
fn primary_key_cols(def: &TableDef) -> ([u16; MAX_INDEX_COLS], usize) {
    let mut out = [0u16; MAX_INDEX_COLS];
    for uk in def.uniques() {
        if uk.is_primary {
            let n = uk.n_cols.min(MAX_INDEX_COLS);
            out[..n].copy_from_slice(&uk.columns()[..n]);
            return (out, n);
        }
    }
    let mut n = 0;
    for (i, c) in def.columns().iter().enumerate() {
        if c.primary {
            out[n] = i as u16;
            n += 1;
        }
    }
    (out, n)
}

/// Whether `columns` (as a set) exactly matches some unique key of the table: a
/// single UNIQUE/PRIMARY column flag, or a multi-column key constraint.
fn is_unique_key(def: &TableDef, columns: &[u16]) -> bool {
    if columns.len() == 1 {
        let c = &def.columns()[columns[0] as usize];
        if c.unique || c.primary {
            return true;
        }
    }
    def.uniques().iter().any(|uk| {
        uk.n_cols == columns.len() && {
            let a = uk.columns();
            columns.iter().all(|c| a.contains(c)) && a.iter().all(|c| columns.contains(c))
        }
    })
}

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

/// Unique/PK enforcement: `values[col]` must not equal that column in any
/// other visible row. Committed conflicts are 23505; a conflicting
/// uncommitted row from another transaction fails fast with 40001.
pub fn check_unique(
    storage: &Storage,
    table_index: usize,
    def: &TableDef,
    schema: &[ColType],
    values: &[Datum],
    self_rowid: Option<u64>,
    txid: u32,
) -> Result<(), SqlError> {
    let any_unique = def.columns().iter().any(|c| c.unique);
    if !any_unique {
        return Ok(());
    }
    let table = storage.table(table_index);
    for (&rowid, state) in table.rows.iter() {
        if Some(rowid) == self_rowid {
            continue;
        }
        // Check both the committed image and any pending image: a commit
        // of either would collide.
        for (loc, pending_of) in [
            (state.committed, None),
            (
                state.pending.and_then(|p| p.loc),
                state.pending.map(|p| p.txid),
            ),
        ] {
            let Some(loc) = loc else { continue };
            let mut other = [Datum::Null; MAX_COLUMNS];
            rowenc::decode(storage.heap.get(loc), schema, &mut other)?;
            for (i, c) in def.columns().iter().enumerate() {
                if !c.unique || values[i].is_null() || other[i].is_null() {
                    continue;
                }
                if compare_datums(&values[i], &other[i])
                    .map(|o| o.is_eq())
                    .unwrap_or(false)
                {
                    if let Some(owner) = pending_of
                        && owner != txid {
                            return Err(sql_err!(
                                "40001",
                                "could not serialize access due to concurrent update"
                            ));
                        }
                    let kind = if c.primary { "pkey" } else { "key" };
                    return Err(sql_err!(
                        "23505",
                        "duplicate key value violates unique constraint \"{}_{}_{}\"",
                        def.name.as_str(),
                        c.name.as_str(),
                        kind
                    ));
                }
            }
        }
    }
    Ok(())
}

/// Enforces both column-level UNIQUE/PRIMARY KEY and every UNIQUE index.
#[allow(clippy::too_many_arguments)]
pub fn check_all_unique(
    storage: &Storage,
    table_index: usize,
    def: &TableDef,
    schema: &[ColType],
    values: &[Datum],
    self_rowid: Option<u64>,
    txid: u32,
) -> Result<(), SqlError> {
    check_unique(storage, table_index, def, schema, values, self_rowid, txid)?;
    check_unique_indexes(storage, table_index, def, schema, values, self_rowid, txid)?;
    check_unique_keys(storage, table_index, def, schema, values, self_rowid, txid)
}

/// Enforces every UNIQUE index on the table: a candidate row conflicts if some
/// other visible row has an equal, all-non-NULL tuple over the index columns
/// (23505; a conflicting uncommitted row from another transaction is 40001).
/// SQL treats NULLs as distinct, so a candidate with any NULL index column is
/// never a conflict.
#[allow(clippy::too_many_arguments)]
pub fn check_unique_indexes(
    storage: &Storage,
    table_index: usize,
    def: &TableDef,
    schema: &[ColType],
    values: &[Datum],
    self_rowid: Option<u64>,
    txid: u32,
) -> Result<(), SqlError> {
    let table_name = def.name.as_str();
    for index in storage.unique_indexes_for(table_name, txid) {
        let icols = &index.columns[..index.n_cols];
        tuple_uniqueness(
            storage, table_index, schema, icols, values, self_rowid, txid, index.name.as_str(),
        )?;
    }
    Ok(())
}

/// Enforces multi-column PRIMARY KEY / UNIQUE table constraints (single-column
/// ones ride the column flags via [`check_unique`]).
#[allow(clippy::too_many_arguments)]
fn check_unique_keys(
    storage: &Storage,
    table_index: usize,
    def: &TableDef,
    schema: &[ColType],
    values: &[Datum],
    self_rowid: Option<u64>,
    txid: u32,
) -> Result<(), SqlError> {
    for uk in def.uniques() {
        tuple_uniqueness(
            storage, table_index, schema, uk.columns(), values, self_rowid, txid, uk.name.as_str(),
        )?;
    }
    Ok(())
}

/// A candidate row conflicts if some other visible row has an equal,
/// all-non-NULL tuple over `columns` (23505; 40001 if the conflicting row is
/// another transaction's uncommitted write). A NULL in any key column of the
/// candidate makes it distinct, never a conflict.
#[allow(clippy::too_many_arguments)]
fn tuple_uniqueness(
    storage: &Storage,
    table_index: usize,
    schema: &[ColType],
    columns: &[u16],
    values: &[Datum],
    self_rowid: Option<u64>,
    txid: u32,
    constraint_name: &str,
) -> Result<(), SqlError> {
    if columns.iter().any(|&c| values[c as usize].is_null()) {
        return Ok(());
    }
    let table = storage.table(table_index);
    for (&rowid, state) in table.rows.iter() {
        if Some(rowid) == self_rowid {
            continue;
        }
        for (loc, pending_of) in [
            (state.committed, None),
            (state.pending.and_then(|p| p.loc), state.pending.map(|p| p.txid)),
        ] {
            let Some(loc) = loc else { continue };
            let mut other = [Datum::Null; MAX_COLUMNS];
            rowenc::decode(storage.heap.get(loc), schema, &mut other)?;
            let all_eq = columns.iter().all(|&c| {
                let column_index = c as usize;
                !other[column_index].is_null()
                    && compare_datums(&values[column_index], &other[column_index])
                        .map(|o| o.is_eq())
                        .unwrap_or(false)
            });
            if all_eq {
                if let Some(owner) = pending_of
                    && owner != txid
                {
                    return Err(sql_err!(
                        "40001",
                        "could not serialize access due to concurrent update"
                    ));
                }
                return Err(sql_err!(
                    "23505",
                    "duplicate key value violates unique constraint \"{}\"",
                    constraint_name
                ));
            }
        }
    }
    Ok(())
}

/// Pre-parsed CHECK predicates for a statement, aligned with `def.checks()`.
type ParsedChecks<'a> = [Option<&'a Expr<'a>>; crate::storage::MAX_CHECKS];

/// Re-parses every stored CHECK predicate once per statement into the arena.
fn parse_checks<'a>(def: &'a TableDef, arena: &'a Arena) -> Result<ParsedChecks<'a>, SqlError> {
    let mut out: ParsedChecks<'a> = [None; crate::storage::MAX_CHECKS];
    for (i, c) in def.checks().iter().enumerate() {
        out[i] = Some(super::parser::parse_expr(c.expression.as_str(), arena)?);
    }
    Ok(out)
}

/// Enforces unique keys, CHECK predicates, and outbound foreign keys for one
/// candidate row about to be stored.
#[allow(clippy::too_many_arguments)]
fn enforce_row_constraints(
    storage: &Storage,
    table_index: usize,
    def: &TableDef,
    schema: &[ColType],
    values: &[Datum],
    self_rowid: Option<u64>,
    txid: u32,
    checks: &ParsedChecks,
    arena: &Arena,
    params: &[Datum],
) -> Result<(), SqlError> {
    check_all_unique(storage, table_index, def, schema, values, self_rowid, txid)?;
    check_row_checks(def, checks, values, arena, params)?;
    check_fk_child(storage, def, values, txid)?;
    Ok(())
}

/// Evaluates each CHECK predicate against the candidate row. A predicate that
/// is FALSE raises 23514; NULL and TRUE both pass, per SQL three-valued logic.
fn check_row_checks(
    def: &TableDef,
    checks: &ParsedChecks,
    values: &[Datum],
    arena: &Arena,
    params: &[Datum],
) -> Result<(), SqlError> {
    let context = RowCtx { def, values };
    for (i, c) in def.checks().iter().enumerate() {
        let Some(expression) = checks[i] else { continue };
        if matches!(eval(expression, arena, params, &context)?, Datum::Bool(false)) {
            return Err(sql_err!(
                "23514",
                "new row for relation \"{}\" violates check constraint \"{}\"",
                def.name.as_str(),
                c.name.as_str()
            ));
        }
    }
    Ok(())
}

/// Enforces this table's outbound foreign keys: each non-NULL referencing tuple
/// must match a row in the parent (MATCH SIMPLE — a NULL in any referencing
/// column satisfies the constraint). Missing referent raises 23503.
fn check_fk_child(
    storage: &Storage,
    def: &TableDef,
    values: &[Datum],
    txid: u32,
) -> Result<(), SqlError> {
    for fk in def.fkeys() {
        if fk.columns().iter().any(|&c| values[c as usize].is_null()) {
            continue;
        }
        let Some(pi) = storage.find_visible(fk.parent.as_str(), txid) else {
            return Err(sql_err!(
                "23503",
                "insert or update on table \"{}\" violates foreign key constraint \"{}\"",
                def.name.as_str(),
                fk.name.as_str()
            ));
        };
        let pdef = storage.table(pi).def;
        let mut pschema = [ColType::Bool; MAX_COLUMNS];
        pdef.schema(&mut pschema);
        if !parent_has_key(
            storage,
            pi,
            &pschema[..pdef.n_columns],
            fk.parent_cols(),
            fk.columns(),
            values,
            txid,
        )? {
            return Err(sql_err!(
                "23503",
                "insert or update on table \"{}\" violates foreign key constraint \"{}\"",
                def.name.as_str(),
                fk.name.as_str()
            ));
        }
    }
    Ok(())
}

/// Whether any row of the parent (visible to `txid`) has, in `parent_cols`, the
/// same tuple the child row carries in `child_cols`.
#[allow(clippy::too_many_arguments)]
fn parent_has_key(
    storage: &Storage,
    parent_index: usize,
    parent_schema: &[ColType],
    parent_cols: &[u16],
    child_cols: &[u16],
    child_values: &[Datum],
    txid: u32,
) -> Result<bool, SqlError> {
    let table = storage.table(parent_index);
    for (_, state) in table.rows.iter() {
        let Some(loc) = state.visible_to(txid) else { continue };
        let mut prow = [Datum::Null; MAX_COLUMNS];
        rowenc::decode(storage.heap.get(loc), parent_schema, &mut prow)?;
        let all_eq = parent_cols.iter().zip(child_cols).all(|(&pc, &cc)| {
            let pv = &prow[pc as usize];
            let cv = &child_values[cc as usize];
            !pv.is_null()
                && compare_datums(cv, pv).map(|o| o.is_eq()).unwrap_or(false)
        });
        if all_eq {
            return Ok(true);
        }
    }
    Ok(false)
}

/// Referential-action cascades can chase foreign keys through many tables
/// (or a cycle); past this depth the statement fails loudly.
const MAX_FK_CASCADE_DEPTH: u32 = 32;

/// After a parent row is deleted (`new_parent` None) or its referenced key
/// updated (Some), applies every referencing foreign key's action:
/// NO ACTION / RESTRICT block (23503); CASCADE deletes or re-keys the
/// referencing rows; SET NULL / SET DEFAULT rewrite the referencing columns
/// (re-checking the child's own constraints). Rewritten or deleted child
/// rows recursively apply their own referential actions. `old_parent` /
/// `new_parent` must not borrow storage (the cascade mutates it) — decode
/// them from arena-copied row bytes.
#[allow(clippy::too_many_arguments)]
fn apply_fk_parent_actions(
    storage: &mut Storage,
    txn: &mut TxnState,
    parent_name: &str,
    old_parent: &[Datum],
    new_parent: Option<&[Datum]>,
    arena: &Arena,
    params: &[Datum],
    depth: u32,
) -> Result<(), SqlError> {
    if depth == 0 {
        return Err(sql_err!(
            "54001",
            "foreign key cascade nested more than {} levels deep",
            MAX_FK_CASCADE_DEPTH
        ));
    }
    for child_index in 0..storage.table_count() {
        if !storage.table(child_index).visible_to(txn.txid) {
            continue;
        }
        let cdef = storage.table(child_index).def;
        let mut cschema = [ColType::Bool; MAX_COLUMNS];
        cdef.schema(&mut cschema);
        let cschema = &cschema[..cdef.n_columns];
        for fk_index in 0..cdef.n_fkeys {
            let fk = cdef.fkeys[fk_index];
            if fk.parent.as_str() != parent_name {
                continue;
            }
            // An update triggers this key's action only when the key changed.
            if let Some(new_parent) = new_parent {
                let changed = fk.parent_cols().iter().any(|&pc| {
                    let (a, b) = (&old_parent[pc as usize], &new_parent[pc as usize]);
                    match (a.is_null(), b.is_null()) {
                        (true, true) => false,
                        (true, false) | (false, true) => true,
                        (false, false) => {
                            !compare_datums(a, b).map(|o| o.is_eq()).unwrap_or(false)
                        }
                    }
                });
                if !changed {
                    continue;
                }
            }
            let action = if new_parent.is_none() { fk.on_delete } else { fk.on_update };

            // Collect the referencing rows first: the rewrites below mutate
            // the row map, so the scan must complete before them.
            let refers = |crow: &[Datum]| {
                !fk.columns().iter().any(|&c| crow[c as usize].is_null())
                    && fk.columns().iter().zip(fk.parent_cols()).all(|(&cc, &pc)| {
                        let (cv, pv) = (&crow[cc as usize], &old_parent[pc as usize]);
                        !pv.is_null()
                            && compare_datums(cv, pv).map(|o| o.is_eq()).unwrap_or(false)
                    })
            };
            let mut n_match = 0usize;
            {
                let table = storage.table(child_index);
                for (_, state) in table.rows.iter() {
                    let Some(loc) = state.visible_to(txn.txid) else { continue };
                    let mut crow = [Datum::Null; MAX_COLUMNS];
                    rowenc::decode(storage.heap.get(loc), cschema, &mut crow)?;
                    if refers(&crow[..cdef.n_columns]) {
                        n_match += 1;
                    }
                }
            }
            if n_match == 0 {
                continue;
            }
            use crate::storage::FkAction as StorageFkAction;
            if matches!(action, StorageFkAction::NoAction | StorageFkAction::Restrict) {
                // NO ACTION raises 23503; RESTRICT the distinct 23001, as
                // PostgreSQL (same message, different SQLSTATE).
                let code =
                    if action == StorageFkAction::Restrict { "23001" } else { "23503" };
                return Err(sql_err!(
                    code,
                    "update or delete on table \"{}\" violates foreign key constraint \"{}\" on table \"{}\"",
                    parent_name,
                    fk.name.as_str(),
                    cdef.name.as_str()
                ));
            }
            let matches: &mut [(u64, &[u8])] = arena
                .alloc_slice_with(n_match, |_| (0u64, &[] as &[u8]))
                .map_err(|_| {
                    sql_err!(
                        sqlstate::PROGRAM_LIMIT_EXCEEDED,
                        "foreign key cascade exceeds the statement arena"
                    )
                })?;
            {
                let mut at = 0usize;
                let table = storage.table(child_index);
                for (rowid, state) in table.rows.iter() {
                    let Some(loc) = state.visible_to(txn.txid) else { continue };
                    let bytes = storage.heap.get(loc);
                    let mut crow = [Datum::Null; MAX_COLUMNS];
                    rowenc::decode(bytes, cschema, &mut crow)?;
                    if refers(&crow[..cdef.n_columns]) {
                        // Copy the row bytes out of the heap so the decoded
                        // values survive the mutations below.
                        let copy = arena.alloc_slice_copy(bytes).map_err(|_| {
                            sql_err!(
                                sqlstate::PROGRAM_LIMIT_EXCEEDED,
                                "foreign key cascade exceeds the statement arena"
                            )
                        })?;
                        matches[at] = (*rowid, &*copy);
                        at += 1;
                    }
                }
            }

            let child_name = cdef.name.as_str();
            for &(rowid, old_bytes) in matches.iter() {
                let mut crow = [Datum::Null; MAX_COLUMNS];
                rowenc::decode(old_bytes, cschema, &mut crow)?;
                let crow = &crow[..cdef.n_columns];
                if new_parent.is_none() && action == StorageFkAction::Cascade {
                    // Cascade the delete: grandchildren first, then this row.
                    apply_fk_parent_actions(
                        storage, txn, child_name, crow, None, arena, params, depth - 1,
                    )?;
                    let prior = storage.write_pending(child_index, rowid, txn.txid, None)?;
                    if let Err(e) = txn.touch(child_index as u32, rowid, prior) {
                        storage.restore_pending(child_index, rowid, txn.txid, prior);
                        return Err(e);
                    }
                    continue;
                }
                // The rewriting actions produce a new child row image.
                let mut new_child = [Datum::Null; MAX_COLUMNS];
                new_child[..cdef.n_columns].copy_from_slice(crow);
                for (&cc, &pc) in fk.columns().iter().zip(fk.parent_cols()) {
                    new_child[cc as usize] = match action {
                        StorageFkAction::Cascade => {
                            new_parent.expect("delete-cascade handled above")[pc as usize]
                        }
                        StorageFkAction::SetNull => Datum::Null,
                        StorageFkAction::SetDefault => cdef.columns()[cc as usize]
                            .default_value
                            .as_ref()
                            .map(|d| d.as_datum())
                            .unwrap_or(Datum::Null),
                        StorageFkAction::NoAction | StorageFkAction::Restrict => {
                            unreachable!("blocking actions handled above")
                        }
                    };
                }
                let new_child = &new_child[..cdef.n_columns];
                check_not_null(&cdef, new_child)?;
                let checks = parse_checks(&cdef, arena)?;
                enforce_row_constraints(
                    storage,
                    child_index,
                    &cdef,
                    cschema,
                    new_child,
                    Some(rowid),
                    txn.txid,
                    &checks,
                    arena,
                    params,
                )?;
                // The child's own referenced keys may have changed — recurse.
                apply_fk_parent_actions(
                    storage,
                    txn,
                    child_name,
                    crow,
                    Some(new_child),
                    arena,
                    params,
                    depth - 1,
                )?;
                let len = rowenc::encoded_len(new_child);
                let out = arena.alloc_slice_with(len, |_| 0u8).map_err(|_| {
                    sql_err!(
                        sqlstate::PROGRAM_LIMIT_EXCEEDED,
                        "foreign key cascade exceeds the statement arena"
                    )
                })?;
                rowenc::encode(new_child, out);
                let (new_loc, slice) = storage.heap.append(out.len())?;
                slice.copy_from_slice(out);
                let prior = storage.write_pending(child_index, rowid, txn.txid, Some(new_loc))?;
                if let Err(e) = txn.touch(child_index as u32, rowid, prior) {
                    storage.restore_pending(child_index, rowid, txn.txid, prior);
                    return Err(e);
                }
            }
        }
    }
    Ok(())
}

/// Whether any visible table has a foreign key referencing `name`.
fn table_is_referenced(storage: &Storage, name: &str, txid: u32) -> bool {
    for column_index in 0..storage.table_count() {
        if !storage.table(column_index).visible_to(txid) {
            continue;
        }
        if storage
            .table(column_index)
            .def
            .fkeys()
            .iter()
            .any(|fk| fk.parent.as_str() == name)
        {
            return true;
        }
    }
    false
}

/// Whether an update to `parent_name` changed any column referenced by some
/// child foreign key (so referential integrity must be re-checked).
fn referenced_key_changed(
    storage: &Storage,
    parent_name: &str,
    old: &[Datum],
    new: &[Datum],
    txid: u32,
) -> bool {
    for column_index in 0..storage.table_count() {
        if !storage.table(column_index).visible_to(txid) {
            continue;
        }
        let cdef = storage.table(column_index).def;
        for fk in cdef.fkeys() {
            if fk.parent.as_str() != parent_name {
                continue;
            }
            for &pc in fk.parent_cols() {
                let i = pc as usize;
                let (a, b) = (&old[i], &new[i]);
                let changed = match (a.is_null(), b.is_null()) {
                    (true, true) => false,
                    (true, false) | (false, true) => true,
                    (false, false) => !compare_datums(a, b).map(|o| o.is_eq()).unwrap_or(false),
                };
                if changed {
                    return true;
                }
            }
        }
    }
    false
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
        None => return sql_fail(undefined_table(statement.name)),
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
        return sql_fail(sql_err!("42P01", "index \"{}\" does not exist", name));
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

/// Result-column names and types, statically inferred. Names borrow the
/// statement (aliases) or the catalog (wildcard columns); `'q` is whichever
/// is shorter at the call site.
pub fn describe_items<'q>(
    items: &[SelectItem<'q>],
    def: Option<&'q TableDef>,
    out: &mut [ColDesc<'q>],
) -> Result<usize, SqlError> {
    let mut n = 0;
    for item in items {
        let mut push = |desc: ColDesc<'q>| -> Result<(), SqlError> {
            if n == out.len() {
                return Err(sql_err!(
                    sqlstate::PROGRAM_LIMIT_EXCEEDED,
                    "select list expands past {} columns",
                    out.len()
                ));
            }
            out[n] = desc;
            n += 1;
            Ok(())
        };
        match item {
            SelectItem::Wildcard => {
                let Some(def) = def else {
                    return Err(sql_err!(
                        sqlstate::SYNTAX_ERROR,
                        "SELECT * requires a FROM clause"
                    ));
                };
                for c in def.columns() {
                    push(ColDesc::of_type(c.name.as_str(), c.ctype))?;
                }
            }
            SelectItem::TableWildcard(q) => {
                let matches = def.is_some_and(|d| d.name.as_str() == *q);
                if !matches {
                    return Err(sql_err!(
                        "42P01",
                        "missing FROM-clause entry for table \"{}\"",
                        q
                    ));
                }
                for c in def.expect("matched").columns() {
                    push(ColDesc::of_type(c.name.as_str(), c.ctype))?;
                }
            }
            SelectItem::RecordStar(base) => {
                describe_record_star(base, def, &mut push)?;
            }
            SelectItem::Expr { expression, alias } => {
                let (mut type_oid, mut typlen) = infer_type_pub(expression, def)?;
                // A bare unknown (string literal / param) resolves to text
                // for output, as PostgreSQL does.
                if type_oid == oid::UNKNOWN {
                    type_oid = oid::TEXT;
                    typlen = -1;
                }
                let name = alias.unwrap_or(derived_name(expression));
                push(ColDesc::new(name, type_oid, typlen))?;
            }
        }
    }
    Ok(n)
}

/// Emits one `ColDesc` per field of a `(record).*` expansion, resolving field
/// names and types at the caller's `'q` lifetime (single-table describe path).
fn describe_record_star<'q>(
    base: &Expr<'q>,
    def: Option<&'q TableDef>,
    push: &mut impl FnMut(ColDesc<'q>) -> Result<(), SqlError>,
) -> Result<(), SqlError> {
    match base {
        Expr::Call { name, args, .. } if name.eq_ignore_ascii_case("row") => {
            let resolver: &dyn ColTypeResolver = match def {
                Some(d) => &DefCols(d),
                None => &NoCols,
            };
            check_row_field_types(base, resolver)?;
            for (i, arg) in args.iter().take(RECORD_FIELD_NAMES.len()).enumerate() {
                let (oid, typlen) = infer_type_pub(arg, def)?;
                push(ColDesc::new(RECORD_FIELD_NAMES[i], oid, typlen))?;
            }
            Ok(())
        }
        Expr::Call { name, .. } if json_each_value_type(name).is_some() => {
            push(ColDesc::of_type("key", ColType::Text))?;
            push(ColDesc::of_type("value", json_each_value_type(name).expect("checked")))?;
            Ok(())
        }
        Expr::WholeRow(table) | Expr::Column { qualifier: None, name: table }
            if def.is_some_and(|d| d.name.as_str() == *table) =>
        {
            for c in def.expect("matched").columns() {
                push(ColDesc::of_type(c.name.as_str(), c.ctype))?;
            }
            Ok(())
        }
        _ => Err(sql_err!(
            "42809",
            "row expansion is not supported on this expression"
        )),
    }
}

/// Maps a type oid back to a ColType (numeric tower + common types).
pub(crate) fn coltype_of_oid(o: i32) -> Option<ColType> {
    Some(match o {
        oid::BOOL => ColType::Bool,
        oid::INT2 => ColType::Int2,
        oid::INT4 => ColType::Int4,
        oid::INT8 => ColType::Int8,
        oid::NUMERIC => ColType::Numeric,
        oid::FLOAT4 => ColType::Float4,
        oid::FLOAT8 => ColType::Float8,
        oid::TEXT => ColType::Text,
        oid::VARCHAR => ColType::Varchar,
        oid::BPCHAR => ColType::Bpchar,
        oid::DATE => ColType::Date,
        oid::TIMESTAMP => ColType::Timestamp,
        oid::TIMESTAMPTZ => ColType::Timestamptz,
        oid::TIME => ColType::Time,
        oid::TIMETZ => ColType::Timetz,
        oid::INTERVAL => ColType::Interval,
        oid::JSON => ColType::Json,
        oid::JSONB => ColType::Jsonb,
        oid::UUID => ColType::Uuid,
        oid::BYTEA => ColType::Bytea,
        oid::INT4MULTIRANGE => ColType::Multirange(super::types::RangeKind::Int4),
        oid::INT8MULTIRANGE => ColType::Multirange(super::types::RangeKind::Int8),
        oid::NUMMULTIRANGE => ColType::Multirange(super::types::RangeKind::Num),
        oid::DATEMULTIRANGE => ColType::Multirange(super::types::RangeKind::Date),
        oid::TSMULTIRANGE => ColType::Multirange(super::types::RangeKind::Ts),
        oid::TSTZMULTIRANGE => ColType::Multirange(super::types::RangeKind::Tstz),
        oid::BIT => ColType::Bit { varying: false },
        oid::VARBIT => ColType::Bit { varying: true },
        // `"char"` (internal single-byte) and `name` appear in catalog columns;
        // treat them as text so catalog-derived tables describe.
        18 | 19 => ColType::Text,
        // Array OIDs (catalog columns like indkey/conkey/indoption are arrays).
        1000 => ColType::Array(super::types::ArrElem::Bool),
        1005 | 1007 => ColType::Array(super::types::ArrElem::Int4),
        1016 => ColType::Array(super::types::ArrElem::Int8),
        1021 | 1022 => ColType::Array(super::types::ArrElem::Float8),
        1009 | 1015 | 1002 | 1014 => ColType::Array(super::types::ArrElem::Text),
        1231 => ColType::Array(super::types::ArrElem::Numeric),
        3904 => ColType::Range(super::types::RangeKind::Int4),
        3926 => ColType::Range(super::types::RangeKind::Int8),
        3906 => ColType::Range(super::types::RangeKind::Num),
        3912 => ColType::Range(super::types::RangeKind::Date),
        3908 => ColType::Range(super::types::RangeKind::Ts),
        3910 => ColType::Range(super::types::RangeKind::Tstz),
        _ => return None,
    })
}

/// Unifies two types by PostgreSQL's numeric preference (int4<int8<numeric<
/// float8); non-numeric or equal types keep the first.
/// The result type (oid, typlen) of an array function that promotes an array's
/// element type to also hold a new scalar element (`array_append`/`prepend`/
/// `replace`). Falls back to the array's own type when either is unknown.
fn array_promoted(array_oid: Option<i32>, elem_oid: Option<i32>) -> (i32, i16) {
    let fallback = (array_oid.unwrap_or(oid::TEXT), -1i16);
    let (Some(ao), Some(eo)) = (array_oid, elem_oid) else {
        return fallback;
    };
    let (Some(ColType::Array(ae)), Some(et)) = (coltype_of_oid(ao), coltype_of_oid(eo)) else {
        return fallback;
    };
    let unified = unify_numeric_tower(ae.to_coltype(), et);
    match super::types::ArrElem::from_coltype(unified) {
        Some(e) => (ColType::Array(e).oid(), -1),
        None => fallback,
    }
}

pub(crate) fn unify_numeric_tower(a: ColType, b: ColType) -> ColType {
    use ColType::*;
    let rank = |t: ColType| match t {
        Int4 => 1, Int8 => 2, Numeric => 3, Float8 => 4, _ => 0,
    };
    let (ra, rb) = (rank(a), rank(b));
    if ra > 0 && rb > 0 {
        if ra >= rb { a } else { b }
    } else {
        a
    }
}

/// PostgreSQL's error when an aggregate has no signature for the argument
/// type (e.g. sum(text), max(boolean)).
fn agg_undefined(name: &str, arg_oid: i32) -> SqlError {
    let table_name = coltype_of_oid(arg_oid).map(|t| t.name()).unwrap_or("unknown");
    sql_err!(
        sqlstate::UNDEFINED_FUNCTION,
        "function {}({}) does not exist",
        name,
        table_name
    )
}

/// A specific output name for an expression, if it has one (parse_target.c
/// FigureColnameInternal): a column ref, a function call, a cast (the type
/// name), or a CASE whose ELSE yields a name. `None` for anything unnamed.
fn name_of<'a>(expression: &Expr<'a>) -> Option<&'a str> {
    match expression {
        Expr::Column { name, .. } => Some(name),
        // `SIMILAR TO` is an operator in PostgreSQL (an anonymous `?column?`),
        // though we desugar it to a `similar_to(...)` call internally.
        Expr::Call { name: "similar_to", .. } => None,
        Expr::Call { name, .. } => Some(name),
        // A cast keeps its operand's name when the operand is a column or
        // function call (`count(*)::int` → `count`); otherwise it takes the
        // target type's name (`'x'::int` → `int4`), matching PostgreSQL.
        Expr::Cast { operand, type_name, .. } => match operand {
            Expr::Column { .. } | Expr::Call { .. } => name_of(operand),
            _ => ColType::from_sql_name(type_name).map(ColType::internal_name),
        },
        Expr::Case { otherwise: Some(e), .. } => name_of(e),
        // An array subscript keeps the base column's name (`m[1]` → `m`).
        Expr::Subscript { base, .. } => name_of(base),
        // `(record).field` is named after the field.
        Expr::Field { field, .. } => Some(field),
        _ => None,
    }
}

/// PostgreSQL's output-column name for a SELECT-list expression: `name_of`
/// with the per-node fallback ("case" for a CASE, else "?column?").
pub fn derived_name<'a>(expression: &Expr<'a>) -> &'a str {
    if let Some(n) = name_of(expression) {
        return n;
    }
    match expression {
        Expr::Case { .. } => "case",
        Expr::WholeRow(t) => t,
        Expr::Exists(_) => "exists",
        Expr::ArraySubquery(_) | Expr::Array(_) => "array",
        // A scalar subquery is named by its single output column.
        Expr::Subquery(s) => match s.items.first() {
            Some(SelectItem::Expr { alias: Some(a), .. }) => a,
            Some(SelectItem::Expr { expression, alias: None }) => derived_name(expression),
            _ => "?column?",
        },
        _ => "?column?",
    }
}

/// Resolves a column reference's type during static analysis. Returns an
/// error for an unknown column (or absent FROM clause).
pub trait ColTypeResolver {
    fn resolve(&self, qualifier: Option<&str>, name: &str) -> Result<ColType, SqlError>;

    /// Whether an unqualified `name` names a FROM item (so a bare reference to
    /// it is a whole-row/record value). Defaults to false.
    fn is_whole_row(&self, _name: &str) -> bool {
        false
    }

    /// If a whole-row reference to `name` is actually a scalar (a
    /// set-returning-function scan's single output column), that column's type.
    /// Defaults to None, meaning the whole-row reference is an anonymous record.
    fn whole_row_scalar_type(&self, _name: &str) -> Option<ColType> {
        None
    }

    /// The columns of the FROM item exposed as `name`, for resolving a
    /// whole-row record's field shape (`(t).c`, `(t).*`). Defaults to None.
    fn table_columns(&self, _name: &str) -> Option<&[ColumnMeta]> {
        None
    }
}

/// Static field names PostgreSQL assigns an anonymous record (`ROW(...)`):
/// `f1`, `f2`, … Indexed 1-based by the caller.
pub const RECORD_FIELD_NAMES: [&str; 64] = [
    "f1", "f2", "f3", "f4", "f5", "f6", "f7", "f8", "f9", "f10", "f11", "f12", "f13", "f14",
    "f15", "f16", "f17", "f18", "f19", "f20", "f21", "f22", "f23", "f24", "f25", "f26", "f27",
    "f28", "f29", "f30", "f31", "f32", "f33", "f34", "f35", "f36", "f37", "f38", "f39", "f40",
    "f41", "f42", "f43", "f44", "f45", "f46", "f47", "f48", "f49", "f50", "f51", "f52", "f53",
    "f54", "f55", "f56", "f57", "f58", "f59", "f60", "f61", "f62", "f63", "f64",
];

/// The value type of `json_each`-family output's `value` column, for callers
/// outside this module (scope-based record-star expansion).
pub fn json_each_value_type_pub(name: &str) -> Option<ColType> {
    json_each_value_type(name)
}

/// The value type of `json_each`-family output's `value` column.
fn json_each_value_type(name: &str) -> Option<ColType> {
    if name.eq_ignore_ascii_case("json_each") {
        Some(ColType::Json)
    } else if name.eq_ignore_ascii_case("jsonb_each") {
        Some(ColType::Jsonb)
    } else if name.eq_ignore_ascii_case("json_each_text") || name.eq_ignore_ascii_case("jsonb_each_text") {
        Some(ColType::Text)
    } else {
        None
    }
}

/// Visits each `(field_name, type)` of a record-valued expression's shape,
/// returning the field count, or None when `base` is not a record whose shape
/// is statically known. Handles `ROW(...)`, a whole-row reference to a FROM
/// table, and the `json_each` family. The visited names borrow only for the
/// call, so callers copy them (into the arena, or into a `ColDesc`).
pub fn record_shape(
    base: &Expr,
    columns: &dyn ColTypeResolver,
    mut visit: impl FnMut(&str, ColType),
) -> Option<usize> {
    match base {
        Expr::Call { name, args, .. } if name.eq_ignore_ascii_case("row") => {
            let n = args.len().min(RECORD_FIELD_NAMES.len());
            for (i, arg) in args[..n].iter().enumerate() {
                let oid = infer_type_res(arg, columns).ok()?.0;
                visit(RECORD_FIELD_NAMES[i], coltype_of_oid(oid).unwrap_or(ColType::Text));
            }
            Some(n)
        }
        Expr::Call { name, .. } if json_each_value_type(name).is_some() => {
            visit("key", ColType::Text);
            visit("value", json_each_value_type(name)?);
            Some(2)
        }
        Expr::WholeRow(table) => shape_from_columns(columns.table_columns(table)?, visit),
        Expr::Column { qualifier: None, name } if columns.is_whole_row(name) => {
            shape_from_columns(columns.table_columns(name)?, visit)
        }
        _ => None,
    }
}

fn shape_from_columns(cols: &[ColumnMeta], mut visit: impl FnMut(&str, ColType)) -> Option<usize> {
    for col in cols {
        visit(col.name.as_str(), col.ctype);
    }
    Some(cols.len())
}

/// PostgreSQL cannot form the composite type of a `ROW(...)` that contains a
/// bare unknown literal, so selecting a field of (or expanding) such a record
/// fails — even for a well-typed sibling field. Mirror that so `(ROW(1,'x')).f1`
/// errors exactly as PostgreSQL does.
pub fn check_row_field_types(base: &Expr, columns: &dyn ColTypeResolver) -> Result<(), SqlError> {
    if let Expr::Call { name, args, .. } = base
        && name.eq_ignore_ascii_case("row")
    {
        for arg in *args {
            if infer_type_res(arg, columns)?.0 == oid::UNKNOWN {
                return Err(sql_err!(
                    "XX000",
                    "failed to find conversion function from unknown to text"
                ));
            }
        }
    }
    Ok(())
}

/// The type of a record's field `field` (for `(base).field`), or an error if
/// `base` is not a record whose shape is known or the field does not exist.
pub fn record_field_type(
    base: &Expr,
    field: &str,
    columns: &dyn ColTypeResolver,
) -> Result<ColType, SqlError> {
    check_row_field_types(base, columns)?;
    let mut found = None;
    let shape = record_shape(base, columns, |name, ctype| {
        if found.is_none() && name.eq_ignore_ascii_case(field) {
            found = Some(ctype);
        }
    });
    if shape.is_none() {
        return Err(sql_err!(
            "42809",
            "field selection is not supported on this expression"
        ));
    }
    found.ok_or_else(|| {
        sql_err!(
            sqlstate::UNDEFINED_COLUMN,
            "could not identify column \"{}\" in record data type",
            field
        )
    })
}

/// No FROM clause: any column reference is an error.
pub struct NoCols;
impl ColTypeResolver for NoCols {
    fn resolve(&self, _q: Option<&str>, name: &str) -> Result<ColType, SqlError> {
        Err(sql_err!(sqlstate::UNDEFINED_COLUMN, "column \"{}\" does not exist", name))
    }
}

/// A single table's columns.
pub struct DefCols<'d>(pub &'d TableDef);
impl ColTypeResolver for DefCols<'_> {
    fn resolve(&self, q: Option<&str>, name: &str) -> Result<ColType, SqlError> {
        if let Some(q) = q
            && q != self.0.name.as_str() {
                return Err(sql_err!("42P01", "missing FROM-clause entry for table \"{}\"", q));
            }
        match self.0.column_index(name) {
            Some(i) => Ok(self.0.columns()[i].ctype),
            None => Err(sql_err!(sqlstate::UNDEFINED_COLUMN, "column \"{}\" does not exist", name)),
        }
    }

    fn is_whole_row(&self, name: &str) -> bool {
        name == self.0.name.as_str()
    }

    fn table_columns(&self, name: &str) -> Option<&[ColumnMeta]> {
        (name == self.0.name.as_str()).then(|| self.0.columns())
    }
}

/// Adapts a runtime row (`ColumnLookup`) to the static `ColTypeResolver` that
/// `infer_type_res` needs, so an expression's declared type can be recovered
/// during evaluation even when its value is NULL.
struct RowCols<'r, 'a>(&'r dyn super::eval::ColumnLookup<'a>);
impl<'a> ColTypeResolver for RowCols<'_, 'a> {
    fn resolve(&self, qualifier: Option<&str>, name: &str) -> Result<ColType, SqlError> {
        self.0.col_type(qualifier, name).ok_or_else(|| {
            sql_err!(sqlstate::UNDEFINED_COLUMN, "column \"{}\" does not exist", name)
        })
    }
}

/// The PostgreSQL type name `pg_typeof` reports for `expression` evaluated
/// against `row`, resolved statically (so a NULL value still names its declared
/// type, matching PostgreSQL). `None` when the static type can't be pinned down
/// (the caller then falls back to the runtime datum's type).
pub fn typeof_static<'a>(
    expression: &Expr,
    row: &dyn super::eval::ColumnLookup<'a>,
) -> Option<&'static str> {
    use super::types::ArrElem;
    let (type_oid, _) = infer_type_res(expression, &RowCols(row)).ok()?;
    Some(match coltype_of_oid(type_oid)? {
        ColType::Array(elem) => match elem {
            ArrElem::Bool => "boolean[]",
            ArrElem::Int4 => "integer[]",
            ArrElem::Int8 => "bigint[]",
            ArrElem::Float8 => "double precision[]",
            ArrElem::Text => "text[]",
            ArrElem::Numeric => "numeric[]",
            ArrElem::Date => "date[]",
            ArrElem::Timestamp => "timestamp without time zone[]",
            ArrElem::Timestamptz => "timestamp with time zone[]",
        },
        other => other.name(),
    })
}

/// Whether two concrete types have a comparison operator, per PostgreSQL:
/// same type, both numeric-tower, or both in the date/time family.
/// Whether an OID names a range type (so range operators apply).
fn is_range_oid(oid: i32) -> bool {
    matches!(coltype_of_oid(oid), Some(ColType::Range(_)))
}

fn is_multirange_oid(oid: i32) -> bool {
    matches!(coltype_of_oid(oid), Some(ColType::Multirange(_)))
}

fn comparable(a: ColType, b: ColType) -> bool {
    use ColType::*;
    if a == b {
        return true;
    }
    let numeric = |t: ColType| matches!(t, Int4 | Int8 | Numeric | Float8);
    let datetime = |t: ColType| matches!(t, Date | Timestamp | Timestamptz);
    let timeofday = |t: ColType| matches!(t, Time | Timetz);
    let bit = |t: ColType| matches!(t, Bit { .. });
    (numeric(a) && numeric(b))
        || (datetime(a) && datetime(b))
        || (timeofday(a) && timeofday(b))
        || (bit(a) && bit(b))
}

fn operator_undefined(l: ColType, operator: &str, r: ColType) -> SqlError {
    sql_err!(
        sqlstate::UNDEFINED_FUNCTION,
        "operator does not exist: {} {} {}",
        l.name(),
        operator,
        r.name()
    )
}

pub fn infer_type_pub(expression: &Expr, def: Option<&TableDef>) -> Result<(i32, i16), SqlError> {
    match def {
        Some(d) => infer_type_res(expression, &DefCols(d)),
        None => infer_type_res(expression, &NoCols),
    }
}

/// Static type inference with operator/aggregate validation, matching
/// PostgreSQL's plan-time analysis: comparisons and arithmetic over
/// incompatible types raise 42883 here, before any row is scanned. String
/// literals and parameters are UNKNOWN and coerce to the other operand.
pub fn infer_type_res(expression: &Expr, columns: &dyn ColTypeResolver) -> Result<(i32, i16), SqlError> {
    let of = |t: ColType| (t.oid(), t.typlen());
    Ok(match expression {
        Expr::Null | Expr::Str(_) | Expr::Param(_) => (oid::UNKNOWN, -2),
        // A whole-row reference is an anonymous record — unless it is a function
        // scan's whole row, which is its single scalar column.
        Expr::WholeRow(t) => match columns.whole_row_scalar_type(t) {
            Some(ty) => of(ty),
            None => (oid::RECORD, -1),
        },
        Expr::BitLit(_) => (oid::BIT, -1),
        Expr::Bool(_) => of(ColType::Bool),
        Expr::Int(v) => {
            if i32::try_from(*v).is_ok() { of(ColType::Int4) } else { of(ColType::Int8) }
        }
        Expr::Float(_) => of(ColType::Float8),
        Expr::NumericLit(_) => of(ColType::Numeric),
        Expr::Column { qualifier, name } => match columns.resolve(*qualifier, name) {
            Ok(t) => of(t),
            // A bare name that is not a column but names a FROM item is a
            // whole-row/record value — except a function scan's whole row,
            // which is its single scalar column.
            Err(e) if qualifier.is_none() && columns.is_whole_row(name) => {
                let _ = e;
                match columns.whole_row_scalar_type(name) {
                    Some(t) => of(t),
                    None => (oid::RECORD, -1),
                }
            }
            Err(e) => return Err(e),
        },
        Expr::Unary { operator, operand } => match operator {
            super::ast::UnaryOp::Not => of(ColType::Bool),
            super::ast::UnaryOp::Neg | super::ast::UnaryOp::BitNot => infer_type_res(operand, columns)?,
        },
        Expr::Binary { operator, left, right } => {
            use super::ast::BinaryOp::*;
            let lo = infer_type_res(left, columns)?.0;
            let ro = infer_type_res(right, columns)?.0;
            let is_bit = |o: i32| matches!(o, oid::BIT | oid::VARBIT);
            match operator {
                Eq | NotEq | Lt | LtEq | Gt | GtEq => {
                    // Unknown coerces; two concrete types must be comparable.
                    if lo != oid::UNKNOWN && ro != oid::UNKNOWN
                        && let (Some(a), Some(b)) = (coltype_of_oid(lo), coltype_of_oid(ro))
                            && !comparable(a, b) {
                                let sym = match operator {
                                    Eq => "=", NotEq => "<>", Lt => "<",
                                    LtEq => "<=", Gt => ">", _ => ">=",
                                };
                                return Err(operator_undefined(a, sym, b));
                            }
                    of(ColType::Bool)
                }
                And | Or | Like | ILike => of(ColType::Bool),
                Contains | ContainedBy | Overlaps | NotRightOf | NotLeftOf | Adjacent => {
                    of(ColType::Bool)
                }
                // Multirange set operators (`+`/`-`/`*`) return a multirange of
                // the same subtype.
                Add | Sub | Mul if is_multirange_oid(lo) || is_multirange_oid(ro) => {
                    (if is_multirange_oid(lo) { lo } else { ro }, -1)
                }
                // Range set operators (`+`/`-`/`*` on ranges) return a range of
                // the same type; shifts on ranges (`<<`/`>>`) return boolean.
                Add | Sub | Mul if is_range_oid(lo) || is_range_oid(ro) => {
                    (if is_range_oid(lo) { lo } else { ro }, -1)
                }
                Shl | Shr if is_range_oid(lo) || is_range_oid(ro) => of(ColType::Bool),
                // `jsonb - key/keys/index` deletes and returns jsonb.
                Sub if lo == oid::JSONB => (oid::JSONB, -1),
                // `||` concatenates arrays when either side is an array (the
                // array type is preserved), otherwise it is text concatenation.
                Concat if coltype_of_oid(lo).is_some_and(|t| matches!(t, ColType::Array(_))) => {
                    (lo, -1)
                }
                Concat if coltype_of_oid(ro).is_some_and(|t| matches!(t, ColType::Array(_))) => {
                    (ro, -1)
                }
                // `^` stays numeric when an operand is numeric (and none is a
                // float); otherwise it is double precision.
                Pow => {
                    if (lo == oid::NUMERIC || ro == oid::NUMERIC)
                        && lo != oid::FLOAT8
                        && ro != oid::FLOAT8
                        && lo != oid::FLOAT4
                        && ro != oid::FLOAT4
                    {
                        of(ColType::Numeric)
                    } else {
                        of(ColType::Float8)
                    }
                }
                // Bit-string concatenation yields varbit; otherwise text.
                Concat => {
                    if lo == oid::JSONB || ro == oid::JSONB {
                        (oid::JSONB, -1)
                    } else if is_bit(lo) || is_bit(ro) {
                        (oid::VARBIT, -1)
                    } else {
                        (oid::TEXT, -1)
                    }
                }
                // `json -> k` keeps the json/jsonb type; `->>` yields text.
                JsonGet | JsonPath => (if lo == oid::JSONB { oid::JSONB } else { oid::JSON }, -1),
                JsonGetText | JsonPathText => (oid::TEXT, -1),
                JsonDeletePath => (oid::JSONB, -1),
                JsonExists | JsonExistsAny | JsonExistsAll => of(ColType::Bool),
                // On bit strings the bitwise/shift operators return a bit
                // string; on integers they keep the wider integer width.
                BitAnd | BitOr | BitXor | Shl | Shr => {
                    if is_bit(lo) || is_bit(ro) {
                        (if lo == oid::VARBIT || ro == oid::VARBIT { oid::VARBIT } else { oid::BIT }, -1)
                    } else if lo == oid::INT8 || ro == oid::INT8 {
                        of(ColType::Int8)
                    } else {
                        of(ColType::Int4)
                    }
                }
                Add | Sub | Mul | Div | Mod => {
                    let numeric = |o: i32| {
                        matches!(o, oid::INT4 | oid::INT8 | oid::NUMERIC | oid::FLOAT8)
                    };
                    let int_like = |o: i32| matches!(o, oid::INT4 | oid::INT8 | oid::UNKNOWN);
                    // Date arithmetic: date - date -> int4; date +/- int -> date;
                    // int + date -> date.
                    if lo == oid::DATE && ro == oid::DATE && matches!(operator, Sub) {
                        return Ok(of(ColType::Int4));
                    }
                    // timestamp - timestamp -> interval.
                    if matches!(operator, Sub)
                        && (lo == oid::TIMESTAMP && ro == oid::TIMESTAMP
                            || lo == oid::TIMESTAMPTZ && ro == oid::TIMESTAMPTZ)
                    {
                        return Ok(of(ColType::Interval));
                    }
                    if lo == oid::DATE && matches!(operator, Add | Sub) && int_like(ro) {
                        return Ok(of(ColType::Date));
                    }
                    if ro == oid::DATE && matches!(operator, Add) && int_like(lo) {
                        return Ok(of(ColType::Date));
                    }
                    // Interval arithmetic: date/timestamp ± interval -> the
                    // timestamp type; interval ± interval -> interval.
                    let is_dt = |o: i32| matches!(o, oid::DATE | oid::TIMESTAMP | oid::TIMESTAMPTZ);
                    if matches!(operator, Add | Sub) {
                        if lo == oid::INTERVAL && ro == oid::INTERVAL {
                            return Ok(of(ColType::Interval));
                        }
                        if is_dt(lo) && ro == oid::INTERVAL {
                            return Ok(of(if lo == oid::TIMESTAMPTZ { ColType::Timestamptz } else { ColType::Timestamp }));
                        }
                        if matches!(operator, Add) && lo == oid::INTERVAL && is_dt(ro) {
                            return Ok(of(if ro == oid::TIMESTAMPTZ { ColType::Timestamptz } else { ColType::Timestamp }));
                        }
                        // A time of day keeps its own type, and its zone; the
                        // result wraps within the day.
                        let time_of_day = |o: i32| matches!(o, oid::TIME | oid::TIMETZ);
                        if time_of_day(lo) && ro == oid::INTERVAL {
                            return Ok(of(if lo == oid::TIMETZ { ColType::Timetz } else { ColType::Time }));
                        }
                        if matches!(operator, Add) && lo == oid::INTERVAL && time_of_day(ro) {
                            return Ok(of(if ro == oid::TIMETZ { ColType::Timetz } else { ColType::Time }));
                        }
                    }
                    // interval * number / number * interval / interval / number.
                    if (matches!(operator, Mul) && lo == oid::INTERVAL && numeric(ro))
                        || (matches!(operator, Mul) && numeric(lo) && ro == oid::INTERVAL)
                        || (matches!(operator, Div) && lo == oid::INTERVAL && numeric(ro))
                    {
                        return Ok(of(ColType::Interval));
                    }
                    let l_ok = lo == oid::UNKNOWN || numeric(lo);
                    let r_ok = ro == oid::UNKNOWN || numeric(ro);
                    if (!l_ok || !r_ok)
                        && let (Some(a), Some(b)) = (coltype_of_oid(lo), coltype_of_oid(ro)) {
                            let sym = match operator {
                                Add => "+", Sub => "-", Mul => "*", Div => "/", _ => "%",
                            };
                            return Err(operator_undefined(a, sym, b));
                        }
                    // Promotion: float8 > numeric > int8 > int4; unknown is
                    // absorbed by the concrete side.
                    if lo == oid::FLOAT8 || ro == oid::FLOAT8 {
                        of(ColType::Float8)
                    } else if lo == oid::NUMERIC || ro == oid::NUMERIC {
                        of(ColType::Numeric)
                    } else if lo == oid::INT8 || ro == oid::INT8 {
                        of(ColType::Int8)
                    } else if lo == oid::UNKNOWN && ro == oid::UNKNOWN {
                        of(ColType::Numeric)
                    } else if lo == oid::UNKNOWN {
                        (ro, coltype_of_oid(ro).map(|t| t.typlen()).unwrap_or(-1))
                    } else if ro == oid::UNKNOWN {
                        (lo, coltype_of_oid(lo).map(|t| t.typlen()).unwrap_or(-1))
                    } else {
                        of(ColType::Int4)
                    }
                }
            }
        }
        Expr::Cast { operand, type_name, .. } => {
            // `regclass` is oid-based: `'relname'::regclass` yields the relation
            // OID (so `attrelid = 'tbl'::regclass` compares OIDs, as pgx and
            // most tools introspect), while `oid::regclass` renders as the name.
            if type_name.eq_ignore_ascii_case("regclass") {
                let src = infer_type_res(operand, columns)?.0;
                return Ok(if src == oid::TEXT || src == oid::UNKNOWN {
                    of(ColType::Int4)
                } else {
                    of(ColType::Text)
                });
            }
            match ColType::from_sql_name(type_name) {
                Some(t) => of(t),
                None => return Err(sql_err!(sqlstate::UNDEFINED_OBJECT, "type \"{}\" does not exist", type_name)),
            }
        }
        Expr::IsNull { .. } => of(ColType::Bool),
        Expr::InList { .. } | Expr::Between { .. } | Expr::Like { .. } | Expr::Match { .. } => of(ColType::Bool),
        Expr::Case { whens, otherwise, .. } => {
            let mut acc: Option<ColType> = None;
            let mut consider = |e: &Expr| -> Result<(), SqlError> {
                let (o, _) = infer_type_res(e, columns)?;
                if let Some(t) = coltype_of_oid(o) {
                    acc = Some(match acc {
                        None => t,
                        Some(prev) => unify_numeric_tower(prev, t),
                    });
                }
                Ok(())
            };
            for (_, result) in whens.iter() {
                consider(result)?;
            }
            if let Some(e) = otherwise {
                consider(e)?;
            }
            match acc {
                Some(t) => of(t),
                None => (oid::UNKNOWN, -2),
            }
        }
        Expr::DefaultMarker => (oid::UNKNOWN, -2),
        // A scalar subquery's type is not known at static-inference time (its
        // body is resolved against storage only at execution); an array-from-
        // subquery is likewise unknown here. Both carry their real type in the
        // pre-evaluated datum.
        Expr::Subquery(_) | Expr::ArraySubquery(_) => (oid::UNKNOWN, -2),
        // `x IN (subquery)` and EXISTS are predicates: their result is boolean.
        Expr::InSubquery { .. } | Expr::Exists(_) => of(ColType::Bool),
        Expr::AnyAll { .. } => of(ColType::Bool),
        Expr::Array(items) => {
            // An unknown-typed element (a bare string literal) makes the array
            // text[], as PostgreSQL coerces it; only a concrete element type
            // narrows it further.
            let element = items
                .first()
                .and_then(|e| infer_type_res(e, columns).ok())
                .and_then(|(o, _)| coltype_of_oid(o))
                .and_then(super::types::ArrElem::from_coltype)
                .unwrap_or(super::types::ArrElem::Text);
            of(ColType::Array(element))
        }
        Expr::Subscript { base, .. } => {
            match coltype_of_oid(infer_type_res(base, columns)?.0) {
                Some(ColType::Array(e)) => of(e.to_coltype()),
                _ => (oid::UNKNOWN, -2),
            }
        }
        // `(record).field`: the field's type from the record's shape. When the
        // shape is not a statically known record (a `_pg_expandarray` result,
        // reached directly or through a derived-table column — the shape driver
        // introspection relies on), fall back to int4, matching its `.x`/`.n`
        // ordinal fields; a *known* record with a missing field still errors.
        Expr::Field { base, field } => match record_field_type(base, field, columns) {
            Ok(t) => of(t),
            Err(e) if e.sqlstate == "42809" => of(ColType::Int4),
            Err(e) => return Err(e),
        },
        Expr::Call { name, args, order_by, .. } => match *name {
            // Catalog-introspection helpers (for psql \d).
            "pg_get_userbyid" | "format_type" | "pg_get_expr" | "pg_get_indexdef"
            | "pg_get_constraintdef" | "pg_get_viewdef" | "pg_get_functiondef"
            | "col_description" | "obj_description" | "shobj_description"
            | "pg_encoding_to_char" | "array_to_string"
            | "pg_get_statisticsobjdef_columns" => (oid::TEXT, -1),
            "pg_table_is_visible" | "pg_type_is_visible" | "pg_function_is_visible"
            | "has_table_privilege" | "has_column_privilege" | "has_schema_privilege"
            | "pg_relation_is_publishable" => {
                of(ColType::Bool)
            }
            "array_length" | "cardinality" | "array_upper" | "array_lower" | "array_ndims" => {
                of(ColType::Int4)
            }
            "array_dims" => of(ColType::Text),
            "array_to_json" => of(ColType::Json),
            // Array-manipulation functions keep the array argument's type, but
            // promote its element type to hold a wider new/replacement element
            // (PostgreSQL's polymorphic anyarray/anyelement resolution).
            "array_append" => {
                let array_oid = args.first().map(|a| infer_type_res(a, columns)).transpose()?.map(|t| t.0);
                let elem_oid = args.get(1).map(|a| infer_type_res(a, columns)).transpose()?.map(|t| t.0);
                array_promoted(array_oid, elem_oid)
            }
            "array_prepend" => {
                let elem_oid = args.first().map(|a| infer_type_res(a, columns)).transpose()?.map(|t| t.0);
                let array_oid = args.get(1).map(|a| infer_type_res(a, columns)).transpose()?.map(|t| t.0);
                array_promoted(array_oid, elem_oid)
            }
            "array_replace" => {
                let array_oid = args.first().map(|a| infer_type_res(a, columns)).transpose()?.map(|t| t.0);
                let to_oid = args.get(2).map(|a| infer_type_res(a, columns)).transpose()?.map(|t| t.0);
                array_promoted(array_oid, to_oid)
            }
            "array_cat" => {
                let a_oid = args.first().map(|a| infer_type_res(a, columns)).transpose()?.map(|t| t.0);
                let b_oid = args.get(1).map(|a| infer_type_res(a, columns)).transpose()?.map(|t| t.0);
                // Element-type promotion across the two arrays.
                match (a_oid.and_then(coltype_of_oid), b_oid.and_then(coltype_of_oid)) {
                    (Some(ColType::Array(ae)), Some(ColType::Array(be))) => {
                        let e = unify_numeric_tower(ae.to_coltype(), be.to_coltype());
                        of(ColType::Array(super::types::ArrElem::from_coltype(e).unwrap_or(ae)))
                    }
                    _ => (a_oid.unwrap_or(oid::TEXT), -1),
                }
            }
            "array_remove" | "trim_array" => {
                args.first().map(|a| infer_type_res(a, columns)).transpose()?.unwrap_or((oid::TEXT, -1))
            }
            "pg_partition_ancestors" | "pg_partition_root" | "pg_partition_tree" => {
                args.first().map(|a| infer_type_res(a, columns)).transpose()?.unwrap_or((oid::INT4, 4))
            }
            // Window-only functions.
            "row_number" | "rank" | "dense_rank" | "ntile" => of(ColType::Int8),
            "percent_rank" | "cume_dist" => of(ColType::Float8),
            "lag" | "lead" | "first_value" | "last_value" | "nth_value" => args
                .first()
                .map(|a| infer_type_res(a, columns))
                .transpose()?
                .unwrap_or_else(|| of(ColType::Int8)),
            "count" => of(ColType::Int8),
            "row_to_json" | "to_json" | "json_build_object" | "json_build_array" => {
                of(ColType::Json)
            }
            "to_jsonb" | "jsonb_build_object" | "jsonb_build_array" => of(ColType::Jsonb),
            "row" => (oid::RECORD, -1),
            "sum" | "avg" => {
                let a = args.first().map(|a| infer_type_res(a, columns)).transpose()?.map(|t| t.0);
                match a {
                    Some(oid::INT4) if *name == "sum" => of(ColType::Int8),
                    Some(oid::INT4) | Some(oid::INT8) | Some(oid::NUMERIC) => of(ColType::Numeric),
                    Some(oid::FLOAT8) => of(ColType::Float8),
                    Some(oid::UNKNOWN) | None => of(ColType::Numeric),
                    Some(other) => return Err(agg_undefined(name, other)),
                }
            }
            "min" | "max" => {
                let t = args.first().map(|a| infer_type_res(a, columns)).transpose()?;
                if let Some((o, _)) = t
                    && (o == oid::BOOL || o == oid::UUID) {
                        return Err(agg_undefined(name, o));
                    }
                t.unwrap_or_else(|| of(ColType::Int8))
            }
            // Functions returning the common type of their arguments (numeric
            // tower: float8 > numeric > int8 > int4), so a NULL of a wider type
            // still widens the result — matching PostgreSQL and the runtime
            // promotion in `greatest`/`least`.
            "greatest" | "least" => {
                let rank = |o: i32| {
                    if o == oid::FLOAT8 || o == oid::FLOAT4 {
                        4
                    } else if o == oid::NUMERIC {
                        3
                    } else if o == oid::INT8 {
                        2
                    } else if o == oid::INT4 {
                        1
                    } else {
                        0
                    }
                };
                let mut best: Option<(i32, i16)> = None;
                for a in args.iter() {
                    let t = infer_type_res(a, columns)?;
                    best = Some(match best {
                        None => t,
                        Some(p) => {
                            if rank(t.0) > rank(p.0) {
                                t
                            } else {
                                p
                            }
                        }
                    });
                }
                best.unwrap_or(of(ColType::Int8))
            }
            // `abs`/`nullif` take their first argument's type. `coalesce`
            // unifies across all of them, so an untyped NULL in front must not
            // decide the result: `coalesce(NULL, 1)` is integer, not text.
            "coalesce" | "abs" | "nullif" => {
                let mut chosen = None;
                for a in args.iter() {
                    let t = infer_type_res(a, columns)?;
                    if t.0 != oid::UNKNOWN {
                        chosen = Some(t);
                        break;
                    }
                    if !name.eq_ignore_ascii_case("coalesce") {
                        break;
                    }
                }
                match chosen {
                    Some(t) => t,
                    None if args.is_empty() => of(ColType::Int8),
                    // All arguments untyped: PostgreSQL resolves the unknown
                    // to text, exactly as it does for a bare literal.
                    None if name.eq_ignore_ascii_case("coalesce") => of(ColType::Text),
                    None => infer_type_res(args[0], columns)?,
                }
            }
            "length" | "char_length" | "character_length" | "octet_length" | "strpos"
            | "position" | "ascii" => of(ColType::Int4),
            // Math: sqrt/exp/ln/power stay numeric for a numeric argument (and
            // no float argument outranking it), else double; floor/ceil/trunc/
            // round/sign are numeric for a numeric argument and double
            // otherwise; mod returns the integer type of its arguments.
            "sqrt" | "exp" | "ln" | "power" | "pow" | "log" | "log10" => {
                let mut numeric = false;
                let mut float = false;
                for a in args.iter() {
                    match infer_type_res(a, columns)?.0 {
                        oid::NUMERIC => numeric = true,
                        oid::FLOAT8 | oid::FLOAT4 => float = true,
                        _ => {}
                    }
                }
                if numeric && !float { of(ColType::Numeric) } else { of(ColType::Float8) }
            }
            "div" | "trim_scale" | "to_number" => of(ColType::Numeric),
            "scale" | "min_scale" | "width_bucket" | "regexp_count" | "regexp_instr"
            | "array_position" | "jsonb_array_length" | "json_array_length"
            | "num_nonnulls" | "num_nulls" => of(ColType::Int4),
            "array_positions" => of(ColType::Array(super::types::ArrElem::Int4)),
            // array_fill returns an array of its value argument's element type.
            "array_fill" => {
                let elem = args
                    .first()
                    .map(|a| infer_type_res(a, columns))
                    .transpose()?
                    .and_then(|(oid, _)| coltype_of_oid(oid))
                    .and_then(super::types::ArrElem::from_coltype)
                    .unwrap_or(super::types::ArrElem::Int4);
                of(ColType::Array(elem))
            }
            "jsonb_typeof" | "json_typeof" | "json_extract_path_text"
            | "jsonb_extract_path_text" => of(ColType::Text),
            "json_extract_path" => of(ColType::Json),
            "jsonb_extract_path" => of(ColType::Jsonb),
            "regexp_substr" => of(ColType::Text),
            "regexp_like" => of(ColType::Bool),
            "regexp_split_to_array" | "string_to_array" => {
                of(ColType::Array(super::types::ArrElem::Text))
            }
            "format" | "overlay" | "regexp_replace" => of(ColType::Text),
            "floor" | "ceil" | "ceiling" | "sign" => {
                let a = args.first().map(|a| infer_type_res(a, columns)).transpose()?.map(|t| t.0);
                if a == Some(oid::NUMERIC) { of(ColType::Numeric) } else { of(ColType::Float8) }
            }
            "round" | "trunc" => {
                if args.len() == 2 {
                    of(ColType::Numeric)
                } else {
                    let a = args.first().map(|a| infer_type_res(a, columns)).transpose()?.map(|t| t.0);
                    if a == Some(oid::NUMERIC) { of(ColType::Numeric) } else { of(ColType::Float8) }
                }
            }
            "mod" | "gcd" | "lcm" => {
                let a = args.first().map(|a| infer_type_res(a, columns)).transpose()?.map(|t| t.0);
                let b = args.get(1).map(|a| infer_type_res(a, columns)).transpose()?.map(|t| t.0);
                // `mod` keeps a numeric operand's type; gcd/lcm are integer-only.
                if *name == "mod" && (a == Some(oid::NUMERIC) || b == Some(oid::NUMERIC)) {
                    of(ColType::Numeric)
                } else if a == Some(oid::INT8) || b == Some(oid::INT8) {
                    of(ColType::Int8)
                } else {
                    of(ColType::Int4)
                }
            }
            "to_hex" | "md5" | "to_char" | "pg_size_pretty" => of(ColType::Text),
            "factorial" => of(ColType::Numeric),
            "bit_length" => of(ColType::Int4),
            "starts_with" => of(ColType::Bool),
            "cbrt" | "sin" | "cos" | "tan" | "cot" | "asin" | "acos" | "atan" | "atan2" | "sinh"
            | "cosh" | "tanh" | "asinh" | "acosh" | "atanh" | "degrees" | "radians" | "pi" => {
                of(ColType::Float8)
            }
            "bool_and" | "bool_or" | "every" => of(ColType::Bool),
            // Bitwise aggregates preserve the argument's (integer or bit) type.
            "bit_and" | "bit_or" | "bit_xor" => {
                args.first().map(|a| infer_type_res(a, columns)).transpose()?.unwrap_or(of(ColType::Int4))
            }
            // Single-argument variance/stddev mirror the input class: numeric for
            // integer/numeric inputs, double precision for float8 (PostgreSQL's
            // aggregate signatures).
            "var_pop" | "var_samp" | "variance" | "stddev_pop" | "stddev_samp" | "stddev" => {
                let a = args.first().map(|a| infer_type_res(a, columns)).transpose()?.map(|t| t.0);
                match a {
                    Some(oid::FLOAT8) | Some(oid::FLOAT4) => of(ColType::Float8),
                    _ => of(ColType::Numeric),
                }
            }
            // The two-argument regression/covariance/correlation aggregates take
            // and return double precision; regr_count returns bigint.
            "corr" | "covar_pop" | "covar_samp" | "regr_slope" | "regr_intercept" | "regr_r2"
            | "regr_avgx" | "regr_avgy" | "regr_sxx" | "regr_syy" | "regr_sxy" => {
                of(ColType::Float8)
            }
            "regr_count" => of(ColType::Int8),
            "string_agg" => of(ColType::Text),
            "array_agg" => {
                // Element type from the argument; the result is elem[].
                let elem = args
                    .first()
                    .map(|a| infer_type_res(a, columns))
                    .transpose()?
                    .and_then(|(oid, _)| coltype_of_oid(oid))
                    .and_then(super::types::ArrElem::from_coltype)
                    .unwrap_or(super::types::ArrElem::Int4);
                of(ColType::Array(elem))
            }
            // Ordered-set aggregates: percentile_cont yields double precision
            // (numeric for a numeric input); percentile_disc/mode yield the
            // WITHIN GROUP input type.
            "percentile_cont" | "percentile_disc" | "mode" => {
                let input = order_by
                    .first()
                    .map(|o| infer_type_res(o.expression, columns))
                    .transpose()?
                    .map(|t| t.0);
                match *name {
                    "percentile_cont" if input == Some(oid::NUMERIC) => of(ColType::Numeric),
                    "percentile_cont" => of(ColType::Float8),
                    _ => match input.and_then(coltype_of_oid) {
                        Some(t) => of(t),
                        None => (oid::UNKNOWN, -2),
                    },
                }
            }
            "extract" => of(ColType::Numeric),
            "date_part" => of(ColType::Float8),
            // Paren-less temporal functions carry a proper type so date/time
            // arithmetic (e.g. `current_date - 1`) type-checks correctly.
            "to_date" => of(ColType::Date),
            "to_timestamp" => of(ColType::Timestamptz),
            "generate_series" => {
                let a = args.first().map(|a| infer_type_res(a, columns)).transpose()?.map(|t| t.0);
                if a == Some(oid::INT8) { of(ColType::Int8) } else { of(ColType::Int4) }
            }
            "unnest" => {
                // The element type of the array argument.
                match args.first().map(|a| infer_type_res(a, columns)).transpose()?.map(|t| t.0) {
                    Some(o) => match coltype_of_oid(o) {
                        Some(ColType::Array(element)) => of(element.to_coltype()),
                        _ => of(ColType::Text),
                    },
                    None => of(ColType::Text),
                }
            }
            // regexp_matches returns each match's capture groups as text[].
            "regexp_matches" => of(ColType::Array(super::types::ArrElem::Text)),
            "regexp_split_to_table" => of(ColType::Text),
            "generate_subscripts" => of(ColType::Int4),
            "jsonb_object_keys" | "json_object_keys" | "jsonb_array_elements_text"
            | "json_array_elements_text" => of(ColType::Text),
            "jsonb_array_elements" => of(ColType::Jsonb),
            "json_array_elements" => of(ColType::Json),
            // The `each` family yields a `(key, value)` composite per member.
            "json_each" | "jsonb_each" | "json_each_text" | "jsonb_each_text" => {
                (oid::RECORD, -1)
            }
            "grouping" => of(ColType::Int4),
            "make_date" => of(ColType::Date),
            "make_time" => of(ColType::Time),
            "make_timestamp" => of(ColType::Timestamp),
            "make_timestamptz" => of(ColType::Timestamptz),
            "isfinite" => of(ColType::Bool),
            // Encoding / hashing / bytea manipulation.
            "sha224" | "sha256" | "sha384" | "sha512" | "decode" | "set_byte" | "set_bit"
            | "convert_to" => of(ColType::Bytea),
            "encode" | "convert_from" | "quote_ident" | "quote_literal" | "quote_nullable" => {
                of(ColType::Text)
            }
            "get_byte" | "get_bit" => of(ColType::Int4),
            "overlaps" => of(ColType::Bool),
            "bit_count" => of(ColType::Int8),
            "parse_ident" => of(ColType::Array(super::types::ArrElem::Text)),
            // date_bin returns the type of its source timestamp (arg 1).
            "date_bin" => {
                let src = args.get(1).map(|a| infer_type_res(a, columns)).transpose()?.map(|t| t.0);
                if src == Some(oid::TIMESTAMPTZ) {
                    of(ColType::Timestamptz)
                } else {
                    of(ColType::Timestamp)
                }
            }
            "age" | "justify_hours" | "justify_days" | "justify_interval" | "make_interval" => {
                of(ColType::Interval)
            }
            // timezone(zone, ts) == ts AT TIME ZONE zone: timestamptz <-> timestamp.
            "timezone" => {
                let arg = args.get(1).map(|a| infer_type_res(a, columns)).transpose()?.map(|t| t.0);
                match arg {
                    Some(oid::TIMESTAMPTZ) => of(ColType::Timestamp),
                    _ => of(ColType::Timestamptz),
                }
            }
            "int4range" | "int8range" | "numrange" | "daterange" | "tsrange" | "tstzrange" => {
                of(ColType::Range(super::types::RangeKind::from_name(name).expect("range name")))
            }
            "int4multirange" | "int8multirange" | "nummultirange" | "datemultirange"
            | "tsmultirange" | "tstzmultirange" => of(ColType::Multirange(
                super::types::RangeKind::from_multirange_name(name).expect("multirange name"),
            )),
            "similar_to" | "isempty" | "lower_inc" | "upper_inc" | "lower_inf" | "upper_inf" => of(ColType::Bool),
            "range_merge" => {
                // Same range type as its arguments.
                match args.first().map(|a| infer_type_res(a, columns)).transpose()?.map(|t| t.0) {
                    Some(o) if is_range_oid(o) => (o, -1),
                    _ => (oid::TEXT, -1),
                }
            }
            "lower" | "upper" => {
                // A range argument yields its element type; otherwise text.
                match args.first().map(|a| infer_type_res(a, columns)).transpose()?.map(|t| t.0) {
                    Some(o) => match coltype_of_oid(o) {
                        Some(ColType::Range(kind)) | Some(ColType::Multirange(kind)) => {
                            of(kind.elem_type())
                        }
                        _ => (oid::TEXT, -1),
                    },
                    None => (oid::TEXT, -1),
                }
            }
            "current_date" => of(ColType::Date),
            "current_time" => of(ColType::Timetz),
            "localtime" => of(ColType::Time),
            "localtimestamp" => of(ColType::Timestamp),
            "now" | "current_timestamp" | "transaction_timestamp" | "statement_timestamp"
            | "clock_timestamp" => of(ColType::Timestamptz),
            "date_trunc" => {
                // Returns the timestamp type of its second argument.
                let a = args.get(1).map(|a| infer_type_res(a, columns)).transpose()?.map(|t| t.0);
                if a == Some(oid::TIMESTAMPTZ) {
                    of(ColType::Timestamptz)
                } else {
                    of(ColType::Timestamp)
                }
            }
            // The remaining implemented functions (trim family, substr, replace,
            // repeat, reverse, left, right, concat[_ws], initcap, chr, ...) and
            // any not-yet-modeled function default to text.
            _ => (oid::TEXT, -1),
        },
    })
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

/// Tagged, order-preserving-for-equality encoding of a projected row:
/// per value, a tag byte plus a fixed or length-prefixed payload.
pub fn encode_projected_pub<'a>(values: &[Datum], arena: &'a Arena) -> Result<&'a [u8], SqlError> {
    let mut len = 1usize;
    for v in values {
        len += projected_value_len(v);
    }
    let out = arena.alloc_slice_with(len, |_| 0u8).map_err(|_| {
        sql_err!(
            sqlstate::PROGRAM_LIMIT_EXCEEDED,
            "DISTINCT row exceeds the statement arena"
        )
    })?;
    out[0] = values.len() as u8;
    let mut at = 1usize;
    for v in values {
        at += write_projected_value(v, &mut out[at..]);
    }
    Ok(&*out)
}

/// The projected-encoding byte length of one value (tag + payload).
pub fn projected_value_len(v: &Datum) -> usize {
    1 + match v {
        Datum::Null => 0,
        Datum::Bool(_) => 1,
        Datum::Int4(_) | Datum::Date(_) => 4,
        Datum::Int8(_)
        | Datum::Float8(_)
        | Datum::Timestamp(_)
        | Datum::Timestamptz(_)
        | Datum::Time(_) => 8,
        Datum::Timetz(..) => 12,
        Datum::Interval(_) => 16,
        Datum::Uuid(_) => 16,
        Datum::Text(s) => 4 + s.len(),
        Datum::Json { text, .. } => 5 + text.len(),
        Datum::Array { raw, .. } => 6 + raw.len(),
        Datum::Bytea(b) => 4 + b.len(),
        Datum::Numeric(nm) => 7 + nm.digits.len(),
        Datum::Range { text, .. } => 5 + text.len(),
        Datum::Bit { bits, .. } => 5 + bits.len(),
        Datum::Multirange { text, .. } => 5 + text.len(),
        // A record is stored as its rendered text (decode has no arena to
        // rebuild the field slice); the column's RECORD type comes from the
        // describe pass, so output is unaffected.
        Datum::Record(_) => 4 + record_text_len(v),
    }
}

/// The byte length of a value's `Display` output (no allocation).
fn record_text_len(v: &Datum) -> usize {
    use core::fmt::Write as _;
    struct Counter(usize);
    impl core::fmt::Write for Counter {
        fn write_str(&mut self, s: &str) -> core::fmt::Result {
            self.0 += s.len();
            Ok(())
        }
    }
    let mut c = Counter(0);
    let _ = write!(c, "{v}");
    c.0
}

/// Writes one value's tag+payload into `out[0..]` (already sized by
/// `projected_value_len`), returning the bytes written. Shared by the
/// top-level encoder and a record's nested fields.
fn write_projected_value(v: &Datum, out: &mut [u8]) -> usize {
    match v {
        Datum::Null => {
            out[0] = 0;
            1
        }
        Datum::Bool(b) => {
            out[0] = 1;
            out[1] = u8::from(*b);
            2
        }
        Datum::Int4(x) => {
            out[0] = 2;
            out[1..5].copy_from_slice(&x.to_le_bytes());
            5
        }
        Datum::Int8(x) => {
            out[0] = 3;
            out[1..9].copy_from_slice(&x.to_le_bytes());
            9
        }
        Datum::Float8(x) => {
            out[0] = 4;
            out[1..9].copy_from_slice(&x.to_bits().to_le_bytes());
            9
        }
        Datum::Text(str_value) => {
            out[0] = 5;
            out[1..5].copy_from_slice(&(str_value.len() as u32).to_le_bytes());
            out[5..5 + str_value.len()].copy_from_slice(str_value.as_bytes());
            5 + str_value.len()
        }
        Datum::Date(x) => {
            out[0] = 6;
            out[1..5].copy_from_slice(&x.to_le_bytes());
            5
        }
        Datum::Timestamp(x) => {
            out[0] = 7;
            out[1..9].copy_from_slice(&x.to_le_bytes());
            9
        }
        Datum::Timestamptz(x) => {
            out[0] = 8;
            out[1..9].copy_from_slice(&x.to_le_bytes());
            9
        }
        Datum::Time(x) => {
            out[0] = 12;
            out[1..9].copy_from_slice(&x.to_le_bytes());
            9
        }
        Datum::Timetz(x, zone) => {
            out[0] = 20;
            out[1..9].copy_from_slice(&x.to_le_bytes());
            out[9..13].copy_from_slice(&zone.to_le_bytes());
            13
        }
        Datum::Interval(interval) => {
            out[0] = 13;
            out[1..5].copy_from_slice(&interval.months.to_le_bytes());
            out[5..9].copy_from_slice(&interval.days.to_le_bytes());
            out[9..17].copy_from_slice(&interval.micros.to_le_bytes());
            17
        }
        Datum::Json { text, jsonb } => {
            out[0] = 14;
            out[1] = u8::from(*jsonb);
            out[2..6].copy_from_slice(&(text.len() as u32).to_le_bytes());
            out[6..6 + text.len()].copy_from_slice(text.as_bytes());
            6 + text.len()
        }
        Datum::Array { element, raw } => {
            out[0] = 15;
            out[1] = element.code();
            out[2..6].copy_from_slice(&(raw.len() as u32).to_le_bytes());
            out[6..6 + raw.len()].copy_from_slice(raw);
            6 + raw.len()
        }
        Datum::Uuid(b) => {
            out[0] = 9;
            out[1..17].copy_from_slice(b);
            17
        }
        Datum::Bytea(b) => {
            out[0] = 10;
            out[1..5].copy_from_slice(&(b.len() as u32).to_le_bytes());
            out[5..5 + b.len()].copy_from_slice(b);
            5 + b.len()
        }
        Datum::Numeric(nm) => {
            out[0] = 11;
            out[1] = match nm.sign {
                crate::sql::numeric::Sign::Pos => 0,
                crate::sql::numeric::Sign::Neg => 1,
                crate::sql::numeric::Sign::NaN => 2,
            };
            out[2..4].copy_from_slice(&nm.weight.to_le_bytes());
            out[4..6].copy_from_slice(&nm.dscale.to_le_bytes());
            out[6..8].copy_from_slice(&(nm.ndigits() as u16).to_le_bytes());
            out[8..8 + nm.digits.len()].copy_from_slice(nm.digits);
            8 + nm.digits.len()
        }
        Datum::Range { text, kind } => {
            out[0] = 16;
            out[1] = kind.code();
            out[2..6].copy_from_slice(&(text.len() as u32).to_le_bytes());
            out[6..6 + text.len()].copy_from_slice(text.as_bytes());
            6 + text.len()
        }
        Datum::Bit { bits, varying } => {
            out[0] = 17;
            out[1] = u8::from(*varying);
            out[2..6].copy_from_slice(&(bits.len() as u32).to_le_bytes());
            out[6..6 + bits.len()].copy_from_slice(bits.as_bytes());
            6 + bits.len()
        }
        Datum::Multirange { text, kind } => {
            out[0] = 18;
            out[1] = kind.code();
            out[2..6].copy_from_slice(&(text.len() as u32).to_le_bytes());
            out[6..6 + text.len()].copy_from_slice(text.as_bytes());
            6 + text.len()
        }
        Datum::Record(_) => {
            use core::fmt::Write as _;
            // A cursor writing Display output straight into `out` after the
            // 5-byte header (tag + u32 length).
            struct SliceWriter<'b> {
                buf: &'b mut [u8],
                at: usize,
            }
            impl core::fmt::Write for SliceWriter<'_> {
                fn write_str(&mut self, s: &str) -> core::fmt::Result {
                    self.buf[self.at..self.at + s.len()].copy_from_slice(s.as_bytes());
                    self.at += s.len();
                    Ok(())
                }
            }
            out[0] = 19;
            let mut w = SliceWriter { buf: out, at: 5 };
            let _ = write!(w, "{v}");
            let text_len = w.at - 5;
            out[1..5].copy_from_slice(&(text_len as u32).to_le_bytes());
            5 + text_len
        }
    }
}

/// Reads the value whose tag is `tag` at byte `at`, returning it and its
/// payload length. This is the one place the projected encoding's tag
/// sizes live: a second, hand-written copy in the sort path drifted from
/// it and panicked the server on every tag it had not been taught.
pub fn decode_projected_value(bytes: &[u8], tag: u8, at: usize) -> (Datum<'_>, usize) {
    match tag {
        0 => (Datum::Null, 0),
        1 => (Datum::Bool(bytes[at] != 0), 1),
        2 => (
            Datum::Int4(i32::from_le_bytes(bytes[at..at + 4].try_into().unwrap())),
            4,
        ),
        3 => (
            Datum::Int8(i64::from_le_bytes(bytes[at..at + 8].try_into().unwrap())),
            8,
        ),
        4 => (
            Datum::Float8(f64::from_bits(u64::from_le_bytes(
                bytes[at..at + 8].try_into().unwrap(),
            ))),
            8,
        ),
        5 => {
            let len =
                u32::from_le_bytes(bytes[at..at + 4].try_into().unwrap()) as usize;
            (
                Datum::Text(
                    core::str::from_utf8(&bytes[at + 4..at + 4 + len])
                        .expect("encoded from valid UTF-8"),
                ),
                4 + len,
            )
        }
        6 => (
            Datum::Date(i32::from_le_bytes(bytes[at..at + 4].try_into().unwrap())),
            4,
        ),
        7 => (
            Datum::Timestamp(i64::from_le_bytes(bytes[at..at + 8].try_into().unwrap())),
            8,
        ),
        8 => (
            Datum::Timestamptz(i64::from_le_bytes(bytes[at..at + 8].try_into().unwrap())),
            8,
        ),
        12 => (
            Datum::Time(i64::from_le_bytes(bytes[at..at + 8].try_into().unwrap())),
            8,
        ),
        13 => (
            Datum::Interval(crate::sql::types::Interval {
                months: i32::from_le_bytes(bytes[at..at + 4].try_into().unwrap()),
                days: i32::from_le_bytes(bytes[at + 4..at + 8].try_into().unwrap()),
                micros: i64::from_le_bytes(bytes[at + 8..at + 16].try_into().unwrap()),
            }),
            16,
        ),
        14 => {
            let jsonb = bytes[at] != 0;
            let len = u32::from_le_bytes(bytes[at + 1..at + 5].try_into().unwrap()) as usize;
            let s = core::str::from_utf8(&bytes[at + 5..at + 5 + len]).unwrap_or("");
            (Datum::Json { text: s, jsonb }, 5 + len)
        }
        15 => {
            let element = crate::sql::types::ArrElem::from_code(bytes[at]).unwrap_or(crate::sql::types::ArrElem::Int4);
            let len = u32::from_le_bytes(bytes[at + 1..at + 5].try_into().unwrap()) as usize;
            (Datum::Array { element, raw: &bytes[at + 5..at + 5 + len] }, 5 + len)
        }
        16 => {
            let kind = crate::sql::types::RangeKind::from_code(bytes[at]);
            let len = u32::from_le_bytes(bytes[at + 1..at + 5].try_into().unwrap()) as usize;
            let s = core::str::from_utf8(&bytes[at + 5..at + 5 + len]).unwrap_or("");
            (Datum::Range { text: s, kind }, 5 + len)
        }
        17 => {
            let varying = bytes[at] != 0;
            let len = u32::from_le_bytes(bytes[at + 1..at + 5].try_into().unwrap()) as usize;
            let s = core::str::from_utf8(&bytes[at + 5..at + 5 + len]).unwrap_or("");
            (Datum::Bit { bits: s, varying }, 5 + len)
        }
        18 => {
            let kind = crate::sql::types::RangeKind::from_code(bytes[at]);
            let len = u32::from_le_bytes(bytes[at + 1..at + 5].try_into().unwrap()) as usize;
            let s = core::str::from_utf8(&bytes[at + 5..at + 5 + len]).unwrap_or("");
            (Datum::Multirange { text: s, kind }, 5 + len)
        }
        19 => {
            // A record is stored as its rendered text; the column's RECORD
            // type comes from describe, so returning Text renders it right.
            let len = u32::from_le_bytes(bytes[at..at + 4].try_into().unwrap()) as usize;
            let s = core::str::from_utf8(&bytes[at + 4..at + 4 + len]).unwrap_or("");
            (Datum::Text(s), 4 + len)
        }
        9 => (Datum::Uuid(bytes[at..at + 16].try_into().unwrap()), 16),
        10 => {
            let len =
                u32::from_le_bytes(bytes[at..at + 4].try_into().unwrap()) as usize;
            (Datum::Bytea(&bytes[at + 4..at + 4 + len]), 4 + len)
        }
        11 => {
            let sign = match bytes[at] {
                0 => crate::sql::numeric::Sign::Pos,
                1 => crate::sql::numeric::Sign::Neg,
                _ => crate::sql::numeric::Sign::NaN,
            };
            let weight = i16::from_le_bytes(bytes[at + 1..at + 3].try_into().unwrap());
            let dscale = u16::from_le_bytes(bytes[at + 3..at + 5].try_into().unwrap());
            let ndigits =
                u16::from_le_bytes(bytes[at + 5..at + 7].try_into().unwrap()) as usize;
            (
                Datum::Numeric(crate::sql::numeric::Numeric {
                    sign,
                    weight,
                    dscale,
                    digits: &bytes[at + 7..at + 7 + ndigits * 2],
                }),
                7 + ndigits * 2,
            )
        }
        20 => (
            Datum::Timetz(
                i64::from_le_bytes(bytes[at..at + 8].try_into().unwrap()),
                i32::from_le_bytes(bytes[at + 8..at + 12].try_into().unwrap()),
            ),
            12,
        ),
        _ => unreachable!("tags are exhaustive"),
    }
}

/// Byte length of an encoded row's first `width` values, tags included.
pub fn projected_prefix_len(bytes: &[u8], width: usize) -> usize {
    let mut at = 1usize;
    for _ in 0..width {
        let tag = bytes[at];
        // The reader takes the offset *past* the tag, as its own caller does.
        at += 1;
        at += decode_projected_value(bytes, tag, at).1;
    }
    at
}

/// Reads column `col` back out of an [`encode_projected`] row.
pub fn decode_projected_pub(bytes: &[u8], col: usize) -> Datum<'_> {
    let mut at = 1usize;
    let mut current = 0usize;
    loop {
        let tag = bytes[at];
        at += 1;
        let (value, size) = decode_projected_value(bytes, tag, at);
        if current == col {
            return value;
        }
        at += size;
        current += 1;
    }
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

/// Enforces a PostgreSQL atttypmod on an already-cast value: varchar(n) length
/// (22001) and numeric(p,s) rounding to scale + precision (22003). Values with
/// no modifier, and NULLs, pass through unchanged.
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
    if type_mod < 4 || v.is_null() {
        return Ok(v);
    }
    match (ctype, v) {
        (ColType::Text | ColType::Varchar, Datum::Text(s)) => {
            let max = (type_mod - 4) as usize;
            if s.chars().count() > max {
                let end = s.char_indices().nth(max).map_or(s.len(), |(i, _)| i);
                let t = arena.alloc_str(&s[..end]).map_err(|_| {
                    sql_err!(sqlstate::PROGRAM_LIMIT_EXCEEDED, "cast result too large")
                })?;
                return Ok(Datum::Text(t));
            }
            Ok(v)
        }
        (ColType::Bpchar, Datum::Text(s)) => bpchar_fit(s, (type_mod - 4) as usize, true, arena),
        (ColType::Bit { varying }, Datum::Bit { bits, .. }) => {
            super::eval::fit_bits(bits, (type_mod - 4) as usize, varying, arena)
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

pub fn apply_typmod<'a>(
    v: Datum<'a>,
    ctype: ColType,
    type_mod: i32,
    arena: &'a Arena,
) -> Result<Datum<'a>, SqlError> {
    if type_mod < 4 || v.is_null() {
        return Ok(v);
    }
    match (ctype, v) {
        (ColType::Text | ColType::Varchar, Datum::Text(s)) => {
            let max = (type_mod - 4) as usize;
            if s.chars().count() > max {
                return Err(sql_err!(
                    "22001",
                    "value too long for type character varying({})",
                    max
                ));
            }
            Ok(v)
        }
        (ColType::Bpchar, Datum::Text(s)) => bpchar_fit(s, (type_mod - 4) as usize, false, arena),
        (ColType::Numeric, Datum::Numeric(n)) => {
            let t = type_mod - 4;
            let precision = ((t >> 16) & 0xFFFF) as usize;
            let scale = (t & 0xFFFF) as usize;
            apply_numeric_typmod(&n, precision, scale, arena).map(Datum::Numeric)
        }
        (ColType::Bit { varying }, Datum::Bit { bits, .. }) => {
            super::eval::fit_bits(bits, (type_mod - 4) as usize, varying, arena)
        }
        // Fractional-second precision: micros round half-away-from-zero in
        // integer arithmetic, as PostgreSQL's AdjustTimestampForTypmod.
        (ColType::Timestamp, Datum::Timestamp(t)) => {
            Ok(Datum::Timestamp(round_micros(t, type_mod - 4)))
        }
        (ColType::Timestamptz, Datum::Timestamptz(t)) => {
            Ok(Datum::Timestamptz(round_micros(t, type_mod - 4)))
        }
        (ColType::Time, Datum::Time(t)) => Ok(Datum::Time(round_micros(t, type_mod - 4))),
        (ColType::Timetz, Datum::Timetz(t, zone)) => {
            Ok(Datum::Timetz(round_micros(t, type_mod - 4), zone))
        }
        (ColType::Interval, Datum::Interval(iv)) => Ok(Datum::Interval(
            crate::sql::types::Interval {
                months: iv.months,
                days: iv.days,
                micros: round_micros(iv.micros, type_mod - 4),
            },
        )),
        _ => Ok(v),
    }
}

/// Rounds microseconds to `p` (0..=6) fractional-second digits,
/// half-away-from-zero in integer arithmetic (PostgreSQL's
/// `AdjustTimestampForTypmod`).
fn round_micros(micros: i64, p: i32) -> i64 {
    let p = p.clamp(0, 6) as u32;
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

