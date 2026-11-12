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
    CheckConstraint, ColumnDefault, ColumnMeta, ColumnSet, ForeignKey, MAX_COLUMNS, MAX_INDEX_COLS,
    MAX_RELATION_COLUMNS, OwnedDatum, SqlName, Storage, TableDefBuilder, UniqueKey,
};
use crate::util::StackStr;

use super::apply_typmod;

pub(super) fn build_def(
    name: &str,
    columns: &[ColumnDef],
    storage: &Storage,
    txid: u32,
    arena: &Arena,
) -> Result<TableDefBuilder, SqlError> {
    if columns.len() > MAX_RELATION_COLUMNS {
        return Err(sql_err!(
            sqlstate::TOO_MANY_COLUMNS,
            "tables can have at most {} columns",
            MAX_RELATION_COLUMNS
        ));
    }
    let mut def = TableDefBuilder {
        name: SqlName::parse(name)?,
        columns: [empty_meta(); MAX_COLUMNS],
        n_columns: columns.len(),
        ..TableDefBuilder::empty()
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
        if let Some(name) = c.not_null_name {
            def.set_not_null_constraint_name(i, SqlName::parse(name)?);
        }
    }
    // A generation expression may not reference another generated column (42P17);
    // needs the full column set, so check once the table is assembled.
    validate_generated_refs(&def, arena)?;
    Ok(def)
}

/// Enforces that no `GENERATED` column's expression references another generated
/// column (or itself) — PostgreSQL's `check_nested_generated` rule (42P17).
pub(super) fn validate_generated_refs<Definition0: crate::storage::DefinitionBacking>(
    def: &crate::storage::TableDefinition<Definition0>,
    arena: &Arena,
) -> Result<(), SqlError> {
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
        storage: crate::sql::ast::ColumnStorage::Plain,
        compression: crate::sql::ast::ColumnCompression::Default,
        not_null: crate::storage::NotNullOrigin::Nullable,
        not_null_inheritable: true,
        unique: false,
        primary: false,
        auto_increment: false,
        default: ColumnDefault::NONE,
        is_identity: false,
        identity_always: false,
        user_type: None,
        statistics_target: -1,
        statistics_options: crate::sql::ast::ColumnStatisticsOptions::DEFAULT,
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
                    let Some(element) = crate::sql::types::ArrElem::domain(d.creation(), d.base)
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
                        Some(crate::storage::UserTypeName::declaration(d.schema, d.name)),
                        None,
                    )
                } else if let Some(slot) = storage.resolve_enum_slot(element_name, txid) {
                    let definition = storage.enum_for(slot, txid);
                    (
                        ColType::Array(crate::sql::types::ArrElem::Enum(Some(
                            definition.creation(),
                        ))),
                        -1,
                        Some(crate::storage::UserTypeName::declaration(
                            definition.schema,
                            definition.name,
                        )),
                        None,
                    )
                } else if let Some(slot) = storage.resolve_composite_slot(element_name, txid) {
                    let definition = storage.composite_for(slot, txid);
                    (
                        ColType::Array(crate::sql::types::ArrElem::Composite(Some(
                            definition.creation(),
                        ))),
                        -1,
                        Some(crate::storage::UserTypeName::declaration(
                            definition.schema,
                            definition.name,
                        )),
                        None,
                    )
                } else {
                    return Err(crate::sql::error_position::at_subject(
                        sql_err!(
                            sqlstate::UNDEFINED_OBJECT,
                            "type \"{}\" does not exist",
                            c.type_name
                        ),
                        c.type_name,
                    ));
                }
            } else {
                match storage.find_domain(c.type_name, txid) {
                    Some(d) => (
                        d.base,
                        d.base_type_mod,
                        Some(crate::storage::UserTypeName::declaration(d.schema, d.name)),
                        d.default_expr,
                    ),
                    None => match storage.resolve_enum_slot(c.type_name, txid) {
                        Some(slot) => {
                            let definition = storage.enum_for(slot, txid);
                            (
                                ColType::Enum(Some(definition.creation())),
                                -1,
                                Some(crate::storage::UserTypeName::declaration(
                                    definition.schema,
                                    definition.name,
                                )),
                                None,
                            )
                        }
                        None => match storage.resolve_composite_slot(c.type_name, txid) {
                            Some(slot) => {
                                let definition = storage.composite_for(slot, txid);
                                (
                                    ColType::Composite(Some(definition.creation())),
                                    -1,
                                    Some(crate::storage::UserTypeName::declaration(
                                        definition.schema,
                                        definition.name,
                                    )),
                                    None,
                                )
                            }
                            None => {
                                return Err(crate::sql::error_position::at_subject(
                                    sql_err!(
                                        sqlstate::UNDEFINED_OBJECT,
                                        "type \"{}\" does not exist",
                                        c.type_name
                                    ),
                                    c.type_name,
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
    let bound =
        storage.bind_routine_result(crate::storage::RoutineResult { ctype, user_type }, txid)?;
    let (ctype, user_type) = (bound.ctype, bound.user_type);
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
    // SERIAL requests a bound nextval default during table creation.
    // IDENTITY retains implicit generation; both imply NOT NULL.
    let serial = matches!(
        c.type_name,
        "serial" | "serial4" | "bigserial" | "serial8" | "smallserial" | "serial2"
    );
    let (is_identity, identity_always) = match c.identity {
        Some(spec) => {
            if default.is_generated() {
                return Err(sql_err!(
                    sqlstate::SYNTAX_ERROR,
                    "column \"{}\" cannot be both an identity and a generated column",
                    c.name
                ));
            }
            (true, spec.always)
        }
        None => (false, false),
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
    let column_storage = c.storage.unwrap_or_else(|| default_column_storage(ctype));
    validate_column_storage(c.name, ctype, column_storage, c.compression)?;
    Ok(ColumnMeta {
        name: SqlName::parse(c.name)?,
        ctype,
        type_mod,
        collation: if collatable {
            collation
        } else {
            crate::sql::ast::Collation::None
        },
        storage: column_storage,
        compression: c.compression,
        not_null: crate::storage::NotNullOrigin::local(c.not_null || auto_increment),
        not_null_inheritable: c.not_null_inheritable,
        unique: c.unique,
        primary: c.primary,
        auto_increment,
        default,
        is_identity,
        identity_always,
        user_type,
        statistics_target: -1,
        statistics_options: crate::sql::ast::ColumnStatisticsOptions::DEFAULT,
    })
}

pub(super) fn default_column_storage(ctype: ColType) -> crate::sql::ast::ColumnStorage {
    crate::sql::ast::ColumnStorage::from_code(ctype.physical_layout().storage)
        .expect("captured PostgreSQL type storage policy is valid")
}

/// Storage and compression are catalog-visible schema properties. They are
/// accepted only for the type families PostgreSQL can TOAST; object blocks keep
/// their independent provider-neutral codec.
pub(super) fn validate_column_storage(
    name: &str,
    ctype: ColType,
    storage: crate::sql::ast::ColumnStorage,
    compression: crate::sql::ast::ColumnCompression,
) -> Result<(), SqlError> {
    let toastable = ctype.physical_layout().storage != b'p';
    if !toastable && storage != crate::sql::ast::ColumnStorage::Plain {
        return Err(sql_err!(
            sqlstate::FEATURE_NOT_SUPPORTED,
            "cannot set storage option to {} for column \"{}\"",
            match storage {
                crate::sql::ast::ColumnStorage::Plain => "PLAIN",
                crate::sql::ast::ColumnStorage::External => "EXTERNAL",
                crate::sql::ast::ColumnStorage::Extended => "EXTENDED",
                crate::sql::ast::ColumnStorage::Main => "MAIN",
            },
            name
        ));
    }
    if compression != crate::sql::ast::ColumnCompression::Default && !toastable {
        return Err(sql_err!(
            sqlstate::FEATURE_NOT_SUPPORTED,
            "compression is not supported for column \"{}\"",
            name
        ));
    }
    Ok(())
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
    if expression.contains_subquery() {
        return Err(sql_err!(
            sqlstate::FEATURE_NOT_SUPPORTED,
            "cannot use subquery in DEFAULT expression"
        ));
    }
    let mut has_reference = false;
    expression.for_each_column_reference(&mut |_, _| has_reference = true);
    if has_reference {
        return Err(sql_err!(
            sqlstate::FEATURE_NOT_SUPPORTED,
            "cannot use column reference in DEFAULT expression"
        ));
    }
    // A literal-only default folds to a constant now; anything volatile or
    // stable (any function call) is kept as text and evaluated at insert time.
    if !expression.contains_call() {
        let catalog = crate::sql::query::storage_catalog(storage, arena, txid);
        let hooks = EvalHooks {
            group: None,
            aggs: None,
            subs: None,
            subquery_executor: None,
            windows: None,
            catalog: Some(&catalog),
            srf_index: None,
            project_sets: None,
            sequences: None,
            merge_action: None,
        };
        let v = eval_full(
            expression,
            arena,
            crate::sql::eval::NO_PARAMS,
            &NoColumns,
            &hooks,
        )?;
        let source_oid =
            crate::sql::eval::expression_type_identity(expression, &NoColumns, &hooks)?
                .routine_argument_oid();
        let v = match ctype {
            ColType::Array(crate::sql::types::ArrElem::Domain { .. })
                if source_oid == ctype.oid() =>
            {
                v
            }
            ColType::Enum(slot) => super::coerce_enum_value(v, slot, storage, txid, arena)?,
            ColType::Array(
                element @ (crate::sql::types::ArrElem::Enum(_)
                | crate::sql::types::ArrElem::Domain { .. }),
            ) => super::coerce_user_type_array(v, element, storage, txid, arena, None)?,
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
    let source = store_default_text(default_text)?;
    Ok(ColumnDefault::Expression(
        crate::sql::enum_values::checks::bind_default::<{ crate::storage::DEFAULT_EXPR_MAX }>(
            source.as_str(),
            ctype,
            storage,
            txid,
            false,
            arena,
        )?,
    ))
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
pub(super) fn resolve_cols<Definition0: crate::storage::DefinitionBacking>(
    def: &crate::storage::TableDefinition<Definition0>,
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

/// [`resolve_cols`] for the column lists of a foreign key, with
/// PostgreSQL's `transformColumnNameList` error.
fn resolve_foreign_key_cols<Definition0: crate::storage::DefinitionBacking>(
    def: &crate::storage::TableDefinition<Definition0>,
    names: &[&str],
) -> Result<([u16; MAX_INDEX_COLS], usize), SqlError> {
    if let Some(missing) = names.iter().find(|name| def.column_index(name).is_none()) {
        return Err(sql_err!(
            sqlstate::UNDEFINED_COLUMN,
            "column \"{}\" referenced in foreign key constraint does not exist",
            missing
        ));
    }
    resolve_cols(def, names)
}

/// Validates that every column reference in a CHECK predicate names a real
/// column of the table being defined, and that the predicate uses no subquery
/// (which PostgreSQL forbids in CHECK). Each referenced column's index is OR'd
/// into `cols` (bit `i` = column `i`), so the caller can name the constraint
/// the way PostgreSQL does — `<table>_<column>_check` when the predicate
/// references exactly one column, `<table>_check` otherwise.
pub(crate) fn check_referenced_columns<Definition0: crate::storage::DefinitionBacking>(
    expression: &Expr,
    def: &crate::storage::TableDefinition<Definition0>,
) -> Result<ColumnSet, SqlError> {
    let mut columns = ColumnSet::EMPTY;
    validate_check_refs(expression, def, &mut columns)?;
    Ok(columns)
}

fn validate_check_refs<Definition0: crate::storage::DefinitionBacking>(
    expression: &Expr,
    def: &crate::storage::TableDefinition<Definition0>,
    cols: &mut ColumnSet,
) -> Result<(), SqlError> {
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
            cols.insert(index);
        }
        Expr::RoutineParam { .. } => {}
        Expr::RecursiveState { .. } => {}
        Expr::Subquery(_)
        | Expr::RowSubquery { .. }
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
        | Expr::BoundDefault {
            expression: operand,
            ..
        }
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
        | Expr::BoundLiteral { .. }
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
        Expr::Field { base, .. } | Expr::RecordFieldIndex { base, .. } => {
            validate_check_refs(base, def, cols)?
        }
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
    def: &mut TableDefBuilder,
    constraints: &[TableConstraint],
    merge_equivalent: bool,
    txid: u32,
    exclude: Option<usize>,
    arena: &Arena,
) -> Result<(), SqlError> {
    // A multi-column primary key lives in `uniques`, not on a column flag, so
    // looking only at the columns would miss one a `LIKE ... INCLUDING INDEXES`
    // had already copied in and let the table end up with two.
    let mut has_primary = def.columns().iter().any(|c| c.primary)
        || def.uniques[..def.n_uniques]
            .iter()
            .any(|k| k.kind.is_primary());
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
                // An unnamed single-column key retains its admitted name on
                // the compact column flag; an explicitly named key is a first-class
                // key so DROP / RENAME CONSTRAINT and the violation message all
                // see the given name (its NOT NULL is already set above).
                if n == 1 && name.is_none() && !timing.is_deferrable() {
                    def.columns[indices[0] as usize].primary = true;
                    def.columns[indices[0] as usize].unique = true;
                } else {
                    if !merge_equivalent
                        && let Some(name) = name
                        && constraint_name_taken(def, name)
                    {
                        return Err(sql_err!(
                            sqlstate::DUPLICATE_OBJECT,
                            "constraint \"{}\" for relation \"{}\" already exists",
                            name,
                            def.name.as_str()
                        ));
                    }
                    let chosen = match name {
                        Some(name) => SqlName::parse(name)?,
                        None => auto_key_name(
                            storage,
                            def,
                            &[],
                            "pkey",
                            txid,
                            exclude,
                            crate::storage::GeneratedNameClass::IndexConstraint,
                        )?,
                    };
                    add_unique_key(
                        def,
                        chosen,
                        &indices,
                        n,
                        crate::storage::UniqueKeyKind::Primary,
                        *timing,
                    )?;
                }
            }
            TableConstraint::Unique {
                name,
                columns,
                timing,
                nulls_not_distinct,
            } => {
                let (indices, n) = resolve_cols(def, columns)?;
                let stored_timing = storage_timing(*timing);
                let equivalent_flag = n == 1
                    && !*nulls_not_distinct
                    && !stored_timing.is_deferrable()
                    && (def.columns[indices[0] as usize].unique
                        || def.columns[indices[0] as usize].primary);
                let equivalent_key = def.uniques().iter().any(|key| {
                    !key.kind.is_primary()
                        && key.kind.nulls_not_distinct() == *nulls_not_distinct
                        && key.timing == stored_timing
                        && key.columns() == &indices[..n]
                });
                if merge_equivalent && (equivalent_flag || equivalent_key) {
                    continue;
                }
                if n == 1
                    && name.is_none()
                    && !stored_timing.is_deferrable()
                    && !*nulls_not_distinct
                {
                    def.columns[indices[0] as usize].unique = true;
                } else {
                    if !merge_equivalent
                        && let Some(name) = name
                        && constraint_name_taken(def, name)
                    {
                        return Err(sql_err!(
                            sqlstate::DUPLICATE_OBJECT,
                            "constraint \"{}\" for relation \"{}\" already exists",
                            name,
                            def.name.as_str()
                        ));
                    }
                    let kind = if *nulls_not_distinct {
                        crate::storage::UniqueKeyKind::UniqueNullsNotDistinct
                    } else {
                        crate::storage::UniqueKeyKind::UniqueNullsDistinct
                    };
                    let chosen = match name {
                        Some(name) => SqlName::parse(name)?,
                        None => auto_key_name(
                            storage,
                            def,
                            &indices[..n],
                            "key",
                            txid,
                            exclude,
                            crate::storage::GeneratedNameClass::IndexConstraint,
                        )?,
                    };
                    add_unique_key(def, chosen, &indices, n, kind, *timing)?;
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
                    None => auto_check_name(storage, def, referenced_cols, txid, exclude)?,
                };
                if constraint_name_taken(def, constraint_name.as_str()) {
                    return Err(sql_err!(
                        sqlstate::DUPLICATE_OBJECT,
                        "constraint \"{}\" for relation \"{}\" already exists",
                        constraint_name.as_str(),
                        def.name.as_str()
                    ));
                }
                let mut c = CheckConstraint {
                    identity: None,
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
                delete_set_columns,
                on_update,
                timing,
                validation,
            } => {
                if !merge_equivalent
                    && let Some(name) = name
                    && constraint_name_taken(def, name)
                {
                    return Err(sql_err!(
                        sqlstate::DUPLICATE_OBJECT,
                        "constraint \"{}\" for relation \"{}\" already exists",
                        name,
                        def.name.as_str()
                    ));
                }
                attach_fkey(
                    storage,
                    def,
                    *name,
                    columns,
                    parent,
                    parent_cols,
                    (*on_delete, delete_set_columns),
                    *on_update,
                    *timing,
                    *validation,
                    txid,
                    exclude,
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
                    None => auto_key_name(
                        storage,
                        def,
                        &indices[..count],
                        "excl",
                        txid,
                        exclude,
                        crate::storage::GeneratedNameClass::IndexConstraint,
                    )?,
                };
                if !merge_equivalent && constraint_name_taken(def, exclusion.name.as_str()) {
                    return Err(sql_err!(
                        sqlstate::DUPLICATE_OBJECT,
                        "constraint \"{}\" for relation \"{}\" already exists",
                        exclusion.name.as_str(),
                        def.name.as_str()
                    ));
                }
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
    def.attributes.initialize(def.n_columns)?;
    crate::storage::constraint_names::table(def)
}

/// PostgreSQL's auto-generated constraint name: `<table>_pkey` for a primary
/// key, otherwise `<table>_<col1>_<col2>_<suffix>` over every key column.
pub(super) fn auto_key_name<Definition0: crate::storage::DefinitionBacking>(
    storage: &Storage,
    def: &crate::storage::TableDefinition<Definition0>,
    columns: &[u16],
    suffix: &str,
    transaction: u32,
    exclude: Option<usize>,
    class: crate::storage::GeneratedNameClass,
) -> Result<SqlName, SqlError> {
    use core::fmt::Write as _;
    let mut addition = StackStr::<128>::new();
    for &column in columns {
        if !addition.is_empty() {
            addition.write_char('_').expect("fixed name writer");
        }
        addition
            .write_str(def.columns()[usize::from(column)].name.as_str())
            .expect("fixed name writer");
        if addition.len() >= 64 {
            break;
        }
    }
    if addition.is_truncated() {
        return Err(sql_err!(
            sqlstate::PROGRAM_LIMIT_EXCEEDED,
            "index name columns exceed their fixed reservation"
        ));
    }
    let mut number = 0u32;
    loop {
        let mut label = StackStr::<32>::from_str(suffix);
        if number != 0 {
            write!(label, "{number}").expect("u32 generated-name suffix fits");
        }
        let name = crate::storage::constraint_names::generated(
            def.name.as_str(),
            (!addition.is_empty()).then_some(addition.as_str()),
            label.as_str(),
        )?;
        if !storage.generated_name_taken(def, name, transaction, exclude, class) {
            return Ok(name);
        }
        number = number.checked_add(1).ok_or_else(|| {
            sql_err!(
                sqlstate::PROGRAM_LIMIT_EXCEEDED,
                "generated constraint names are exhausted"
            )
        })?;
    }
}

pub(super) fn auto_check_name<Definition0: crate::storage::DefinitionBacking>(
    storage: &Storage,
    def: &crate::storage::TableDefinition<Definition0>,
    referenced_cols: ColumnSet,
    transaction: u32,
    exclude: Option<usize>,
) -> Result<SqlName, SqlError> {
    let column = [referenced_cols.first().unwrap_or(0) as u16];
    auto_key_name(
        storage,
        def,
        if referenced_cols.count_ones() == 1 {
            &column
        } else {
            &[]
        },
        "check",
        transaction,
        exclude,
        crate::storage::GeneratedNameClass::Constraint,
    )
}

/// Whether the draft already contains a constraint with this admitted name.
pub(super) fn constraint_name_taken<Definition0: crate::storage::DefinitionBacking>(
    def: &crate::storage::TableDefinition<Definition0>,
    name: &str,
) -> bool {
    if def.checks.as_ref()[..def.n_checks]
        .iter()
        .any(|c| c.name.as_str() == name)
        || def.uniques.as_ref()[..def.n_uniques]
            .iter()
            .any(|k| k.name.as_str() == name)
        || def.fkeys.as_ref()[..def.n_fkeys]
            .iter()
            .any(|f| f.name.as_str() == name)
        || def.exclusions.as_ref()[..def.n_exclusions]
            .iter()
            .any(|exclusion| exclusion.name.as_str() == name)
        || def
            .partition
            .detached_bound
            .is_some_and(|constraint| constraint.name.as_str() == name)
    {
        return true;
    }
    for (column, metadata) in def.columns().iter().enumerate() {
        if (metadata.primary || metadata.unique)
            && def
                .column_key_name(column)
                .is_some_and(|stored| stored.as_str() == name)
        {
            return true;
        }
    }
    false
}

pub(super) fn add_unique_key(
    def: &mut TableDefBuilder,
    name: SqlName,
    indices: &[u16; MAX_INDEX_COLS],
    n: usize,
    kind: crate::storage::UniqueKeyKind,
    timing: AstConstraintTiming,
) -> Result<(), SqlError> {
    if def.n_uniques == crate::storage::MAX_UNIQUES {
        return Err(sql_err!(
            sqlstate::PROGRAM_LIMIT_EXCEEDED,
            "a table can have at most {} key constraints",
            crate::storage::MAX_UNIQUES
        ));
    }
    let mut k = UniqueKey::EMPTY;
    k.name = name;
    k.columns[..n].copy_from_slice(&indices[..n]);
    k.n_cols = n;
    k.kind = kind;
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
    def: &mut TableDefBuilder,
    name: Option<&str>,
    child_cols: &[&str],
    parent: &QualName,
    parent_cols: &[&str],
    (on_delete, delete_set_names): (FkAction, &[&str]),
    on_update: FkAction,
    timing: AstConstraintTiming,
    validation: AstConstraintValidation,
    txid: u32,
    exclude: Option<usize>,
    _arena: &Arena,
) -> Result<(), SqlError> {
    if def.n_fkeys == crate::storage::MAX_FKEYS {
        return Err(sql_err!(
            sqlstate::PROGRAM_LIMIT_EXCEEDED,
            "a table can have at most {} foreign keys",
            crate::storage::MAX_FKEYS
        ));
    }
    let (child_idxs, n_child) = resolve_foreign_key_cols(def, child_cols)?;

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
    let parent_owner = if self_ref {
        None
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
        Some((*storage.table_def(pi, txid)).clone())
    };
    let parent_def = match &parent_owner {
        Some(owner) => owner.borrowed(),
        None => def.borrowed(),
    };
    let compatible_persistence = match def.persistence {
        crate::storage::RelationPersistence::Permanent => {
            parent_def.persistence == crate::storage::RelationPersistence::Permanent
        }
        crate::storage::RelationPersistence::Unlogged => {
            parent_def.persistence != crate::storage::RelationPersistence::Temporary
        }
        crate::storage::RelationPersistence::Temporary => {
            parent_def.persistence == crate::storage::RelationPersistence::Temporary
        }
    };
    if !compatible_persistence {
        let relation_kind = match def.persistence {
            crate::storage::RelationPersistence::Permanent => "permanent",
            crate::storage::RelationPersistence::Unlogged => "unlogged",
            crate::storage::RelationPersistence::Temporary => "temporary",
        };
        return Err(sql_err!(
            sqlstate::INVALID_TABLE_DEFINITION,
            "constraints on {} tables may reference only {} tables",
            relation_kind,
            match def.persistence {
                crate::storage::RelationPersistence::Unlogged => "permanent or unlogged",
                crate::storage::RelationPersistence::Permanent => "permanent",
                crate::storage::RelationPersistence::Temporary => "temporary",
            }
        ));
    }
    // `ON DELETE SET NULL (...)` names key columns, each once
    // (`validateFkOnDeleteSetColumns`).
    let (named_set_cols, n_named_set) = resolve_foreign_key_cols(def, delete_set_names)?;
    let mut delete_set_cols = [0u16; MAX_INDEX_COLS];
    let mut n_delete_set = 0;
    for (&column, name) in named_set_cols[..n_named_set].iter().zip(delete_set_names) {
        if !child_idxs[..n_child].contains(&column) {
            return Err(sql_err!(
                sqlstate::INVALID_COLUMN_REFERENCE,
                "column \"{}\" referenced in ON DELETE SET action must be part of foreign key",
                name
            ));
        }
        if !delete_set_cols[..n_delete_set].contains(&column) {
            delete_set_cols[n_delete_set] = column;
            n_delete_set += 1;
        }
    }
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
    let (parent_idxs, _) = resolve_foreign_key_cols(&parent_def, &pcol_names[..n_parent])?;
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
    let fname = match name {
        Some(n) => SqlName::parse(n)?,
        None => auto_key_name(
            storage,
            def,
            &child_idxs[..n_child],
            "fkey",
            txid,
            exclude,
            crate::storage::GeneratedNameClass::Constraint,
        )?,
    };
    for i in 0..n_child {
        check_foreign_key_column_types(
            storage,
            txid,
            fname.as_str(),
            (child_cols[i], &def.columns()[child_idxs[i] as usize]),
            (
                pcol_names[i],
                &parent_def.columns()[parent_idxs[i] as usize],
            ),
        )?;
    }
    let mut fk = ForeignKey::EMPTY;
    fk.name = fname;
    fk.columns[..n_child].copy_from_slice(&child_idxs[..n_child]);
    fk.n_cols = n_child;
    fk.parent_schema = parent_def.schema;
    fk.parent = parent_def.name;
    fk.parent_cols[..n_parent].copy_from_slice(&parent_idxs[..n_parent]);
    fk.n_parent_cols = n_parent;
    fk.on_delete = fk_action_of(on_delete);
    fk.delete_set_cols = delete_set_cols;
    fk.n_delete_set_cols = n_delete_set;
    fk.on_update = fk_action_of(on_update);
    fk.timing = storage_timing(timing);
    fk.validation = storage_validation(validation);
    def.fkeys[def.n_fkeys] = fk;
    def.n_fkeys += 1;
    Ok(())
}

/// After a column type change, every foreign key from or to `altered` must
/// still be implementable, as PostgreSQL checks when it re-adds them.
pub(super) fn check_altered_foreign_key_types(
    storage: &Storage,
    txid: u32,
    altered: &TableDefBuilder,
) -> Result<(), SqlError> {
    let is_altered =
        |schema: &SqlName, name: &SqlName| *schema == altered.schema && *name == altered.name;
    let check = |child: &TableDefBuilder, foreign_key: &ForeignKey, parent: &TableDefBuilder| {
        for (&child_column, &parent_column) in
            foreign_key.columns().iter().zip(foreign_key.parent_cols())
        {
            let referencing = &child.columns()[usize::from(child_column)];
            let referenced = &parent.columns()[usize::from(parent_column)];
            check_foreign_key_column_types(
                storage,
                txid,
                foreign_key.name.as_str(),
                (referencing.name.as_str(), referencing),
                (referenced.name.as_str(), referenced),
            )?;
        }
        Ok::<(), SqlError>(())
    };
    for foreign_key in altered.fkeys() {
        if is_altered(&foreign_key.parent_schema, &foreign_key.parent) {
            check(altered, foreign_key, altered)?;
        } else if let Some(parent) = storage.find_visible(
            foreign_key.parent_schema.as_str(),
            foreign_key.parent.as_str(),
            txid,
        ) {
            let parent = TableDefBuilder::from_definition(&storage.table_def(parent, txid));
            check(altered, foreign_key, &parent)?;
        }
    }
    for table in 0..storage.table_count() {
        if !storage.table(table).visible_to(txid) {
            continue;
        }
        let child = storage.table_def(table, txid);
        if is_altered(&child.schema, &child.name) {
            continue;
        }
        if !child
            .fkeys()
            .iter()
            .any(|foreign_key| is_altered(&foreign_key.parent_schema, &foreign_key.parent))
        {
            continue;
        }
        let child = TableDefBuilder::from_definition(&child);
        for foreign_key in child.fkeys() {
            if is_altered(&foreign_key.parent_schema, &foreign_key.parent) {
                check(&child, foreign_key, altered)?;
            }
        }
    }
    Ok(())
}

/// Whether a referencing and a referenced key column can be compared, as
/// PostgreSQL's `ATAddForeignKeyConstraint` decides (see
/// [`super::foreign_key_equality`]).
fn check_foreign_key_column_types(
    storage: &Storage,
    txid: u32,
    constraint: &str,
    (referencing_name, referencing): (&str, &crate::storage::ColumnMeta),
    (referenced_name, referenced): (&str, &crate::storage::ColumnMeta),
) -> Result<(), SqlError> {
    if super::foreign_key_equality::key_equality(referencing.ctype, referenced.ctype).is_none() {
        let type_oid = |column: &crate::storage::ColumnMeta| {
            storage
                .routine_type_oid(column.ctype, column.user_type, txid)
                .ok_or_else(|| {
                    sql_err!(
                        sqlstate::INTERNAL_ERROR,
                        "column type identity is unavailable"
                    )
                })
        };
        let (referencing_oid, referenced_oid) = (type_oid(referencing)?, type_oid(referenced)?);
        if let Some(input) = super::foreign_key_equality::operator_class_input(referenced.ctype)
            .and_then(ColType::try_oid)
            && let Some(from) = referencing.ctype.try_oid()
            && storage
                .cast_for_oids(from, input, txid)
                .is_some_and(|(_, cast)| cast.context.permits_implicit())
        {
            return Err(sql_err!(
                sqlstate::FEATURE_NOT_SUPPORTED,
                "foreign key constraint \"{}\" through a user-defined implicit cast is not supported",
                constraint
            ));
        }
        let detail = crate::stack_format!(
            crate::sql::eval::MAX_DIAGNOSTIC_DETAIL_BYTES,
            "Key columns \"{}\" of the referencing table and \"{}\" of the referenced table are of incompatible types: {} and {}.",
            referencing_name,
            referenced_name,
            crate::sql::catalog::format_type_name(storage, txid, referencing_oid)?.as_str(),
            crate::sql::catalog::format_type_name(storage, txid, referenced_oid)?.as_str()
        );
        crate::sql::eval::stash_diagnostic(Some(detail), None);
        return Err(sql_err!(
            sqlstate::DATATYPE_MISMATCH,
            "foreign key constraint \"{}\" cannot be implemented",
            constraint
        ));
    }
    // PostgreSQL reports this as an internal error after accepting the types
    // (a `"char"` key referencing `text`).
    if referencing.ctype.is_collatable() != referenced.ctype.is_collatable() {
        return Err(sql_err!(
            sqlstate::INTERNAL_ERROR,
            "key columns are not both collatable"
        ));
    }
    Ok(())
}

/// The column indices forming the table's primary key (column flags or a
/// multi-column PRIMARY KEY constraint); the count is 0 if none.
pub(super) fn primary_key_cols<Definition0: crate::storage::DefinitionBacking>(
    def: &crate::storage::TableDefinition<Definition0>,
) -> ([u16; MAX_INDEX_COLS], usize) {
    let mut out = [0u16; MAX_INDEX_COLS];
    for uk in def.uniques() {
        if uk.kind.is_primary() {
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
pub(super) fn references_named_key<Definition0: crate::storage::DefinitionBacking>(
    def: &crate::storage::TableDefinition<Definition0>,
    name: &str,
    columns: &[u16],
) -> bool {
    if columns.len() == 1 {
        let column = usize::from(columns[0]);
        let metadata = &def.columns()[column];
        if metadata.primary || metadata.unique {
            return def
                .column_key_name(column)
                .is_some_and(|stored| stored.as_str() == name);
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

fn is_unique_key<Definition0: crate::storage::DefinitionBacking>(
    def: &crate::storage::TableDefinition<Definition0>,
    columns: &[u16],
) -> bool {
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
