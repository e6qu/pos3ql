//! Turning a parsed `CREATE TABLE` into the definition the storage layer keeps.
//!
//! Column metadata from the column definitions, then the constraints attached
//! over it: PRIMARY KEY and UNIQUE (single-column ones ride the column flags,
//! wider ones become keys of their own), CHECK with its references validated
//! against the columns that exist, and FOREIGN KEY resolved against the parent
//! it names. Constraint names follow PostgreSQL's generated spelling when the
//! statement does not give one.

use crate::mem::arena::Arena;
use crate::sql::ast::{ColumnDef, Expr, FkAction, TableConstraint};
use crate::sql::eval::{cast_to, eval, sqlstate, NoColumns, SqlError};
use crate::sql::types::ColType;
use crate::storage::{
    CheckConstraint, ColumnMeta, ForeignKey, OwnedDatum, SqlName, Storage, TableDef, UniqueKey,
    MAX_COLUMNS, MAX_INDEX_COLS,
};
use crate::{sql_err, stack_format};

use super::apply_typmod;

pub(super) fn build_def(name: &str, columns: &[ColumnDef], arena: &Arena) -> Result<TableDef, SqlError> {
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
                sqlstate::DUPLICATE_COLUMN,
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
pub(super) fn build_column(c: &ColumnDef, arena: &Arena) -> Result<ColumnMeta, SqlError> {
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
            let v = eval(expression, arena, crate::sql::eval::NO_PARAMS, &NoColumns).map_err(|_| {
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

fn fk_action_of(a: FkAction) -> crate::storage::FkAction {
    use crate::storage::FkAction as S;
    match a {
        FkAction::NoAction => S::NoAction,
        FkAction::Restrict => S::Restrict,
        FkAction::Cascade => S::Cascade,
        FkAction::SetNull => S::SetNull,
        FkAction::SetDefault => S::SetDefault,
    }
}

/// Resolves a constraint's column names to indices in `def` (42703 if absent).
pub(super) fn resolve_cols(def: &TableDef, names: &[&str]) -> Result<([u16; MAX_INDEX_COLS], usize), SqlError> {
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
                sqlstate::FEATURE_NOT_SUPPORTED,
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
        Expr::Case { operand, whens, otherwise, .. } => {
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
pub(super) fn attach_constraints(
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
                        crate::sql::eval::sqlstate::INVALID_TABLE_DEFINITION,
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
pub(super) fn auto_key_name(
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

pub(super) fn add_unique_key(
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
                crate::sql::eval::sqlstate::INVALID_FOREIGN_KEY,
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
            crate::sql::eval::sqlstate::INVALID_FOREIGN_KEY,
            "number of referencing and referenced columns for foreign key disagree"
        ));
    }
    let (parent_idxs, _) = resolve_cols(&parent_def, &pcol_names[..n_parent])?;

    // The referenced columns must be a unique key of the parent (PG 42830).
    if !is_unique_key(&parent_def, &parent_idxs[..n_parent]) {
        return Err(sql_err!(
            crate::sql::eval::sqlstate::INVALID_FOREIGN_KEY,
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
                crate::sql::eval::sqlstate::DATATYPE_MISMATCH,
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
pub(super) fn primary_key_cols(def: &TableDef) -> ([u16; MAX_INDEX_COLS], usize) {
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
