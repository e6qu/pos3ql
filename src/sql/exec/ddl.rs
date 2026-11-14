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
        // CREATE TABLE positions its columns' default errors; ALTER TABLE
        // ADD COLUMN does not.
        def.columns[i] =
            crate::sql::error_position::with_analysis(|| build_column(c, storage, txid, arena))?;
        if let Some(name) = c.not_null_name {
            def.set_not_null_constraint_name(i, SqlName::parse(name)?);
        }
    }
    // A generation expression reads the other columns, so it is analysed
    // once the table is assembled, positioned in the statement's text.
    crate::sql::error_position::with_analysis(|| {
        validate_generated_refs(&def, Some(columns), storage, txid, arena)
    })?;
    Ok(def)
}

/// PostgreSQL's `cookDefault` for each `GENERATED` column: the expression's
/// columns, operators and constants resolve against the table, it may not
/// reference another generated column (or itself, 42P17), and its type must
/// convert to the column's by assignment. `sources` are the columns as
/// written, whose generation text is positioned in the statement; without
/// them the stored text is analysed.
pub(super) fn validate_generated_refs<Definition0: crate::storage::DefinitionBacking>(
    def: &crate::storage::TableDefinition<Definition0>,
    sources: Option<&[ColumnDef]>,
    storage: &Storage,
    txid: u32,
    arena: &Arena,
) -> Result<(), SqlError> {
    for c in def.columns() {
        if !c.default.is_generated() {
            continue;
        }
        let default = c.default.expression();
        let stored = default.as_ref().expect("generated column has expr text");
        let text = sources
            .and_then(|columns| {
                columns
                    .iter()
                    .find(|column| column.name == c.name.as_str())?
                    .generated_text
            })
            .unwrap_or(stored.as_str());
        let expression = crate::sql::parser::parse_expr(text, arena)?;
        let column_oid = storage
            .routine_type_oid(c.ctype, c.user_type, txid)
            .unwrap_or_else(|| c.ctype.oid());
        let user_type = |type_oid: i32| crate::storage::NamedTypeKind::from_oid(type_oid);
        if c.default.is_virtual() && (c.primary || c.unique) {
            return Err(sql_err!(
                sqlstate::FEATURE_NOT_SUPPORTED,
                "{} on virtual generated columns are not supported",
                if c.primary {
                    "primary keys"
                } else {
                    "unique constraints"
                }
            ));
        }
        if c.default.is_virtual() {
            // PostgreSQL 18 limits what a virtual column may compute with.
            match user_type(column_oid) {
                Some((crate::storage::NamedTypeKind::Domain, _, false)) => {
                    return Err(sql_err!(
                        sqlstate::FEATURE_NOT_SUPPORTED,
                        "virtual generated column \"{}\" cannot have a domain type",
                        c.name.as_str()
                    ));
                }
                Some(_) => {
                    return Err(crate::sql::error_position::with_detail(
                        sql_err!(
                            sqlstate::FEATURE_NOT_SUPPORTED,
                            "virtual generated column \"{}\" cannot have a user-defined type",
                            c.name.as_str()
                        ),
                        format_args!(
                            "Virtual generated columns that make use of user-defined types are not yet supported."
                        ),
                    ));
                }
                None => {}
            }
            if let Some(function) = expression.find_function(&|name| {
                let name = name.rsplit('.').next().unwrap_or(name);
                crate::sql::catalog::builtin_callable_signatures(name)
                    .next()
                    .is_none()
            }) {
                return Err(crate::sql::error_position::with_detail(
                    crate::sql::error_position::at_reference(
                        sql_err!(
                            sqlstate::FEATURE_NOT_SUPPORTED,
                            "generation expression uses user-defined function"
                        ),
                        function,
                    ),
                    format_args!(
                        "Virtual generated columns that make use of user-defined functions are not yet supported."
                    ),
                ));
            }
        }
        let columns = super::CatalogCols {
            definition: Some(crate::storage::RelationDefinition::from_definition(def)),
            alias: None,
            output_aliases: &[],
            storage,
            txid,
            arena,
        };
        let (source_oid, _) = super::infer_type_res(expression, &columns)?;
        crate::sql::query::check_literal_operands_typed(
            expression,
            &|other| {
                super::infer_type_res(other, &columns)
                    .ok()
                    .and_then(|(oid, _)| ColType::from_oid(oid))
            },
            storage,
            txid,
            arena,
        )?;
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
        if c.default.is_virtual() && user_type(source_oid).is_some() {
            return Err(crate::sql::error_position::with_detail(
                sql_err!(
                    sqlstate::FEATURE_NOT_SUPPORTED,
                    "generation expression uses user-defined type"
                ),
                format_args!(
                    "Virtual generated columns that make use of user-defined types are not yet supported."
                ),
            ));
        }
        let target_oid = column_oid;
        if !crate::sql::catalog::can_coerce_assignment(storage, txid, source_oid, target_oid) {
            return Err(crate::sql::error_position::with_hint(
                sql_err!(
                    sqlstate::DATATYPE_MISMATCH,
                    "column \"{}\" is of type {} but default expression is of type {}",
                    c.name.as_str(),
                    crate::sql::catalog::format_type_name(storage, txid, target_oid)?.as_str(),
                    crate::sql::catalog::format_type_name(storage, txid, source_oid)?.as_str()
                ),
                "You will need to rewrite or cast the expression.",
            ));
        }
        // A cast or operator PostgreSQL marks stable (`a::text::regclass`)
        // makes the expression depend on more than the row; PostgreSQL checks
        // this after the expression's type.
        if crate::sql::planner_inputs::inline::index_expression_is_mutable(
            expression, &columns, storage, txid, arena,
        )?
        .unwrap_or(false)
        {
            return Err(sql_err!(
                sqlstate::INVALID_OBJECT_DEFINITION,
                "generation expression is not immutable"
            ));
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
        local: true,
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
    let missing_type = || {
        crate::sql::error_position::at_subject(
            crate::sql::type_name_miss::TypeNameMiss::in_storage(storage, txid, c.type_name)
                .error(),
            c.type_name,
        )
    };
    if let Some(pseudo) = crate::sql::types::pseudo_type_column_name(c.type_name) {
        return Err(sql_err!(
            sqlstate::INVALID_TABLE_DEFINITION,
            "column \"{}\" has pseudo-type {}",
            c.name,
            pseudo
        ));
    }
    // A base type resolves statically; an unknown name falls back to the
    // domain catalog (base type + identity) then the enum catalog (its own
    // `ColType::Enum` plus its durable identity, so the column's
    // type persists as a name that reloads to the right slot). User-defined
    // array types follow the same path while keeping their element identity.
    let builtin_domain = crate::sql::catalog::user_type_oid(storage, txid, c.type_name)
        .and_then(crate::sql::information_schema_domains::InformationSchemaDomain::from_oid);
    let (ctype, type_mod, user_type, domain_default) = match ColType::from_sql_name(c.type_name) {
        Some(ct) => (ct, c.type_mod, None, None),
        None if let Some((domain, array)) = builtin_domain => {
            let identity = crate::storage::UserTypeName::information_schema_domain(domain);
            if array {
                (
                    ColType::Array(
                        crate::sql::types::ArrElem::from_coltype(domain.base())
                            .expect("information_schema domain base has an array type"),
                    ),
                    -1,
                    Some(identity),
                    None,
                )
            } else {
                (
                    domain.base(),
                    domain.base_type_mod(),
                    Some(identity),
                    domain.default_text().map(
                        crate::util::StackStr::<{ crate::storage::DEFAULT_EXPR_MAX }>::from_str,
                    ),
                )
            }
        }
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
                } else if let Some(relation) = storage.resolve_relation_row_type(element_name, txid)
                {
                    let result = storage.relation_row_type_result(relation, true, txid)?;
                    (result.ctype, -1, result.user_type, None)
                } else {
                    return Err(missing_type());
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
                            None => match storage.resolve_relation_row_type(c.type_name, txid) {
                                Some(relation) => {
                                    let result =
                                        storage.relation_row_type_result(relation, false, txid)?;
                                    (result.ctype, -1, result.user_type, None)
                                }
                                None => {
                                    return Err(missing_type());
                                }
                            },
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
    if let Some(identity) = user_type
        && identity.relation_row_type_creation().is_none()
        && identity.information_schema_domain_binding().is_none()
    {
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
        ColumnDefault::Generated {
            expression: resolve_generated(gtext, storage, txid, arena)?,
            kind: c.generated_kind,
        }
    } else {
        let column_default = resolve_default(
            c.name,
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
        match column_default {
            ColumnDefault::None => domain_default
                .map(ColumnDefault::Expression)
                .unwrap_or(ColumnDefault::None),
            // PostgreSQL stores no default for a NULL constant unless it
            // must override a domain's own default.
            ColumnDefault::Constant {
                value: crate::storage::OwnedDatum::Null,
                ..
            } if domain_default.is_none() => ColumnDefault::None,
            column_default => column_default,
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
    validate_column_storage(ctype, column_storage, c.compression)?;
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
        local: true,
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
    ctype: ColType,
    storage: crate::sql::ast::ColumnStorage,
    compression: crate::sql::ast::ColumnCompression,
) -> Result<(), SqlError> {
    let toastable = ctype.physical_layout().storage != b'p';
    if !toastable && storage != crate::sql::ast::ColumnStorage::Plain {
        return Err(sql_err!(
            sqlstate::FEATURE_NOT_SUPPORTED,
            "column data type {} can only have storage PLAIN",
            ctype.name()
        ));
    }
    if compression != crate::sql::ast::ColumnCompression::Default && !toastable {
        return Err(sql_err!(
            sqlstate::FEATURE_NOT_SUPPORTED,
            "column data type {} does not support compression",
            ctype.name()
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
    storage: &Storage,
    txid: u32,
    arena: &Arena,
) -> Result<StackStr<{ crate::storage::DEFAULT_EXPR_MAX }>, SqlError> {
    let expression = crate::sql::parser::parse_expr(text, arena)?;
    if expression.contains_subquery() {
        return Err(sql_err!(
            sqlstate::FEATURE_NOT_SUPPORTED,
            "cannot use subquery in column generation expression"
        ));
    }
    if expression
        .contains_nonimmutable_function_with(&|name| {
            storage.builtin_routine_name_is_nonimmutable(name, txid)
        })
        .is_some()
    {
        return Err(sql_err!(
            sqlstate::INVALID_OBJECT_DEFINITION,
            "generation expression is not immutable"
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
#[expect(clippy::too_many_arguments, reason = "DDL column context")]
pub(super) fn resolve_default(
    column: &str,
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
    let mut reference = None;
    expression.for_each_column_reference(&mut |qualifier, name| {
        if reference.is_none() {
            reference = Some(crate::sql::error_position::at_reference(
                sql_err!(
                    sqlstate::FEATURE_NOT_SUPPORTED,
                    "cannot use column reference in DEFAULT expression"
                ),
                qualifier.unwrap_or(name),
            ));
        }
    });
    if let Some(error) = reference {
        return Err(error);
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
        let source_oid =
            crate::sql::eval::expression_type_identity(expression, &NoColumns, &hooks)?
                .routine_argument_oid();
        let target_oid = ctype.oid();
        if source_oid != crate::sql::types::oid::UNKNOWN
            && !crate::sql::catalog::can_coerce_assignment(storage, txid, source_oid, target_oid)
        {
            return Err(crate::sql::error_position::with_hint(
                sql_err!(
                    sqlstate::DATATYPE_MISMATCH,
                    "column \"{}\" is of type {} but default expression is of type {}",
                    column,
                    crate::sql::catalog::format_type_name(storage, txid, target_oid)?.as_str(),
                    crate::sql::catalog::format_type_name(storage, txid, source_oid)?.as_str()
                ),
                "You will need to rewrite or cast the expression.",
            ));
        }
        let v = eval_full(
            expression,
            arena,
            crate::sql::eval::NO_PARAMS,
            &NoColumns,
            &hooks,
        )?;
        let literal_input = |error: SqlError| match expression {
            Expr::Str(text) => crate::sql::error_position::at_reference(error, text),
            _ => error,
        };
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
            _ => cast_to(v, ctype, arena).map_err(literal_input)?,
        };
        // A value its type modifier rejects fails at each insert, as
        // PostgreSQL applies the modifier when the default is used.
        if let Ok(v) = apply_typmod(v, ctype, type_mod, arena) {
            return Ok(ColumnDefault::Constant {
                value: OwnedDatum::from_datum(&v)?,
                expression: store_default_text(default_text)?,
            });
        }
    }
    // The expression is analysed when the column is defined: its functions
    // resolve and its literals run their inputs.
    let mark = arena.mark();
    let analysed = super::infer_type_res(
        expression,
        &super::CatalogCols {
            definition: None,
            alias: None,
            output_aliases: &[],
            storage,
            txid,
            arena,
        },
    );
    // Only the verdict leaves; analysis scratch is discarded.
    unsafe { arena.rewind_to(mark) };
    analysed?;
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

/// The distinct variables of an expression over one relation. A whole-row
/// reference (`t.*`, or the bare relation name) is attribute 0; `ROW(t.*)`
/// and `(t).column` read the named columns instead.
pub(crate) struct ExpressionVariables<'v> {
    columns: ColumnSet,
    whole_row: bool,
    count: usize,
    /// The first variable: `Some(column)`, or `None` for the whole row.
    first: Option<Option<usize>>,
    /// Sees each variable once, in the order PostgreSQL's `pull_var_clause`
    /// first meets it.
    visit: Option<&'v mut dyn FnMut(Option<usize>)>,
}

impl ExpressionVariables<'_> {
    pub(crate) const fn columns(&self) -> ColumnSet {
        self.columns
    }

    pub(crate) const fn has_whole_row(&self) -> bool {
        self.whole_row
    }

    /// PostgreSQL names a CHECK after its column only when the predicate has
    /// exactly one variable and that variable is a column.
    pub(crate) fn single_column(&self) -> Option<usize> {
        if self.count == 1 {
            self.first.flatten()
        } else {
            None
        }
    }

    fn add(&mut self, variable: Option<usize>) {
        let new = match variable {
            Some(column) => !self.columns.contains(column),
            None => !self.whole_row,
        };
        if !new {
            return;
        }
        match variable {
            Some(column) => self.columns.insert(column),
            None => self.whole_row = true,
        }
        self.first.get_or_insert(variable);
        self.count += 1;
        if let Some(visit) = self.visit.as_mut() {
            visit(variable);
        }
    }

    fn add_column(&mut self, column: usize) {
        self.add(Some(column));
    }

    fn add_whole_row(&mut self) {
        self.add(None);
    }
}

/// Respells a stored single-relation expression's references to its relation
/// after the relation is renamed: qualifiers (`old.c`, `old.*`, `s.old.c`)
/// and bare whole-row references (`f(old)`), which a column of the same name
/// would shadow. PostgreSQL's analyzed form names the relation, not its name.
pub(crate) fn rename_whole_row_references<const N: usize>(
    text: &StackStr<N>,
    old: &str,
    new: &str,
    shadowed: bool,
    arena: &Arena,
) -> Result<StackStr<N>, SqlError> {
    use crate::sql::lexer::{Lexer, Tok};
    use core::fmt::Write as _;
    let mark = arena.mark();
    let result = (|| {
        let source = text.as_str();
        let mut lexer = Lexer::new(source, arena);
        let mut previous = [Tok::Eof; 3];
        let mut copied = 0;
        let mut output = StackStr::<N>::new();
        let invalid = |error: crate::sql::lexer::LexError| {
            sql_err!(
                error.kind.sqlstate(),
                "invalid stored expression: {}",
                error.message
            )
        };
        loop {
            let token = lexer.next_token().map_err(invalid)?;
            if token == Tok::Eof {
                break;
            }
            let (start, end) = (lexer.token_start(), lexer.token_end());
            let mut ahead = lexer.clone();
            let next = ahead.next_token().map_err(invalid)?;
            let named = matches!(token, Tok::Ident(name) | Tok::QuotedIdent(name) if name == old);
            // A name after a dot is a field or column unless the dot follows
            // a lone schema qualifier (`s.old.c`).
            let after_dot = previous[0] == Tok::Op(".")
                && !(matches!(previous[1], Tok::Ident(_) | Tok::QuotedIdent(_))
                    && previous[2] != Tok::Op(".")
                    && next == Tok::Op("."));
            let relation =
                named && !after_dot && next != Tok::Op("(") && (next == Tok::Op(".") || !shadowed);
            if relation {
                let _ = output.write_str(&source[copied..start]);
                crate::sql::catalog::write_identifier(&mut output, new);
                copied = end;
            }
            previous = [token, previous[0], previous[1]];
        }
        let _ = output.write_str(&source[copied..]);
        if output.is_truncated() {
            return Err(sql_err!(
                sqlstate::PROGRAM_LIMIT_EXCEEDED,
                "renamed stored expression exceeds {} bytes",
                N
            ));
        }
        Ok(output)
    })();
    // SAFETY: the lexer's folded identifiers do not outlive this call.
    unsafe { arena.rewind_to(mark) };
    result
}

/// The variables a stored index's key expressions and predicate read.
fn index_expression_variables<Definition0: crate::storage::DefinitionBacking>(
    table: &crate::storage::TableDefinition<Definition0>,
    index: &crate::storage::IndexDef,
    arena: &Arena,
) -> Result<(ColumnSet, bool), SqlError> {
    let mut columns = ColumnSet::EMPTY;
    let mut whole_row = false;
    for text in index
        .expressions
        .iter()
        .flatten()
        .map(|expression| expression.as_str())
        .chain(index.predicate.as_ref().map(|predicate| predicate.as_str()))
    {
        let mark = arena.mark();
        let found = crate::sql::parser::parse_expr(text, arena)
            .and_then(|expression| expression_variables(expression, table))
            .map(|variables| (variables.columns(), variables.has_whole_row()));
        // SAFETY: nothing parsed above outlives this check.
        unsafe { arena.rewind_to(mark) };
        let (found_columns, found_whole_row) = found?;
        columns |= found_columns;
        whole_row |= found_whole_row;
    }
    Ok((columns, whole_row))
}

/// Whether a stored index key or predicate reads the whole row.
pub(crate) fn index_reads_whole_row<Definition0: crate::storage::DefinitionBacking>(
    table: &crate::storage::TableDefinition<Definition0>,
    index: &crate::storage::IndexDef,
    arena: &Arena,
) -> Result<bool, SqlError> {
    Ok(index_expression_variables(table, index, arena)?.1)
}

/// PostgreSQL's `MergeAttributes` cannot map a whole-row variable of a
/// copied CHECK constraint onto the new relation's row type.
pub(crate) fn require_convertible_check<Definition0: crate::storage::DefinitionBacking>(
    source: &crate::storage::TableDefinition<Definition0>,
    check: &CheckConstraint,
    arena: &Arena,
) -> Result<(), SqlError> {
    let mark = arena.mark();
    let whole_row = crate::sql::parser::parse_expr(check.expression.as_str(), arena)
        .and_then(|expression| expression_variables(expression, source))
        .map(|variables| variables.has_whole_row());
    // SAFETY: nothing parsed above outlives this check.
    unsafe { arena.rewind_to(mark) };
    if whole_row? {
        return Err(crate::sql::eval::with_detail(
            sql_err!(
                sqlstate::FEATURE_NOT_SUPPORTED,
                "cannot convert whole-row table reference"
            ),
            format_args!(
                "Constraint \"{}\" contains a whole-row reference to table \"{}\".",
                check.name.as_str(),
                source.name.as_str()
            ),
        ));
    }
    Ok(())
}

/// The same refusal for an index that `LIKE ... INCLUDING INDEXES` or a new
/// partition would copy (`generateClonedIndexStmt`).
pub(crate) fn require_convertible_index<Definition0: crate::storage::DefinitionBacking>(
    source: &crate::storage::TableDefinition<Definition0>,
    index: &crate::storage::IndexDef,
    name: &str,
    arena: &Arena,
) -> Result<(), SqlError> {
    if index_reads_whole_row(source, index, arena)? {
        return Err(crate::sql::eval::with_detail(
            sql_err!(
                sqlstate::FEATURE_NOT_SUPPORTED,
                "cannot convert whole-row table reference"
            ),
            format_args!("Index \"{}\" contains a whole-row table reference.", name),
        ));
    }
    Ok(())
}

/// PostgreSQL's `CheckMutability` for an index key or predicate (`42P17`):
/// after the planner's SQL-function inlining, every function, operator and
/// cast it runs must be immutable, so `stamp::date` on a `timestamptz` column
/// and a PL/pgSQL `STABLE` function are refused. `what` is `expression` or
/// `predicate`. A step with no known implementation keeps the name rule for
/// built-in non-immutable functions.
pub(crate) fn require_immutable_index_expression<Definition0: crate::storage::DefinitionBacking>(
    expression: &Expr,
    table: &crate::storage::TableDefinition<Definition0>,
    storage: &Storage,
    txid: u32,
    what: &str,
    arena: &Arena,
) -> Result<(), SqlError> {
    let columns = super::CatalogCols {
        definition: Some(crate::storage::RelationDefinition::from_definition(table)),
        alias: None,
        output_aliases: &[],
        storage,
        txid,
        arena,
    };
    let mutable = match crate::sql::planner_inputs::inline::index_expression_is_mutable(
        expression, &columns, storage, txid, arena,
    )? {
        Some(mutable) => mutable,
        None => expression
            .contains_nonimmutable_function_with(&|name| {
                storage.builtin_routine_name_is_nonimmutable(name, txid)
            })
            .is_some(),
    };
    if mutable {
        return Err(sql_err!(
            sqlstate::INVALID_OBJECT_DEFINITION,
            "functions in index {} must be marked IMMUTABLE",
            what
        ));
    }
    Ok(())
}

/// The column a single-relation expression is when it is nothing but a
/// column reference: `t.a`, `(t).a`, `(t.*).a`, or a column cast to its own
/// type.
pub(crate) fn plain_column_reference<Definition0: crate::storage::DefinitionBacking>(
    expression: &Expr,
    def: &crate::storage::TableDefinition<Definition0>,
) -> Result<Option<usize>, SqlError> {
    match expression {
        // A cast to the column's own unconstrained built-in type is no
        // conversion: the parser keeps the column itself.
        Expr::Cast {
            operand,
            type_name,
            type_mod: -1,
            ..
        } => {
            let column = match operand {
                Expr::Column {
                    qualifier: None,
                    name,
                } => def.column_index(name),
                _ => plain_column_reference(operand, def)?,
            };
            Ok(column.filter(|&column| {
                let meta = def.columns()[column];
                meta.user_type.is_none()
                    && meta.type_mod < 0
                    && ColType::from_sql_name(type_name) == Some(meta.ctype)
            }))
        }
        Expr::Column {
            qualifier: Some(qualifier),
            name,
        } => {
            relation_qualifier(qualifier, def)?;
            Ok(def.column_index(name))
        }
        Expr::Field { base, field } if *field != "*" && names_relation(base, def)? => {
            Ok(def.column_index(field))
        }
        _ => Ok(None),
    }
}

/// The stored spelling of a validated single-relation expression (a CHECK
/// predicate, index key or index predicate). Column references lose their
/// relation qualifier and a whole-row reference is spelled `relation.*`, so
/// the text survives being copied to another relation and adding a column
/// that shares the relation's name, as PostgreSQL's analyzed form does.
pub(crate) fn stored_relation_expression<'a, Definition0: crate::storage::DefinitionBacking>(
    text: &'a str,
    def: &crate::storage::TableDefinition<Definition0>,
    arena: &'a Arena,
) -> Result<&'a str, SqlError> {
    use crate::sql::lexer::{Lexer, Tok};
    use core::fmt::Write as _;
    let full = crate::sql::eval::arena_full;
    let (_, sources) = crate::sql::parser::parse_expr_with_column_sources(text, arena)?;
    let mut tokens = crate::mem::arena::ArenaList::new(arena);
    let mut lexer = Lexer::new(text, arena);
    loop {
        let token = lexer.next_token().map_err(|error| {
            sql_err!(
                error.kind.sqlstate(),
                "invalid stored expression: {}",
                error.message
            )
        })?;
        if token == Tok::Eof {
            break;
        }
        tokens
            .push((token, lexer.token_start(), lexer.token_end()))
            .map_err(|_| full())?;
    }
    let tokens = tokens.as_slice();
    let token = |index: usize| tokens.get(index).map(|(token, _, _)| *token);
    let relation = |index: usize| {
        matches!(token(index), Some(Tok::Ident(name) | Tok::QuotedIdent(name))
            if name == def.name.as_str())
    };
    let field = |index: usize| {
        matches!(token(index), Some(Tok::Ident(_) | Tok::QuotedIdent(_)))
            && token(index + 1) != Some(Tok::Op("("))
    };
    // A bare relation name the parser read as a column reference.
    let bare = |index: usize| {
        def.column_index(def.name.as_str()).is_none()
            && sources.iter().any(|source| {
                source.qualifier_parts == 0
                    && source.start == tokens[index].1
                    && source.name == def.name.as_str()
            })
    };
    let qualified = |index: usize| {
        sources
            .iter()
            .any(|source| source.qualifier_parts == 1 && source.start == tokens[index].1)
    };
    let mut output = crate::mem::arena::ArenaString::new(arena);
    let mut copied = 0;
    let mut index = 0;
    while index < tokens.len() {
        let (_, start, _) = tokens[index];
        // `(t).a` and `(t.*).a` select a column; `t.a` qualifies one; a
        // bare `t` is the whole row.
        let (skip_end, replacement, next) = if token(index) == Some(Tok::Op("("))
            && relation(index + 1)
            && bare(index + 1)
            && token(index + 2) == Some(Tok::Op(")"))
            && token(index + 3) == Some(Tok::Op("."))
            && field(index + 4)
        {
            (tokens[index + 4].1, false, index + 4)
        } else if token(index) == Some(Tok::Op("("))
            && relation(index + 1)
            && token(index + 2) == Some(Tok::Op("."))
            && token(index + 3) == Some(Tok::Op("*"))
            && token(index + 4) == Some(Tok::Op(")"))
            && token(index + 5) == Some(Tok::Op("."))
            && field(index + 6)
        {
            (tokens[index + 6].1, false, index + 6)
        } else if relation(index)
            && token(index + 1) == Some(Tok::Op("."))
            && field(index + 2)
            && qualified(index + 2)
        {
            (tokens[index + 2].1, false, index + 2)
        } else if relation(index) && bare(index) {
            (tokens[index].2, true, index + 1)
        } else {
            index += 1;
            continue;
        };
        output.write_str(&text[copied..start]).map_err(|_| full())?;
        if replacement {
            let mut name = crate::util::StackStr::<130>::new();
            crate::sql::catalog::write_identifier(&mut name, def.name.as_str());
            output.write_str(name.as_str()).map_err(|_| full())?;
            output.write_str(".*").map_err(|_| full())?;
        }
        copied = skip_end;
        index = next;
    }
    output.write_str(&text[copied..]).map_err(|_| full())?;
    Ok(output.as_str())
}

/// Validates that every column reference in a CHECK predicate, index
/// expression or predicate names a real column of the table being defined,
/// and that the predicate uses no subquery (which PostgreSQL forbids there).
/// Returns the referenced columns; a whole-row reference adds none.
pub(crate) fn check_referenced_columns<Definition0: crate::storage::DefinitionBacking>(
    expression: &Expr,
    def: &crate::storage::TableDefinition<Definition0>,
) -> Result<ColumnSet, SqlError> {
    Ok(expression_variables(expression, def)?.columns())
}

/// [`check_referenced_columns`] with the whole-row reference and the variable
/// count PostgreSQL's generated constraint names use.
pub(crate) fn expression_variables<Definition0: crate::storage::DefinitionBacking>(
    expression: &Expr,
    def: &crate::storage::TableDefinition<Definition0>,
) -> Result<ExpressionVariables<'static>, SqlError> {
    let mut variables = ExpressionVariables {
        columns: ColumnSet::EMPTY,
        whole_row: false,
        count: 0,
        first: None,
        visit: None,
    };
    validate_check_refs(expression, def, &mut variables)?;
    Ok(variables)
}

/// Visits each distinct variable of `expression` once, in the order
/// PostgreSQL's `pull_var_clause` first meets it (its `conkey` order).
pub(crate) fn visit_expression_variables<Definition0: crate::storage::DefinitionBacking>(
    expression: &Expr,
    def: &crate::storage::TableDefinition<Definition0>,
    visit: &mut dyn FnMut(Option<usize>),
) -> Result<(), SqlError> {
    let mut variables = ExpressionVariables {
        columns: ColumnSet::EMPTY,
        whole_row: false,
        count: 0,
        first: None,
        visit: Some(visit),
    };
    validate_check_refs(expression, def, &mut variables)
}

/// Whether `name` spells the relation itself rather than one of its columns:
/// PostgreSQL resolves a bare name that is no column to the whole row.
fn names_relation<Definition0: crate::storage::DefinitionBacking>(
    expression: &Expr,
    def: &crate::storage::TableDefinition<Definition0>,
) -> Result<bool, SqlError> {
    match expression {
        Expr::WholeRow(table) => {
            relation_qualifier(table, def)?;
            Ok(true)
        }
        Expr::Column {
            qualifier: None,
            name,
        } => Ok(def.column_index(name).is_none() && *name == def.name.as_str()),
        _ => Ok(false),
    }
}

fn relation_qualifier<Definition0: crate::storage::DefinitionBacking>(
    table: &str,
    def: &crate::storage::TableDefinition<Definition0>,
) -> Result<(), SqlError> {
    if table == def.name.as_str() {
        Ok(())
    } else {
        Err(sql_err!(
            sqlstate::UNDEFINED_TABLE,
            "missing FROM-clause entry for table \"{}\"",
            table
        ))
    }
}

fn validate_check_refs<Definition0: crate::storage::DefinitionBacking>(
    expression: &Expr,
    def: &crate::storage::TableDefinition<Definition0>,
    variables: &mut ExpressionVariables<'_>,
) -> Result<(), SqlError> {
    match expression {
        Expr::SchemaColumn { table, .. } => {
            return Err(sql_err!(
                sqlstate::UNDEFINED_TABLE,
                "invalid reference to FROM-clause entry for table \"{}\"",
                table
            ));
        }
        Expr::WholeRow(table) => {
            relation_qualifier(table, def)?;
            variables.add_whole_row();
        }
        Expr::Column { qualifier, name } => {
            if let Some(qualifier) = qualifier {
                relation_qualifier(qualifier, def)?;
            }
            match def.column_index(name) {
                Some(index) => variables.add_column(index),
                None if qualifier.is_none() && *name == def.name.as_str() => {
                    variables.add_whole_row()
                }
                None => {
                    return Err(crate::sql::column_suggestion::missing_relation_column(
                        *qualifier,
                        name,
                        def.name.as_str(),
                        &def.columns.as_ref()[..def.n_columns],
                    ));
                }
            }
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
        | Expr::IsNull { operand, .. } => validate_check_refs(operand, def, variables)?,
        Expr::Binary { left, right, .. } => {
            validate_check_refs(left, def, variables)?;
            validate_check_refs(right, def, variables)?;
        }
        Expr::Call { name, args, .. } if name.eq_ignore_ascii_case("row") => {
            // `ROW(t.*)` and `ROW((t).*)` expand to the relation's columns.
            for argument in *args {
                let expansion = match argument {
                    Expr::WholeRow(_) => Some(*argument),
                    Expr::Field { base, field: "*" } => Some(*base),
                    _ => None,
                };
                match expansion {
                    Some(base) if names_relation(base, def)? => {
                        for column in 0..def.n_columns {
                            variables.add_column(column);
                        }
                    }
                    _ => validate_check_refs(argument, def, variables)?,
                }
            }
        }
        Expr::Call { args, .. } => {
            for a in *args {
                validate_check_refs(a, def, variables)?;
            }
        }
        Expr::InList { operand, list, .. } => {
            validate_check_refs(operand, def, variables)?;
            for a in *list {
                validate_check_refs(a, def, variables)?;
            }
        }
        Expr::Like {
            operand, pattern, ..
        }
        | Expr::Match {
            operand, pattern, ..
        } => {
            validate_check_refs(operand, def, variables)?;
            validate_check_refs(pattern, def, variables)?;
        }
        Expr::Case {
            operand,
            whens,
            otherwise,
            ..
        } => {
            if let Some(o) = operand {
                validate_check_refs(o, def, variables)?;
            }
            for (w, t) in *whens {
                validate_check_refs(w, def, variables)?;
                validate_check_refs(t, def, variables)?;
            }
            if let Some(o) = otherwise {
                validate_check_refs(o, def, variables)?;
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
        | Expr::OuterAggregate { .. }
        | Expr::DefaultMarker => {}
        Expr::Array(items) => {
            for e in *items {
                validate_check_refs(e, def, variables)?;
            }
        }
        Expr::Subscript { base, index } => {
            validate_check_refs(base, def, variables)?;
            validate_check_refs(index, def, variables)?;
        }
        Expr::Slice { base, lower, upper } => {
            validate_check_refs(base, def, variables)?;
            if let Some(e) = lower {
                validate_check_refs(e, def, variables)?;
            }
            if let Some(e) = upper {
                validate_check_refs(e, def, variables)?;
            }
        }
        // `(t).column` selects that column of the relation's row.
        Expr::Field { base, field } if *field != "*" && names_relation(base, def)? => {
            let Some(index) = def.column_index(field) else {
                return Err(sql_err!(
                    sqlstate::UNDEFINED_COLUMN,
                    "column {}.{} does not exist",
                    def.name.as_str(),
                    field
                ));
            };
            variables.add_column(index);
        }
        Expr::Field { base, .. } | Expr::RecordFieldIndex { base, .. } => {
            validate_check_refs(base, def, variables)?
        }
        Expr::AnyAll { operand, array, .. } => {
            validate_check_refs(operand, def, variables)?;
            validate_check_refs(array, def, variables)?;
        }
    }
    Ok(())
}

/// How an exclusion element was written: its key options as parsed, or, when
/// a stored constraint is rebuilt, the options it already holds.
#[derive(Clone, Copy)]
enum ExclusionKeySource<'k> {
    Written(&'k crate::sql::ast::IndexColumn<'k>),
    Stored(crate::storage::ExclusionKeyOptions),
}

/// The operator class `written` names for an exclusion element of type
/// `type_oid` under `method`, as PostgreSQL's `ResolveOpClass` finds it.
fn resolve_exclusion_class(
    storage: &Storage,
    written: crate::sql::ast::QualName<'_>,
    input: crate::storage::RoutineResult,
    type_oid: i32,
    method: crate::sql::ast::IndexAccessMethod,
    txid: u32,
) -> Result<crate::storage::IndexOperatorClass, SqlError> {
    use crate::storage::IndexOperatorClass;
    let missing = || {
        sql_err!(
            sqlstate::UNDEFINED_OBJECT,
            "operator class \"{}\" does not exist for access method \"{}\"",
            written.name,
            method.name()
        )
    };
    let mismatch = || {
        sql_err!(
            sqlstate::DATATYPE_MISMATCH,
            "operator class \"{}\" does not accept data type {}",
            written.name,
            crate::sql::catalog::format_type_oid(storage, txid, type_oid, -1).as_str()
        )
    };
    if method == crate::sql::ast::IndexAccessMethod::Btree
        && let Some(slot) = storage.operator_class_slot_on_path(written.schema, written.name, txid)
    {
        let definition = storage.operator_class_for(slot, txid);
        if storage.routine_type_oid(definition.input.ctype, definition.input.user_type, txid)
            != Some(type_oid)
        {
            return Err(mismatch());
        }
        let oid = crate::storage::OperatorClassOid::parse(storage.operator_class(slot).oid())
            .expect("user operator-class OID is typed");
        return Ok(IndexOperatorClass::Catalog(oid));
    }
    if written
        .schema
        .is_some_and(|schema| !schema.eq_ignore_ascii_case("pg_catalog"))
    {
        return Err(missing());
    }
    let ctype = input.ctype;
    let class = match method {
        crate::sql::ast::IndexAccessMethod::Btree => {
            crate::sql::types::BtreeOperatorClass::parse(written.name)
                .map(|class| (class.accepts(ctype), IndexOperatorClass::Btree(class)))
        }
        crate::sql::ast::IndexAccessMethod::Hash => {
            crate::sql::types::HashOperatorClass::parse(written.name)
                .map(|class| (class.accepts(ctype), IndexOperatorClass::Hash(class)))
        }
        crate::sql::ast::IndexAccessMethod::Gist => {
            crate::sql::types::GistOperatorClass::parse(written.name)
                .map(|class| (class.accepts(ctype), IndexOperatorClass::Gist(class)))
        }
        crate::sql::ast::IndexAccessMethod::SpGist => {
            crate::sql::types::SpGistOperatorClass::parse(written.name)
                .map(|class| (class.accepts(ctype), IndexOperatorClass::SpGist(class)))
        }
        crate::sql::ast::IndexAccessMethod::Brin | crate::sql::ast::IndexAccessMethod::Gin => None,
    };
    match class {
        Some((true, class)) => Ok(class),
        Some((false, _)) => Err(mismatch()),
        None => Err(missing()),
    }
}

/// The operator family of `class` and whether `operator` is one of its
/// members, with the family's name for PostgreSQL's error.
fn exclusion_family_membership(
    storage: &Storage,
    class: crate::storage::IndexOperatorClass,
    method: crate::sql::ast::IndexAccessMethod,
    operator: i32,
    txid: u32,
) -> Result<(bool, SqlName), SqlError> {
    if let crate::storage::IndexOperatorClass::Catalog(oid) = class {
        let slot = storage
            .operator_class_slot_by_oid(oid, txid)
            .ok_or_else(|| sql_err!(sqlstate::UNDEFINED_OBJECT, "operator class does not exist"))?;
        let definition = storage.operator_class_for(slot, txid);
        let family_slot = storage
            .operator_family_slot_by_oid(definition.family, txid)
            .ok_or_else(|| {
                sql_err!(sqlstate::UNDEFINED_OBJECT, "operator family does not exist")
            })?;
        let family = storage.operator_family_for(family_slot, txid);
        let member = definition
            .operators
            .iter()
            .chain(family.operators.iter())
            .any(|member| member.used && member.operator == operator);
        return Ok((member, family.name));
    }
    let Some((family, _)) = crate::sql::catalog::builtin_operator_class_family(class.oid()) else {
        return Err(sql_err!(
            sqlstate::UNDEFINED_OBJECT,
            "cache lookup failed for operator class {}",
            class.oid()
        ));
    };
    let name = crate::sql::catalog::builtin_operator_family(family).map_or("", |(_, name)| name);
    Ok((
        crate::sql::catalog::builtin_family_has_operator(family, method.oid(), operator),
        SqlName::parse(name)?,
    ))
}

/// The operator, operator class and collation of one exclusion element of
/// type `type_oid`, checked in the order PostgreSQL's `ComputeIndexAttrs`
/// checks them: the collation, the operator class, then a commutative
/// operator named `written` from the class's operator family.
#[allow(clippy::too_many_arguments)]
fn exclusion_operator(
    storage: &Storage,
    element: crate::storage::RoutineResult,
    type_oid: i32,
    natural_collation: crate::sql::ast::Collation,
    key: ExclusionKeySource<'_>,
    method: crate::sql::ast::IndexAccessMethod,
    (written_schema, written): (Option<&str>, &str),
    txid: u32,
) -> Result<
    (
        crate::storage::ExclusionOperator,
        crate::storage::ExclusionKeyOptions,
    ),
    SqlError,
> {
    let type_name = |oid: i32| crate::sql::catalog::format_type_oid(storage, txid, oid, -1);
    let mut options = crate::storage::ExclusionKeyOptions::DEFAULT;
    let collation = match key {
        ExclusionKeySource::Written(key) => key
            .collation
            .map(|collation| resolve_parsed_collation(storage, txid, collation))
            .transpose()?,
        ExclusionKeySource::Stored(stored) => stored.explicit_collation.then_some(stored.collation),
    };
    if collation.is_some() && !element.ctype.is_collatable() {
        return Err(sql_err!(
            sqlstate::DATATYPE_MISMATCH,
            "collations are not supported by type {}",
            type_name(type_oid).as_str()
        ));
    }
    // A collation equal to the key's own prints nothing, as PostgreSQL
    // compares `indcollation` with the key's collation.
    options.collation = collation.unwrap_or(natural_collation);
    options.explicit_collation = options.collation != natural_collation;
    let class = match key {
        ExclusionKeySource::Written(key) => match key.operator_class {
            Some(written_class) => {
                if !key.operator_class_options.is_empty() {
                    return Err(sql_err!(
                        sqlstate::FEATURE_NOT_SUPPORTED,
                        "operator class options in exclusion constraint elements are not supported"
                    ));
                }
                let class = resolve_exclusion_class(
                    storage,
                    written_class,
                    element,
                    type_oid,
                    method,
                    txid,
                )?;
                options.class = Some(class);
                class
            }
            None => storage.resolve_index_operator_class(None, element, method, txid)?,
        },
        ExclusionKeySource::Stored(stored) => {
            options.class = stored.class;
            storage.resolve_index_operator_class(stored.class, element, method, txid)?
        }
    };
    options.resolved_class = Some(class);
    if let crate::storage::IndexOperatorClass::Catalog(oid) = class {
        let slot = storage
            .operator_class_slot_by_oid(oid, txid)
            .ok_or_else(|| sql_err!(sqlstate::UNDEFINED_OBJECT, "operator class does not exist"))?;
        if storage
            .operator_class_for(slot, txid)
            .comparison_function()
            .is_none()
        {
            return Err(sql_err!(
                sqlstate::UNDEFINED_OBJECT,
                "missing support function 1 for index operator class"
            ));
        }
    }
    let input = match crate::sql::catalog::builtin_operator_class_family(class.oid()) {
        Some((_, input)) => input,
        None => type_oid,
    };
    let builtin = written_schema
        .is_none_or(|schema| schema.eq_ignore_ascii_case("pg_catalog"))
        .then(|| {
            crate::sql::catalog::builtin_binary_operator(written, type_oid, type_oid)
                .or_else(|| crate::sql::catalog::builtin_binary_operator(written, input, input))
        })
        .flatten();
    let operator = match builtin {
        Some(operator) => crate::storage::ExclusionOperator::from_oid(operator),
        None => storage
            .operator_slot_for_oids(
                written_schema,
                written,
                Some(type_oid),
                Some(type_oid),
                txid,
            )?
            .and_then(|slot| crate::storage::ExclusionOperator::user(storage.operator(slot).oid())),
    }
    .ok_or_else(|| {
        crate::sql::error_position::with_hint(
            sql_err!(
                sqlstate::UNDEFINED_FUNCTION,
                "operator does not exist: {} {} {}",
                type_name(type_oid).as_str(),
                written,
                type_name(type_oid).as_str()
            ),
            "No operator matches the given name and argument types. You might need to add explicit type casts.",
        )
    })?;
    // PostgreSQL formats the operator under a restricted search path, so a
    // user-defined operator is always schema-qualified.
    let (left, right, commutative, schema) =
        match crate::sql::catalog::builtin_binary_operator_row(operator.oid()) {
            Some((_, left, right, commutator)) => (left, right, commutator == operator.oid(), None),
            None => {
                let slot = storage
                    .operator_slot_by_oid(operator.oid(), txid)
                    .expect("a resolved user operator is visible");
                let definition = storage.operator_for(slot, txid);
                let argument = |argument: Option<crate::storage::RoutineResult>| {
                    argument
                        .and_then(|argument| {
                            storage.routine_type_oid(argument.ctype, argument.user_type, txid)
                        })
                        .unwrap_or(type_oid)
                };
                (
                    argument(definition.signature.left),
                    argument(definition.signature.right),
                    definition.commutator == Some(operator.oid()),
                    Some(definition.schema),
                )
            }
        };
    let signature = crate::stack_format!(
        600,
        "{}{}{}({},{})",
        schema.as_ref().map_or("", |schema| schema.as_str()),
        if schema.is_some() { "." } else { "" },
        written,
        type_name(left).as_str(),
        type_name(right).as_str()
    );
    if !commutative {
        crate::sql::eval::stash_diagnostic(
            Some(crate::util::StackStr::from_str(
                "Only commutative operators can be used in exclusion constraints.",
            )),
            None,
        );
        return Err(sql_err!(
            sqlstate::WRONG_OBJECT_TYPE,
            "operator {} is not commutative",
            signature.as_str()
        ));
    }
    let (member, family_name) =
        exclusion_family_membership(storage, class, method, operator.oid(), txid)?;
    if !member {
        crate::sql::eval::stash_diagnostic(
            Some(crate::util::StackStr::from_str(
                "The exclusion operator must be related to the index operator class for the constraint.",
            )),
            None,
        );
        return Err(sql_err!(
            sqlstate::WRONG_OBJECT_TYPE,
            "operator {} is not a member of operator family \"{}\"",
            signature.as_str(),
            family_name.as_str()
        ));
    }
    let (ordering_specified, nulls_order_specified) = match key {
        ExclusionKeySource::Written(key) => {
            options.descending = key.descending;
            options.nulls_first = key.nulls_first;
            (key.ordering_specified, key.nulls_order_specified)
        }
        ExclusionKeySource::Stored(stored) => {
            options.descending = stored.descending;
            options.nulls_first = stored.nulls_first;
            (false, false)
        }
    };
    // Only an ordered access method accepts ordering options.
    if method != crate::sql::ast::IndexAccessMethod::Btree
        && (ordering_specified || nulls_order_specified)
    {
        return Err(sql_err!(
            sqlstate::FEATURE_NOT_SUPPORTED,
            "access method \"{}\" does not support {} options",
            method.name(),
            if ordering_specified {
                "ASC/DESC"
            } else {
                "NULLS FIRST/LAST"
            }
        ));
    }
    Ok((operator, options))
}

/// Applies each parsed table constraint to `def`: single-column PK/UNIQUE set
/// column flags; multi-column PK/UNIQUE become entries in `def.uniques`; CHECK
/// predicates and FOREIGN KEYs are validated and recorded.
/// PostgreSQL's rejection of a key, foreign key or exclusion constraint on a
/// foreign table, positioned where the constraint was written.
pub(super) fn reject_foreign_table_constraint(
    constraint: &TableConstraint,
) -> Result<(), SqlError> {
    let kind = match constraint {
        TableConstraint::PrimaryKey { .. } => "primary key",
        TableConstraint::Unique { .. } => "unique",
        TableConstraint::ForeignKey { .. } => "foreign key",
        TableConstraint::Exclusion { .. } => "exclusion",
        TableConstraint::Check { .. } => return Ok(()),
    };
    Err(crate::sql::error_position::at_source_position(
        sql_err!(
            sqlstate::FEATURE_NOT_SUPPORTED,
            "{} constraints are not supported on foreign tables",
            kind
        ),
        constraint.source_position(),
    ))
}

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
        if def.kind == crate::storage::TableKind::Foreign {
            reject_foreign_table_constraint(con)?;
        }
        match con {
            TableConstraint::PrimaryKey {
                source_position: _,
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
                reject_virtual_columns(def, &indices[..n], "primary keys")?;
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
                source_position: _,
                name,
                columns,
                timing,
                nulls_not_distinct,
            } => {
                let (indices, n) = resolve_cols(def, columns)?;
                reject_virtual_columns(def, &indices[..n], "unique constraints")?;
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
                source_position: _,
                name,
                expression,
                text,
                validation,
                no_inherit,
            } => {
                if *no_inherit && def.partition.is_partitioned() {
                    return Err(sql_err!(
                        sqlstate::INVALID_TABLE_DEFINITION,
                        "cannot add NO INHERIT constraint to partitioned table \"{}\"",
                        def.name.as_str()
                    ));
                }
                // CREATE TABLE (the only caller merging equivalent
                // constraints) positions a CHECK's unknown column as
                // PostgreSQL does; ALTER TABLE reports it without one.
                let single_column = if merge_equivalent {
                    crate::sql::error_position::with_analysis(|| {
                        expression_variables(expression, def)
                    })?
                } else {
                    expression_variables(expression, def)?
                }
                .single_column();
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
                    None => auto_check_name(storage, def, single_column, txid, exclude)?,
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
                    no_inherit: *no_inherit,
                };
                let stored = stored_relation_expression(text, def, arena)?;
                let _ = core::fmt::Write::write_str(&mut c.expression, stored);
                if c.expression.is_truncated() {
                    return Err(sql_err!(
                        sqlstate::PROGRAM_LIMIT_EXCEEDED,
                        "CHECK predicate is too long"
                    ));
                }
                def.checks[def.n_checks] = c;
                def.n_checks += 1;
                super::expression_bindings::capture_expression_dependencies(
                    storage,
                    def,
                    crate::storage::ExpressionOwner::Check(c.name),
                    c.expression.as_str(),
                    txid,
                    arena,
                )?;
            }
            TableConstraint::ForeignKey {
                source_position: _,
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
                // The referenced key is checked first, as PostgreSQL does.
                let (indices, n) = resolve_cols(def, columns)?;
                reject_virtual_columns(def, &indices[..n], "foreign key constraints")?;
            }
            TableConstraint::Exclusion {
                source_position: _,
                name,
                method,
                elements,
                predicate,
                predicate_text,
                timing,
            } => {
                if elements.len() > MAX_INDEX_COLS {
                    return Err(sql_err!(
                        sqlstate::TOO_MANY_COLUMNS,
                        "cannot use more than {} columns in an index",
                        MAX_INDEX_COLS
                    ));
                }
                if matches!(
                    method,
                    crate::sql::ast::IndexAccessMethod::Brin
                        | crate::sql::ast::IndexAccessMethod::Gin
                ) {
                    return Err(sql_err!(
                        sqlstate::FEATURE_NOT_SUPPORTED,
                        "access method \"{}\" does not support exclusion constraints",
                        method.name()
                    ));
                }
                let mut exclusion = crate::storage::ExclusionConstraint::EMPTY;
                for element in *elements {
                    let (key, operator, options) =
                        resolve_exclusion_element(storage, def, *method, element, txid, arena)?;
                    exclusion.push(key, operator, options)?;
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
                exclusion.method = *method;
                exclusion.timing = storage_timing(*timing);
                exclusion.name = match name {
                    Some(name) => SqlName::parse(name)?,
                    None => auto_exclusion_name(storage, def, &exclusion, txid, exclude, arena)?,
                };
                if !merge_equivalent && constraint_name_taken(def, exclusion.name.as_str()) {
                    return Err(sql_err!(
                        sqlstate::DUPLICATE_OBJECT,
                        "constraint \"{}\" for relation \"{}\" already exists",
                        exclusion.name.as_str(),
                        def.name.as_str()
                    ));
                }
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
                super::expression_bindings::capture_exclusion_expression_dependencies(
                    storage, def, &exclusion, txid, arena,
                )?;
            }
        }
    }
    def.attributes.initialize(def.n_columns)?;
    crate::storage::constraint_names::table(def)
}

/// One `EXCLUDE` element resolved as PostgreSQL's `ComputeIndexAttrs` does:
/// a plain column reference is the column, any other expression is kept as
/// its canonical text after its names, type and immutability are checked.
fn resolve_exclusion_element<'a>(
    storage: &Storage,
    def: &TableDefBuilder,
    method: crate::sql::ast::IndexAccessMethod,
    element: &crate::sql::ast::ExclusionElementSpec<'a>,
    txid: u32,
    arena: &'a Arena,
) -> Result<
    (
        crate::storage::ExclusionElement<'a>,
        crate::storage::ExclusionOperator,
        crate::storage::ExclusionKeyOptions,
    ),
    SqlError,
> {
    let key = &element.key;
    let column = match key.column {
        Some(name) => Some(def.column_index(name).ok_or_else(|| {
            if key.expression_text.starts_with('(') || key.expression_text != name {
                crate::sql::error_position::at_expression(
                    sql_err!(
                        sqlstate::UNDEFINED_COLUMN,
                        "column \"{}\" does not exist",
                        name
                    ),
                    key.expression,
                )
            } else {
                sql_err!(
                    sqlstate::UNDEFINED_COLUMN,
                    "column \"{}\" named in key does not exist",
                    name
                )
            }
        })?),
        None => plain_column_reference(key.expression, def)?,
    };
    let (stored, input, type_oid, natural) = if let Some(column) = column {
        let meta = def.columns()[column];
        let type_oid = crate::sql::catalog::catalog_column_type_oid(storage, &meta, txid)?;
        (
            crate::storage::ExclusionElement::Column(column as u16),
            crate::storage::RoutineResult {
                ctype: meta.ctype,
                user_type: meta.user_type,
            },
            type_oid,
            meta.collation,
        )
    } else {
        check_referenced_columns(key.expression, def)?;
        let (input, type_oid) =
            exclusion_expression_type(storage, def, key.expression, method, txid, arena)?;
        let natural = super::constraints::index_key_collations(
            storage,
            txid,
            def,
            &[0],
            &[Some(key.expression)],
            arena,
        )?[0];
        let text = stored_relation_expression(key.expression_text, def, arena)?;
        (
            crate::storage::ExclusionElement::Expression(text),
            input,
            type_oid,
            natural,
        )
    };
    let (operator, options) = exclusion_operator(
        storage,
        input,
        type_oid,
        natural,
        ExclusionKeySource::Written(key),
        method,
        (None, element.operator),
        txid,
    )?;
    Ok((stored, operator, options))
}

/// The type of an exclusion element expression over `def`, which must be
/// immutable, as `ComputeIndexAttrs` checks it.
fn exclusion_expression_type(
    storage: &Storage,
    def: &TableDefBuilder,
    expression: &Expr<'_>,
    method: crate::sql::ast::IndexAccessMethod,
    txid: u32,
    arena: &Arena,
) -> Result<(crate::storage::RoutineResult, i32), SqlError> {
    let (type_oid, _) = super::infer_type_res(
        expression,
        &super::CatalogCols {
            definition: Some(crate::storage::RelationDefinition::from_definition(def)),
            alias: None,
            output_aliases: &[],
            storage,
            txid,
            arena,
        },
    )?;
    require_immutable_index_expression(expression, def, storage, txid, "expression", arena)?;
    let (ctype, user_type) =
        super::catalog_column_type(storage, txid, type_oid).ok_or_else(|| {
            crate::sql::error_position::with_hint(
                sql_err!(
                    sqlstate::UNDEFINED_OBJECT,
                    "data type unknown has no default operator class for access method \"{}\"",
                    method.name()
                ),
                crate::sql::error_position::DEFAULT_OPERATOR_CLASS_HINT,
            )
        })?;
    Ok((crate::storage::RoutineResult { ctype, user_type }, type_oid))
}

/// Re-analyzes `exclusion` over `def` after its columns changed type, as
/// PostgreSQL rebuilds the constraint from its definition: each expression
/// is resolved again and each operator looked up for the new element type.
pub(crate) fn revalidate_exclusion(
    storage: &Storage,
    def: &TableDefBuilder,
    exclusion: &crate::storage::ExclusionConstraint,
    txid: u32,
    arena: &Arena,
) -> Result<crate::storage::ExclusionConstraint, SqlError> {
    let mut rebuilt = exclusion.without_elements();
    for (position, ((element, operator), stored)) in exclusion
        .elements()
        .zip(exclusion.operators())
        .zip(exclusion.options())
        .enumerate()
    {
        let (input, type_oid, natural) = match element {
            crate::storage::ExclusionElement::Column(column) => {
                let meta = def.columns()[usize::from(column)];
                (
                    crate::storage::RoutineResult {
                        ctype: meta.ctype,
                        user_type: meta.user_type,
                    },
                    crate::sql::catalog::catalog_column_type_oid(storage, &meta, txid)?,
                    meta.collation,
                )
            }
            crate::storage::ExclusionElement::Expression(text) => {
                let source = arena
                    .alloc_str(text)
                    .map_err(|_| crate::sql::eval::arena_full())?;
                let expression = super::expression_bindings::bind_table_expression(
                    crate::sql::parser::parse_expr(source, arena)?,
                    storage,
                    def,
                    crate::storage::ExpressionOwner::ExclusionKey(exclusion.name, position as u8),
                    txid,
                    arena,
                )?;
                // The stored definition, not the statement, is analyzed.
                let (input, type_oid) = exclusion_expression_type(
                    storage,
                    def,
                    expression,
                    exclusion.method,
                    txid,
                    arena,
                )
                .inspect_err(|error| {
                    let _ = crate::sql::error_position::take_position(error);
                })?;
                let natural = super::constraints::index_key_collations(
                    storage,
                    txid,
                    def,
                    &[0],
                    &[Some(expression)],
                    arena,
                )?[0];
                (input, type_oid, natural)
            }
        };
        let user;
        let written = match operator.builtin_sql() {
            Some(name) => (None, name),
            None => {
                let slot = storage
                    .operator_slot_by_oid(operator.oid(), txid)
                    .ok_or_else(|| {
                        sql_err!(
                            sqlstate::UNDEFINED_FUNCTION,
                            "cache lookup failed for operator {}",
                            operator.oid()
                        )
                    })?;
                user = storage.operator_for(slot, txid);
                (Some(user.schema.as_str()), user.name.as_str())
            }
        };
        let (operator, options) = exclusion_operator(
            storage,
            input,
            type_oid,
            natural,
            ExclusionKeySource::Stored(*stored),
            exclusion.method,
            written,
            txid,
        )?;
        rebuilt.push(element, operator, options)?;
    }
    Ok(rebuilt)
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
    generated_key_name(
        storage,
        def,
        addition.as_str(),
        suffix,
        transaction,
        exclude,
        class,
    )
}

/// The name PostgreSQL's `ChooseIndexName` gives an exclusion constraint:
/// its elements' index column names, an expression named as
/// `FigureIndexColname` names it.
pub(super) fn auto_exclusion_name<Definition0: crate::storage::DefinitionBacking>(
    storage: &Storage,
    def: &crate::storage::TableDefinition<Definition0>,
    exclusion: &crate::storage::ExclusionConstraint,
    transaction: u32,
    exclude: Option<usize>,
    arena: &Arena,
) -> Result<SqlName, SqlError> {
    let mut names = super::IndexColumnNames::new();
    for element in exclusion.elements() {
        match element {
            crate::storage::ExclusionElement::Column(column) => {
                names.push(def.columns()[usize::from(column)].name.as_str())?;
            }
            crate::storage::ExclusionElement::Expression(text) => {
                let source = arena
                    .alloc_str(text)
                    .map_err(|_| crate::sql::eval::arena_full())?;
                let expression = crate::sql::parser::parse_expr(source, arena)?;
                names.push(super::index_expression_column_name(expression))?;
            }
        }
    }
    generated_key_name(
        storage,
        def,
        names.addition().as_str(),
        "excl",
        transaction,
        exclude,
        crate::storage::GeneratedNameClass::IndexConstraint,
    )
}

fn generated_key_name<Definition0: crate::storage::DefinitionBacking>(
    storage: &Storage,
    def: &crate::storage::TableDefinition<Definition0>,
    addition: &str,
    suffix: &str,
    transaction: u32,
    exclude: Option<usize>,
    class: crate::storage::GeneratedNameClass,
) -> Result<SqlName, SqlError> {
    use core::fmt::Write as _;
    let mut number = 0u32;
    loop {
        let mut label = StackStr::<32>::from_str(suffix);
        if number != 0 {
            write!(label, "{number}").expect("u32 generated-name suffix fits");
        }
        let name = crate::storage::constraint_names::generated(
            def.name.as_str(),
            (!addition.is_empty()).then_some(addition),
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
    single_column: Option<usize>,
    transaction: u32,
    exclude: Option<usize>,
) -> Result<SqlName, SqlError> {
    let column = single_column.map(|column| [column as u16]);
    auto_key_name(
        storage,
        def,
        column.as_ref().map_or(&[][..], |column| &column[..]),
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
    // Referenced columns default to the parent's primary key; otherwise a
    // unique index over exactly the named columns must back them. Both are
    // settled before privileges and the column counts are checked, as in
    // PostgreSQL's `ATAddForeignKeyConstraint`.
    let mut pcol_names: [&str; MAX_INDEX_COLS] = [""; MAX_INDEX_COLS];
    let n_parent;
    let mut primary_index = crate::storage::ReferencedIndex::PendingPrimaryKey;
    if parent_cols.is_empty() {
        let (pk, pk_n) = primary_key_cols(&parent_def);
        if pk_n == 0 {
            return Err(sql_err!(
                crate::sql::eval::sqlstate::INVALID_FOREIGN_KEY,
                "there is no primary key for referenced table \"{}\"",
                parent.name
            ));
        }
        if primary_key_is_deferrable(&parent_def) {
            return Err(sql_err!(
                crate::sql::eval::sqlstate::OBJECT_NOT_IN_PREREQUISITE_STATE,
                "cannot use a deferrable primary key for referenced table \"{}\"",
                parent.name
            ));
        }
        n_parent = pk_n;
        for (i, &column_index) in pk[..pk_n].iter().enumerate() {
            pcol_names[i] = parent_def.columns()[column_index as usize].name.as_str();
        }
        let mut primary = None;
        parent_def.for_each_key_index(|identity, _, _, is_primary| {
            if is_primary {
                primary = Some(identity);
            }
        });
        if let Some(identity) = primary.flatten() {
            primary_index = crate::storage::ReferencedIndex::Index(identity);
        }
    } else {
        n_parent = parent_cols.len();
        pcol_names[..n_parent].copy_from_slice(parent_cols);
    }
    let (parent_idxs, _) = resolve_foreign_key_cols(&parent_def, &pcol_names[..n_parent])?;
    let referenced_index = if parent_cols.is_empty() {
        primary_index
    } else {
        let referenced = &parent_idxs[..n_parent];
        if (1..n_parent).any(|i| referenced[..i].contains(&referenced[i])) {
            return Err(sql_err!(
                crate::sql::eval::sqlstate::INVALID_FOREIGN_KEY,
                "foreign key referenced-columns list must not contain duplicates"
            ));
        }
        match referenced_key(storage, txid, &parent_def, referenced) {
            ReferencedKey::Usable(index) => index,
            ReferencedKey::Deferrable => {
                return Err(sql_err!(
                    crate::sql::eval::sqlstate::OBJECT_NOT_IN_PREREQUISITE_STATE,
                    "cannot use a deferrable unique constraint for referenced table \"{}\"",
                    parent.name
                ));
            }
            ReferencedKey::Missing => {
                return Err(sql_err!(
                    crate::sql::eval::sqlstate::INVALID_FOREIGN_KEY,
                    "there is no unique constraint matching given keys for referenced table \"{}\"",
                    parent.name
                ));
            }
        }
    };
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
    if n_parent != n_child {
        return Err(sql_err!(
            crate::sql::eval::sqlstate::INVALID_FOREIGN_KEY,
            "number of referencing and referenced columns for foreign key disagree"
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
    fk.referenced_index = referenced_index;
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

/// Whether a set of columns is a unique key a foreign key can reference.
enum ReferencedKey {
    Usable(crate::storage::ReferencedIndex),
    /// Only a deferrable unique key matches; PostgreSQL rejects it.
    Deferrable,
    Missing,
}

/// PostgreSQL's `transformFkeyCheckAttrs`: the oldest valid, immediate,
/// unique, non-partial index without expressions whose key columns are
/// exactly `columns` in any order, whether it backs a constraint or not. A
/// key the statement itself adds (to the table it creates or alters) is
/// newer than every existing index and is chosen once it has an identity.
fn referenced_key<Definition0: crate::storage::DefinitionBacking>(
    storage: &Storage,
    txid: u32,
    def: &crate::storage::TableDefinition<Definition0>,
    columns: &[u16],
) -> ReferencedKey {
    let same = |key: &[u16]| key.len() == columns.len() && columns.iter().all(|c| key.contains(c));
    let mut oldest: Option<crate::storage::RelationIdentity> = None;
    let mut pending = false;
    let mut deferrable = false;
    let mut consider = |identity: Option<crate::storage::RelationIdentity>, immediate: bool| {
        if !immediate {
            deferrable = true;
        } else if let Some(identity) = identity {
            if oldest.is_none_or(|best| identity.created_at() < best.created_at()) {
                oldest = Some(identity);
            }
        } else {
            pending = true;
        }
    };
    def.for_each_key_index(|identity, key, immediate, _| {
        if same(key) {
            consider(identity, immediate);
        }
    });
    for index in storage.indexes_for(def.schema.as_str(), def.name.as_str(), txid) {
        let index = index.definition_for(txid);
        let mutable = index.mutable_for(txid);
        if index.unique
            && mutable.kind.valid()
            && index.predicate.is_none()
            && index.expressions[..index.n_cols]
                .iter()
                .all(Option::is_none)
            && same(&index.columns[..index.n_cols])
        {
            consider(
                crate::storage::RelationIdentity::parse(index.created_at),
                mutable.immediate,
            );
        }
    }
    match oldest {
        Some(identity) => ReferencedKey::Usable(crate::storage::ReferencedIndex::Index(identity)),
        None if pending => ReferencedKey::Usable(crate::storage::ReferencedIndex::PendingKey),
        None if deferrable => ReferencedKey::Deferrable,
        None => ReferencedKey::Missing,
    }
}

fn primary_key_is_deferrable<Definition0: crate::storage::DefinitionBacking>(
    def: &crate::storage::TableDefinition<Definition0>,
) -> bool {
    def.uniques()
        .iter()
        .any(|key| key.kind.is_primary() && key.timing.is_deferrable())
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

/// PostgreSQL 18's refusal of a key, index or statistics over a virtual
/// generated column, whose values the row does not hold.
pub(crate) fn reject_virtual_columns<Definition0: crate::storage::DefinitionBacking>(
    def: &crate::storage::TableDefinition<Definition0>,
    columns: &[u16],
    what: &str,
) -> Result<(), SqlError> {
    if columns
        .iter()
        .any(|&column| def.columns()[usize::from(column)].default.is_virtual())
    {
        return Err(sql_err!(
            sqlstate::FEATURE_NOT_SUPPORTED,
            "{} on virtual generated columns are not supported",
            what
        ));
    }
    Ok(())
}
