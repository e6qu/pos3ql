//! CREATE TABLE definition and constraint construction.

use crate::mem::arena::Arena;
use crate::sql::ast::{
    ColumnDef, ConstraintMode, ConstraintTiming as AstConstraintTiming,
    ConstraintValidation as AstConstraintValidation, Expr, FkAction, QualName, TableConstraint,
};
use crate::sql::eval::{EvalHooks, NoColumns, SqlError, cast_to, eval_full, sqlstate};
use crate::sql::types::ColType;
use crate::sql_err;
use crate::storage::{
    CheckConstraint, ColumnDefault, ColumnMeta, ForeignKey, MAX_COLUMNS, MAX_INDEX_COLS,
    OwnedDatum, SqlName, Storage, TableDef, UniqueKey,
};
use crate::util::StackStr;

use super::apply_typmod;

pub(super) fn build_def(
    name: &str,
    columns: &[ColumnDef],
    storage: &Storage,
    txid: u32,
    arena: &Arena,
) -> Result<TableDef, SqlError> {
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
        def.columns[i] = build_column(c, storage, txid, arena)?;
    }
    // A generation expression may not reference another generated column (42P17);
    // needs the full column set, so check once the table is assembled.
    validate_generated_refs(&def, arena)?;
    Ok(def)
}

/// Enforces that no `GENERATED` column's expression references another generated
/// column (or itself) — PostgreSQL's `check_nested_generated` rule (42P17).
pub(super) fn validate_generated_refs(def: &TableDef, arena: &Arena) -> Result<(), SqlError> {
    for c in def.columns() {
        if !c.default.is_generated() {
            continue;
        }
        let default = c.default.expression();
        let text = default.as_ref().expect("generated column has expr text");
        let expression = crate::sql::parser::parse_expr(text.as_str(), arena)?;
        let mut offending: Option<SqlError> = None;
        expression.for_each_column(&mut |name| {
            if offending.is_some() {
                return;
            }
            if let Some(referenced) = def.columns().iter().find(|col| col.name.as_str() == name)
                && referenced.default.is_generated()
            {
                offending = Some(sql_err!(
                    sqlstate::INVALID_OBJECT_DEFINITION,
                    "cannot use generated column \"{}\" in column generation expression",
                    name
                ));
            }
        });
        if let Some(e) = offending {
            return Err(e);
        }
    }
    Ok(())
}

fn empty_meta() -> ColumnMeta {
    ColumnMeta {
        name: SqlName::parse("").expect("empty fits"),
        ctype: ColType::Bool,
        type_mod: -1,
        collation: crate::sql::ast::Collation::None,
        not_null: crate::storage::NotNullOrigin::Nullable,
        unique: false,
        primary: false,
        auto_increment: false,
        default: ColumnDefault::NONE,
        is_identity: false,
        identity_always: false,
        auto_increment_step: 1,
        user_type: None,
        statistics_target: -1,
    }
}

/// Resolves one column definition, evaluating its DEFAULT (which must be a
/// constant) and coercing it to the column type.
pub(super) fn build_column(
    c: &ColumnDef,
    storage: &Storage,
    txid: u32,
    arena: &Arena,
) -> Result<ColumnMeta, SqlError> {
    if matches!(c.type_name, "record" | "record[]") {
        return Err(sql_err!(
            sqlstate::INVALID_TABLE_DEFINITION,
            "column \"{}\" has pseudo-type {}",
            c.name,
            c.type_name
        ));
    }
    // A base type resolves statically; an unknown name falls back to the
    // domain catalog (base type + identity) then the enum catalog (its own
    // `ColType::Enum` plus its durable identity, so the column's
    // type persists as a name that reloads to the right slot). User-defined
    // array types follow the same path while keeping their element identity.
    let (ctype, type_mod, user_type, domain_default) = match ColType::from_sql_name(c.type_name) {
        Some(ct) => (ct, c.type_mod, None, None),
        None => {
            if let Some(element_name) = c.type_name.strip_suffix("[]") {
                if let Some(slot) = storage.resolve_domain_slot(element_name, txid) {
                    let d = storage.domain(slot);
                    let Some(element) = crate::sql::types::ArrElem::domain(slot as u16, d.base)
                    else {
                        return Err(sql_err!(
                            sqlstate::FEATURE_NOT_SUPPORTED,
                            "arrays of domain {} require a scalar base type",
                            element_name
                        ));
                    };
                    (
                        ColType::Array(element),
                        -1,
                        Some(crate::storage::UserTypeName {
                            schema: d.schema,
                            name: d.name,
                        }),
                        None,
                    )
                } else if let Some(slot) = storage.resolve_enum_slot(element_name, txid) {
                    let definition = storage.enum_for(slot, txid);
                    (
                        ColType::Array(crate::sql::types::ArrElem::Enum(slot as u16)),
                        -1,
                        Some(crate::storage::UserTypeName {
                            schema: definition.schema,
                            name: definition.name,
                        }),
                        None,
                    )
                } else if let Some(slot) = storage.resolve_composite_slot(element_name, txid) {
                    let definition = storage.composite_for(slot, txid);
                    (
                        ColType::Array(crate::sql::types::ArrElem::Composite(slot as u16)),
                        -1,
                        Some(crate::storage::UserTypeName {
                            schema: definition.schema,
                            name: definition.name,
                        }),
                        None,
                    )
                } else {
                    return Err(sql_err!(
                        sqlstate::UNDEFINED_OBJECT,
                        "type \"{}\" does not exist",
                        c.type_name
                    ));
                }
            } else {
                match storage.find_domain(c.type_name, txid) {
                    Some(d) => (
                        d.base,
                        d.base_type_mod,
                        Some(crate::storage::UserTypeName {
                            schema: d.schema,
                            name: d.name,
                        }),
                        d.default_expr,
                    ),
                    None => match storage.resolve_enum_slot(c.type_name, txid) {
                        Some(slot) => {
                            let definition = storage.enum_for(slot, txid);
                            (
                                ColType::Enum(slot as u16),
                                -1,
                                Some(crate::storage::UserTypeName {
                                    schema: definition.schema,
                                    name: definition.name,
                                }),
                                None,
                            )
                        }
                        None => match storage.resolve_composite_slot(c.type_name, txid) {
                            Some(slot) => {
                                let definition = storage.composite_for(slot, txid);
                                (
                                    ColType::Composite(slot as u16),
                                    -1,
                                    Some(crate::storage::UserTypeName {
                                        schema: definition.schema,
                                        name: definition.name,
                                    }),
                                    None,
                                )
                            }
                            None => {
                                return Err(sql_err!(
                                    sqlstate::UNDEFINED_OBJECT,
                                    "type \"{}\" does not exist",
                                    c.type_name
                                ));
                            }
                        },
                    },
                }
            }
        }
    };
    if ctype.is_pseudo() {
        return Err(sql_err!(
            sqlstate::INVALID_TABLE_DEFINITION,
            "column \"{}\" has pseudo-type {}",
            c.name,
            c.type_name
        ));
    }
    if let Some(identity) = user_type {
        storage.require_type_usage(identity.schema.as_str(), identity.name.as_str(), txid)?;
    }
    let default = if let Some(gtext) = c.generated_text {
        if c.default.is_some() {
            return Err(sql_err!(
                sqlstate::SYNTAX_ERROR,
                "both default and generation expression specified for column \"{}\"",
                c.name
            ));
        }
        ColumnDefault::Generated(resolve_generated(gtext, arena)?)
    } else {
        let column_default = resolve_default(
            c.default,
            c.default_text,
            ctype,
            type_mod,
            storage,
            txid,
            arena,
        )?;
        // A domain-typed column with no column-level DEFAULT inherits the
        // domain's DEFAULT (baked in at creation, re-evaluated per insert).
        if matches!(column_default, ColumnDefault::None) {
            domain_default
                .map(ColumnDefault::Expression)
                .unwrap_or(ColumnDefault::None)
        } else {
            column_default
        }
    };
    // serial/bigserial/smallserial are int4/int8/int2 with an auto-increment
    // default and an implicit NOT NULL. A GENERATED ... AS IDENTITY column is
    // also auto-increment, but tracked distinctly (attidentity) and with its own
    // step; both imply NOT NULL.
    let serial = matches!(
        c.type_name,
        "serial" | "serial4" | "bigserial" | "serial8" | "smallserial" | "serial2"
    );
    let (is_identity, identity_always, auto_increment_step) = match c.identity {
        Some(spec) => {
            if default.is_generated() {
                return Err(sql_err!(
                    sqlstate::SYNTAX_ERROR,
                    "column \"{}\" cannot be both an identity and a generated column",
                    c.name
                ));
            }
            (true, spec.always, spec.options.increment.unwrap_or(1))
        }
        None => (false, false, 1),
    };
    let auto_increment = serial || is_identity;
    let collatable = ctype.is_collatable();
    let collation = resolve_parsed_collation(storage, txid, c.collation)?;
    if !collatable && collation != crate::sql::ast::Collation::Default {
        return Err(sql_err!(
            sqlstate::DATATYPE_MISMATCH,
            "collations are not supported by type {}",
            ctype.name()
        ));
    }
    Ok(ColumnMeta {
        name: SqlName::parse(c.name)?,
        ctype,
        type_mod,
        collation: if collatable {
            collation
        } else {
            crate::sql::ast::Collation::None
        },
        not_null: crate::storage::NotNullOrigin::local(c.not_null || auto_increment),
        unique: c.unique,
        primary: c.primary,
        auto_increment,
        default,
        is_identity,
        identity_always,
        auto_increment_step,
        user_type,
        statistics_target: -1,
    })
}

pub(super) fn resolve_parsed_collation(
    storage: &Storage,
    txid: u32,
    parsed: crate::sql::ast::ParsedCollation<'_>,
) -> Result<crate::sql::ast::Collation, SqlError> {
    match parsed {
        crate::sql::ast::ParsedCollation::Builtin(collation) => Ok(collation),
        crate::sql::ast::ParsedCollation::Named(name) => storage
            .resolve_collation(name.schema, name.name, txid)
            .ok_or_else(|| {
                sql_err!(
                    sqlstate::UNDEFINED_OBJECT,
                    "collation \"{}\" does not exist",
                    name.name
                )
            }),
    }
}

/// Validates a `GENERATED ALWAYS AS (expr) STORED` expression and returns its
/// stored text: the expression must be immutable (42P17) and free of subqueries
/// (0A000). The no-reference-to-another-generated-column rule needs the full
/// column list and is checked once the table is assembled.
pub(super) fn resolve_generated(
    text: &str,
    arena: &Arena,
) -> Result<StackStr<{ crate::storage::DEFAULT_EXPR_MAX }>, SqlError> {
    let expression = crate::sql::parser::parse_expr(text, arena)?;
    if expression.contains_subquery() {
        return Err(sql_err!(
            sqlstate::FEATURE_NOT_SUPPORTED,
            "cannot use subquery in column generation expression"
        ));
    }
    if let Some(name) = expression.contains_nonimmutable_function() {
        return Err(sql_err!(
            sqlstate::INVALID_OBJECT_DEFINITION,
            "generation expression is not immutable (uses {}())",
            name
        ));
    }
    let stored = StackStr::from_str(text);
    if stored.is_truncated() {
        return Err(sql_err!(
            sqlstate::PROGRAM_LIMIT_EXCEEDED,
            "generation expression exceeds {} bytes",
            crate::storage::DEFAULT_EXPR_MAX
        ));
    }
    Ok(stored)
}

/// Resolves a column DEFAULT into one complete catalog state.
pub(super) fn resolve_default(
    default: Option<&Expr>,
    default_text: Option<&str>,
    ctype: ColType,
    type_mod: i32,
    storage: &Storage,
    txid: u32,
    arena: &Arena,
) -> Result<ColumnDefault, SqlError> {
    let Some(expression) = default else {
        return Ok(ColumnDefault::None);
    };
    // A literal-only default folds to a constant now; anything volatile or
    // stable (any function call) is kept as text and evaluated at insert time.
    if !expression.contains_call() {
        let catalog = crate::sql::query::storage_catalog(storage, arena, txid);
        let hooks = EvalHooks {
            group: None,
            aggs: None,
            subs: None,
            windows: None,
            catalog: Some(&catalog),
            srf_index: None,
            project_sets: None,
            sequences: None,
        };
        let v = eval_full(
            expression,
            arena,
            crate::sql::eval::NO_PARAMS,
            &NoColumns,
            &hooks,
        )?;
        let v = match ctype {
            ColType::Enum(slot) => super::coerce_enum_value(v, slot, storage, txid, arena)?,
            ColType::Array(
                element @ (crate::sql::types::ArrElem::Enum(_)
                | crate::sql::types::ArrElem::Domain { .. }),
            ) => super::coerce_user_type_array(v, element, storage, txid, arena)?,
            ColType::Array(element) if element.is_catalog_reference() => {
                let catalog = crate::sql::query::storage_catalog(storage, arena, txid);
                crate::sql::eval::reg_array_cast(v, element, Some(&catalog), arena)?
            }
            target if target.is_reg_object() => {
                let catalog = crate::sql::query::storage_catalog(storage, arena, txid);
                crate::sql::eval::regobject_cast(v, target, Some(&catalog), arena)?
            }
            _ => cast_to(v, ctype, arena)?,
        };
        let v = apply_typmod(v, ctype, type_mod, arena)?;
        return Ok(ColumnDefault::Constant {
            value: OwnedDatum::from_datum(&v)?,
            expression: store_default_text(default_text)?,
        });
    }
    Ok(ColumnDefault::Expression(store_default_text(default_text)?))
}

fn store_default_text(
    default_text: Option<&str>,
) -> Result<StackStr<{ crate::storage::DEFAULT_EXPR_MAX }>, SqlError> {
    let text = default_text.ok_or_else(|| {
        sql_err!(
            sqlstate::INTERNAL_ERROR,
            "DEFAULT expression source text is unavailable"
        )
    })?;
    let stored = StackStr::from_str(text);
    if stored.is_truncated() {
        return Err(sql_err!(
            sqlstate::PROGRAM_LIMIT_EXCEEDED,
            "DEFAULT expression exceeds {} bytes",
            crate::storage::DEFAULT_EXPR_MAX
        ));
    }
    Ok(stored)
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
pub(super) fn resolve_cols(
    def: &TableDef,
    names: &[&str],
) -> Result<([u16; MAX_INDEX_COLS], usize), SqlError> {
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
/// (which PostgreSQL forbids in CHECK). Each referenced column's index is OR'd
/// into `cols` (bit `i` = column `i`), so the caller can name the constraint
/// the way PostgreSQL does — `<table>_<column>_check` when the predicate
/// references exactly one column, `<table>_check` otherwise.
pub(crate) fn check_referenced_columns(expression: &Expr, def: &TableDef) -> Result<u64, SqlError> {
    let mut columns = 0;
    validate_check_refs(expression, def, &mut columns)?;
    Ok(columns)
}

fn validate_check_refs(expression: &Expr, def: &TableDef, cols: &mut u64) -> Result<(), SqlError> {
    match expression {
        Expr::SchemaColumn { table, .. } => {
            return Err(sql_err!(
                sqlstate::UNDEFINED_TABLE,
                "invalid reference to FROM-clause entry for table \"{}\"",
                table
            ));
        }
        Expr::WholeRow(t) => {
            return Err(sql_err!(
                sqlstate::FEATURE_NOT_SUPPORTED,
                "whole-row reference to \"{}\" is not supported in CHECK",
                t
            ));
        }
        Expr::Column { name, .. } => {
            let Some(index) = def.column_index(name) else {
                return Err(sql_err!(
                    sqlstate::UNDEFINED_COLUMN,
                    "column \"{}\" does not exist",
                    name
                ));
            };
            // MAX_COLUMNS is 64, so a column index always fits the u64 mask.
            *cols |= 1u64 << index;
        }
        Expr::RoutineParam { .. } => {}
        Expr::RecursiveState { .. } => {}
        Expr::Subquery(_)
        | Expr::InSubquery { .. }
        | Expr::QuantifiedSubquery { .. }
        | Expr::Exists(_)
        | Expr::ArraySubquery(_) => {
            return Err(sql_err!(
                sqlstate::FEATURE_NOT_SUPPORTED,
                "cannot use subquery in check constraint"
            ));
        }
        Expr::Unary { operand, .. }
        | Expr::Cast { operand, .. }
        | Expr::Collate { operand, .. }
        | Expr::IsNull { operand, .. } => validate_check_refs(operand, def, cols)?,
        Expr::Binary { left, right, .. } => {
            validate_check_refs(left, def, cols)?;
            validate_check_refs(right, def, cols)?;
        }
        Expr::Call { args, .. } => {
            for a in *args {
                validate_check_refs(a, def, cols)?;
            }
        }
        Expr::InList { operand, list, .. } => {
            validate_check_refs(operand, def, cols)?;
            for a in *list {
                validate_check_refs(a, def, cols)?;
            }
        }
        Expr::Between {
            operand, low, high, ..
        } => {
            validate_check_refs(operand, def, cols)?;
            validate_check_refs(low, def, cols)?;
            validate_check_refs(high, def, cols)?;
        }
        Expr::Like {
            operand, pattern, ..
        }
        | Expr::Match {
            operand, pattern, ..
        } => {
            validate_check_refs(operand, def, cols)?;
            validate_check_refs(pattern, def, cols)?;
        }
        Expr::Case {
            operand,
            whens,
            otherwise,
            ..
        } => {
            if let Some(o) = operand {
                validate_check_refs(o, def, cols)?;
            }
            for (w, t) in *whens {
                validate_check_refs(w, def, cols)?;
                validate_check_refs(t, def, cols)?;
            }
            if let Some(o) = otherwise {
                validate_check_refs(o, def, cols)?;
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
                validate_check_refs(e, def, cols)?;
            }
        }
        Expr::Subscript { base, index } => {
            validate_check_refs(base, def, cols)?;
            validate_check_refs(index, def, cols)?;
        }
        Expr::Slice { base, lower, upper } => {
            validate_check_refs(base, def, cols)?;
            if let Some(e) = lower {
                validate_check_refs(e, def, cols)?;
            }
            if let Some(e) = upper {
                validate_check_refs(e, def, cols)?;
            }
        }
        Expr::Field { base, .. } => validate_check_refs(base, def, cols)?,
        Expr::AnyAll { operand, array, .. } => {
            validate_check_refs(operand, def, cols)?;
            validate_check_refs(array, def, cols)?;
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
    merge_equivalent: bool,
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
            TableConstraint::PrimaryKey {
                name,
                columns,
                timing,
            } => {
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
                    def.columns[column_index as usize].not_null =
                        def.columns[column_index as usize].not_null.add_local();
                }
                // An unnamed single-column key rides the column flag with a
                // synthesized name; an explicitly named one is a first-class
                // key so DROP / RENAME CONSTRAINT and the violation message all
                // see the given name (its NOT NULL is already set above).
                if n == 1 && name.is_none() && !timing.is_deferrable() {
                    def.columns[indices[0] as usize].primary = true;
                    def.columns[indices[0] as usize].unique = true;
                } else {
                    add_unique_key(def, *name, "pkey", &indices, n, true, *timing)?;
                }
            }
            TableConstraint::Unique {
                name,
                columns,
                timing,
            } => {
                let (indices, n) = resolve_cols(def, columns)?;
                let stored_timing = storage_timing(*timing);
                let equivalent_flag = n == 1
                    && !stored_timing.is_deferrable()
                    && (def.columns[indices[0] as usize].unique
                        || def.columns[indices[0] as usize].primary);
                let equivalent_key = def.uniques().iter().any(|key| {
                    !key.is_primary && key.timing == stored_timing && key.columns() == &indices[..n]
                });
                if merge_equivalent && (equivalent_flag || equivalent_key) {
                    continue;
                }
                if n == 1 && name.is_none() && !stored_timing.is_deferrable() {
                    def.columns[indices[0] as usize].unique = true;
                } else {
                    add_unique_key(def, *name, "key", &indices, n, false, *timing)?;
                }
            }
            TableConstraint::Check {
                name,
                expression,
                text,
                validation,
            } => {
                let referenced_cols = check_referenced_columns(expression, def)?;
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
                    None => auto_check_name(def, referenced_cols)?,
                };
                let mut c = CheckConstraint {
                    name: constraint_name,
                    expression: crate::util::StackStr::new(),
                    validation: storage_validation(*validation),
                };
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
                timing,
                validation,
            } => {
                attach_fkey(
                    storage,
                    def,
                    *name,
                    columns,
                    parent,
                    parent_cols,
                    *on_delete,
                    *on_update,
                    *timing,
                    *validation,
                    txid,
                    arena,
                )?;
            }
            TableConstraint::Exclusion {
                name,
                columns,
                operators,
                predicate,
                predicate_text,
                timing,
            } => {
                let (indices, count) = resolve_cols(def, columns)?;
                for &column in &indices[..count] {
                    if !matches!(
                        def.columns[column as usize].ctype,
                        crate::sql::types::ColType::Range(_)
                    ) {
                        return Err(sql_err!(
                            sqlstate::UNDEFINED_OBJECT,
                            "data type {} has no default operator class for access method \"gist\"",
                            def.columns[column as usize].ctype.name()
                        ));
                    }
                }
                if def.n_exclusions == crate::storage::MAX_EXCLUSIONS {
                    return Err(sql_err!(
                        sqlstate::PROGRAM_LIMIT_EXCEEDED,
                        "a table can have at most {} exclusion constraints",
                        crate::storage::MAX_EXCLUSIONS
                    ));
                }
                if let Some(expression) = predicate {
                    let _ = check_referenced_columns(expression, def)?;
                }
                let mut exclusion = crate::storage::ExclusionConstraint::EMPTY;
                exclusion.name = match name {
                    Some(name) => SqlName::parse(name)?,
                    None => auto_key_name(def, &indices[..count], "excl", true)?,
                };
                exclusion.columns[..count].copy_from_slice(&indices[..count]);
                for (index, operator) in operators.iter().copied().enumerate() {
                    exclusion.operators[index] = match operator {
                        crate::sql::ast::ExclusionOperator::Equal => {
                            crate::storage::ExclusionOperator::Equal
                        }
                        crate::sql::ast::ExclusionOperator::Overlaps => {
                            crate::storage::ExclusionOperator::Overlaps
                        }
                        crate::sql::ast::ExclusionOperator::Adjacent => {
                            crate::storage::ExclusionOperator::Adjacent
                        }
                    };
                }
                exclusion.n_cols = count;
                exclusion.timing = storage_timing(*timing);
                exclusion.predicate = match predicate_text {
                    Some(text) => {
                        let mut stored = crate::util::StackStr::new();
                        let _ = core::fmt::Write::write_str(&mut stored, text);
                        if stored.is_truncated() {
                            return Err(sql_err!(
                                sqlstate::PROGRAM_LIMIT_EXCEEDED,
                                "exclusion predicate is too long"
                            ));
                        }
                        Some(stored)
                    }
                    None => None,
                };
                def.exclusions[def.n_exclusions] = exclusion;
                def.n_exclusions += 1;
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

/// PostgreSQL's auto-generated CHECK name: `<table>_<column>_check` when the
/// predicate references exactly one column, `<table>_check` when it references
/// zero or several — with the smallest numeric suffix (`_check1`, `_check2`, …)
/// that avoids colliding with a constraint the table already carries.
fn auto_check_name(def: &TableDef, referenced_cols: u64) -> Result<SqlName, SqlError> {
    use core::fmt::Write as _;
    let mut base = crate::util::StackStr::<64>::new();
    let _ = write!(base, "{}", def.name.as_str());
    if referenced_cols.count_ones() == 1 {
        let column = referenced_cols.trailing_zeros() as usize;
        let _ = write!(base, "_{}", def.columns()[column].name.as_str());
    }
    let _ = write!(base, "_check");
    if base.is_truncated() {
        return Err(sql_err!(
            sqlstate::PROGRAM_LIMIT_EXCEEDED,
            "generated constraint name is too long"
        ));
    }
    disambiguate_constraint_name(def, base.as_str())
}

/// Appends the smallest numeric suffix (none, then 1, 2, …) that makes `base`
/// unique among the table's existing constraint names — PostgreSQL's
/// `ChooseConstraintName`. Constraint names are unique per table, so this
/// searches the checks, keys, foreign keys, and the single-column key names
/// synthesized from column flags.
fn disambiguate_constraint_name(def: &TableDef, base: &str) -> Result<SqlName, SqlError> {
    use core::fmt::Write as _;
    if !constraint_name_taken(def, base) {
        return SqlName::parse(base);
    }
    // The table's constraint arrays are all bounded (a few each), so a free
    // suffix is found almost immediately; the ceiling is just a loud backstop.
    for suffix in 1u32..=u16::MAX as u32 {
        let mut candidate = crate::util::StackStr::<64>::new();
        let _ = write!(candidate, "{}{}", base, suffix);
        if candidate.is_truncated() {
            return Err(sql_err!(
                sqlstate::PROGRAM_LIMIT_EXCEEDED,
                "generated constraint name is too long"
            ));
        }
        if !constraint_name_taken(def, candidate.as_str()) {
            return SqlName::parse(candidate.as_str());
        }
    }
    Err(sql_err!(
        sqlstate::PROGRAM_LIMIT_EXCEEDED,
        "could not find a free constraint name for \"{}\"",
        base
    ))
}

/// Whether `name` is already the name of any constraint on `def`: a stored
/// CHECK, unique key, or foreign key, or one of the names synthesized for the
/// single-column PRIMARY KEY / UNIQUE flags (`<table>_pkey` / `<table>_<col>_key`).
fn constraint_name_taken(def: &TableDef, name: &str) -> bool {
    use core::fmt::Write as _;
    if def.checks[..def.n_checks]
        .iter()
        .any(|c| c.name.as_str() == name)
        || def.uniques[..def.n_uniques]
            .iter()
            .any(|k| k.name.as_str() == name)
        || def.fkeys[..def.n_fkeys]
            .iter()
            .any(|f| f.name.as_str() == name)
        || def.exclusions[..def.n_exclusions]
            .iter()
            .any(|exclusion| exclusion.name.as_str() == name)
    {
        return true;
    }
    for c in def.columns() {
        let mut synthesized = crate::util::StackStr::<64>::new();
        if c.primary {
            let _ = write!(synthesized, "{}_pkey", def.name.as_str());
        } else if c.unique {
            let _ = write!(synthesized, "{}_{}_key", def.name.as_str(), c.name.as_str());
        } else {
            continue;
        }
        if !synthesized.is_truncated() && synthesized.as_str() == name {
            return true;
        }
    }
    false
}

pub(super) fn add_unique_key(
    def: &mut TableDef,
    name: Option<&str>,
    suffix: &str,
    indices: &[u16; MAX_INDEX_COLS],
    n: usize,
    is_primary: bool,
    timing: AstConstraintTiming,
) -> Result<(), SqlError> {
    if def.n_uniques == crate::storage::MAX_UNIQUES {
        return Err(sql_err!(
            sqlstate::PROGRAM_LIMIT_EXCEEDED,
            "a table can have at most {} key constraints",
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
    k.timing = storage_timing(timing);
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
    parent: &QualName,
    parent_cols: &[&str],
    on_delete: FkAction,
    on_update: FkAction,
    timing: AstConstraintTiming,
    validation: AstConstraintValidation,
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

    // The parent may be this very table (self-reference), not yet in
    // storage: PostgreSQL resolves the reference with the new table already
    // cataloged, so a name landing on the creation target is a self-reference
    // — a bare name only when no earlier search-path schema holds an existing
    // table of that name, a qualified one when it names the table's schema.
    let resolved: Option<usize> = match storage.resolve_relation(parent.schema, parent.name, txid) {
        Some(crate::storage::ResolvedRelation::Table(pi)) => Some(pi),
        _ => None,
    };
    let self_ref = parent.name == def.name.as_str()
        && match parent.schema {
            Some(schema) => schema == def.schema.as_str(),
            None => {
                // Does an existing table shadow the self-reference earlier in
                // the path than the creation schema? The creation schema is
                // the first path schema, so any hit resolves after it — the
                // self-reference wins.
                true
            }
        };
    let parent_def: TableDef = if self_ref {
        *def
    } else {
        let Some(pi) = resolved else {
            return Err(match parent.schema {
                Some(schema) => sql_err!(
                    sqlstate::UNDEFINED_TABLE,
                    "relation \"{}.{}\" does not exist",
                    schema,
                    parent.name
                ),
                None => sql_err!(
                    sqlstate::UNDEFINED_TABLE,
                    "relation \"{}\" does not exist",
                    parent.name
                ),
            });
        };
        *storage.table_def(pi, txid)
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
                parent.name
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
    if !self_ref {
        let parent_slot = resolved.expect("non-self foreign key resolved above");
        storage.require_schema_usage(parent_def.schema.as_str(), txid)?;
        let role = storage.current_role_slot(txid).ok_or_else(|| {
            sql_err!(
                sqlstate::INSUFFICIENT_PRIVILEGE,
                "current role is not present in the role catalog"
            )
        })?;
        let object = storage.table_access_object(parent_slot, txid);
        let allowed = storage.has_object_privilege(
            object,
            role,
            crate::storage::PrivilegeSet::REFERENCES,
            txid,
        ) || parent_idxs[..n_parent].iter().all(|column| {
            crate::storage::ColumnPrivilegeTarget::new(object, *column).is_ok_and(|target| {
                storage.has_column_privilege(
                    target,
                    role,
                    crate::storage::PrivilegeSet::REFERENCES,
                    txid,
                )
            })
        });
        if !allowed {
            return Err(sql_err!(
                sqlstate::INSUFFICIENT_PRIVILEGE,
                "permission denied for table {}",
                parent_def.name.as_str()
            ));
        }
    }

    // The referenced columns must be a unique key of the parent (PG 42830).
    if !is_unique_key(&parent_def, &parent_idxs[..n_parent]) {
        return Err(sql_err!(
            crate::sql::eval::sqlstate::INVALID_FOREIGN_KEY,
            "there is no unique constraint matching given keys for referenced table \"{}\"",
            parent.name
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
    fk.parent_schema = parent_def.schema;
    fk.parent = parent_def.name;
    fk.parent_cols[..n_parent].copy_from_slice(&parent_idxs[..n_parent]);
    fk.n_parent_cols = n_parent;
    fk.on_delete = fk_action_of(on_delete);
    fk.on_update = fk_action_of(on_update);
    fk.timing = storage_timing(timing);
    fk.validation = storage_validation(validation);
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
pub(super) fn references_named_key(def: &TableDef, name: &str, columns: &[u16]) -> bool {
    if columns.len() == 1 {
        let c = &def.columns()[columns[0] as usize];
        if c.primary {
            return crate::stack_format!(96, "{}_pkey", def.name.as_str()).as_str() == name;
        }
        if c.unique {
            return crate::stack_format!(128, "{}_{}_key", def.name.as_str(), c.name.as_str())
                .as_str()
                == name;
        }
    }
    def.uniques()
        .iter()
        .find(|uk| {
            !uk.timing.is_deferrable() && uk.n_cols == columns.len() && {
                let key_columns = uk.columns();
                columns.iter().all(|c| key_columns.contains(c))
                    && key_columns.iter().all(|c| columns.contains(c))
            }
        })
        .is_some_and(|key| key.name.as_str() == name)
}

fn is_unique_key(def: &TableDef, columns: &[u16]) -> bool {
    if columns.len() == 1 {
        let c = &def.columns()[columns[0] as usize];
        if c.unique || c.primary {
            return true;
        }
    }
    def.uniques().iter().any(|uk| {
        !uk.timing.is_deferrable() && uk.n_cols == columns.len() && {
            let a = uk.columns();
            columns.iter().all(|c| a.contains(c)) && a.iter().all(|c| columns.contains(c))
        }
    })
}

fn storage_timing(timing: AstConstraintTiming) -> crate::storage::ConstraintTiming {
    match timing {
        AstConstraintTiming::NotDeferrable => crate::storage::ConstraintTiming::NotDeferrable,
        AstConstraintTiming::Deferrable(ConstraintMode::Immediate) => {
            crate::storage::ConstraintTiming::DeferrableImmediate
        }
        AstConstraintTiming::Deferrable(ConstraintMode::Deferred) => {
            crate::storage::ConstraintTiming::DeferrableDeferred
        }
    }
}

fn storage_validation(validation: AstConstraintValidation) -> crate::storage::ConstraintValidation {
    match validation {
        AstConstraintValidation::EnforcedValidated => {
            crate::storage::ConstraintValidation::EnforcedValidated
        }
        AstConstraintValidation::EnforcedNotValid => {
            crate::storage::ConstraintValidation::EnforcedNotValid
        }
        AstConstraintValidation::NotEnforced => crate::storage::ConstraintValidation::NotEnforced,
    }
}
