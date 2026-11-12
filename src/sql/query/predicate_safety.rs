//! Resolved expression effects govern projection reuse and security barriers.

use crate::mem::arena::Arena;
use crate::sql::ast::{BinaryOp, CastBinding, Expr, UnaryOp};
use crate::sql::eval::{SqlError, sqlstate};
use crate::sql::exec::{ColTypeResolver, infer_routine_argument_oid};
use crate::sql::types::ColType;
use crate::sql_err;
use crate::storage::{RoutineVolatility, Storage};

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum AnalysisStage {
    ViewPushdown,
    RuntimeProjection,
}

#[derive(Clone, Copy)]
pub(super) struct ExpressionProperties {
    pub(super) volatile: bool,
    pub(super) leaks_columns: bool,
    pub(super) columns: bool,
}

#[derive(Clone, Copy)]
struct ProcedureProperties {
    volatile: bool,
    leakproof: bool,
}

impl ProcedureProperties {
    const UNKNOWN: Self = Self {
        volatile: true,
        leakproof: false,
    };
    const BINARY: Self = Self {
        volatile: false,
        leakproof: true,
    };

    fn from_builtin(properties: Option<(bool, bool)>) -> Self {
        properties.map_or(Self::UNKNOWN, |(volatile, leakproof)| Self {
            volatile,
            leakproof,
        })
    }
}

fn routine_properties(storage: &Storage, txid: u32, function: i32) -> ProcedureProperties {
    if let Some(slot) = storage.routine_slot_by_oid(function, txid) {
        let attributes = storage.routine_for(slot, txid).attributes;
        ProcedureProperties {
            volatile: attributes.volatility == RoutineVolatility::Volatile,
            leakproof: attributes.leakproof,
        }
    } else {
        ProcedureProperties::from_builtin(crate::sql::catalog::builtin_routine_properties(
            "",
            Some(function),
            &[],
        ))
    }
}

fn cast_properties(
    storage: &Storage,
    txid: u32,
    source: i32,
    target: i32,
    binding: CastBinding,
    type_mod: i32,
    context: crate::sql::eval::DefinedCastContext,
) -> Result<ProcedureProperties, SqlError> {
    if source == target && type_mod < 0 && ColType::from_oid(target).is_some() {
        return Ok(ProcedureProperties::BINARY);
    }
    let mut properties = if binding == CastBinding::Resolve
        && let Some(selected) = super::selected_cast(
            storage,
            txid,
            source,
            target,
            context,
            super::CastSelectionPhase::Analysis,
        )? {
        ProcedureProperties {
            volatile: selected.volatility == RoutineVolatility::Volatile,
            leakproof: selected.leakproof,
        }
    } else {
        ProcedureProperties::from_builtin(crate::sql::catalog::builtin_cast_properties(
            source, target,
        ))
    };
    if type_mod >= 0 {
        let modifier = ProcedureProperties::from_builtin(
            crate::sql::catalog::builtin_cast_properties(target, target),
        );
        properties.volatile |= modifier.volatile;
        properties.leakproof &= modifier.leakproof;
    }
    Ok(properties)
}

fn operator_properties(
    storage: &Storage,
    txid: u32,
    schema: Option<&str>,
    name: &str,
    left: Option<i32>,
    right: Option<i32>,
) -> Result<ProcedureProperties, SqlError> {
    if schema != Some("pg_catalog")
        && let Some(slot) = storage.operator_slot_for_oids(schema, name, left, right, txid)?
    {
        let definition = storage.operator_for(slot, txid);
        let Some(function) = definition.implementation.routine() else {
            return Ok(ProcedureProperties::UNKNOWN);
        };
        let mut result = routine_properties(storage, txid, function);
        for (actual, expected) in [
            (left, definition.signature.left),
            (right, definition.signature.right),
        ] {
            if let (Some(actual), Some(expected)) = (actual, expected)
                && let Some(expected) =
                    storage.routine_type_oid(expected.ctype, expected.user_type, txid)
            {
                let conversion = cast_properties(
                    storage,
                    txid,
                    actual,
                    expected,
                    CastBinding::Resolve,
                    -1,
                    crate::sql::eval::DefinedCastContext::Implicit,
                )?;
                result.volatile |= conversion.volatile;
                result.leakproof &= conversion.leakproof;
            }
        }
        return Ok(result);
    }
    if schema.is_some_and(|schema| schema != "pg_catalog") {
        return Ok(ProcedureProperties::UNKNOWN);
    }
    Ok(ProcedureProperties::from_builtin(
        crate::sql::catalog::builtin_operator_properties(name, left, right),
    ))
}

#[derive(Clone, Copy)]
pub(super) enum SqlConstructor {
    JsonArray,
    JsonObject,
    Other,
}

pub(super) fn sql_constructor(name: &str) -> Option<SqlConstructor> {
    use SqlConstructor::*;
    Some(match name {
        "__xmlparse_document"
        | "__xmlparse_content"
        | "__xmlserialize"
        | "__xmlexists"
        | "__xmlelement"
        | "__xmlforest"
        | "__xmlpi"
        | "__xmlroot"
        | "__is_json_scalar"
        | "__is_json_array"
        | "__is_json_object"
        | "__is_json_value"
        | "__is_json_scalar_unique"
        | "__is_json_array_unique"
        | "__is_json_object_unique"
        | "__is_json_value_unique"
        | "__json"
        | "__json_unique"
        | "__json_format"
        | "__json_serialize_text"
        | "__json_serialize_bytea"
        | "__jsonb_subscript_set"
        | "__array_subscript_set"
        | "__json_exists_true"
        | "__json_exists_unknown"
        | "__json_exists_error"
        | "__json_exists_false"
        | "__json_query_json"
        | "__json_query_jsonb"
        | "__json_query_text"
        | "__json_query_bytea"
        | "__json_value" => Other,
        "__json_array_absent_json"
        | "__json_array_absent_jsonb"
        | "__json_array_absent_text"
        | "__json_array_absent_bytea"
        | "__json_array_null_json"
        | "__json_array_null_jsonb"
        | "__json_array_null_text"
        | "__json_array_null_bytea" => JsonArray,
        "__json_object_json"
        | "__json_object_jsonb"
        | "__json_object_text"
        | "__json_object_bytea"
        | "__json_object_unique_json"
        | "__json_object_unique_jsonb"
        | "__json_object_unique_text"
        | "__json_object_unique_bytea" => JsonObject,
        _ => return None,
    })
}

fn sql_value_expression(name: &str) -> bool {
    matches!(
        name,
        "current_date"
            | "current_timestamp"
            | "current_time"
            | "localtimestamp"
            | "localtime"
            | "current_user"
            | "current_role"
            | "session_user"
            | "user"
            | "current_catalog"
            | "current_schema"
    )
}

/// A non-leakproof function matters at a barrier only when its arguments can
/// expose a row column. A volatile constant call can still move independently.
/// `arena` is scratch for the correlation check of each subquery: nothing
/// allocated there outlives the call.
pub(super) fn expression_properties(
    expression: &Expr<'_>,
    columns: &dyn ColTypeResolver,
    storage: &Storage,
    txid: u32,
    stage: AnalysisStage,
    arena: &Arena,
) -> Result<ExpressionProperties, SqlError> {
    let mut result = ExpressionProperties {
        volatile: false,
        leaks_columns: false,
        columns: false,
    };
    super::walk_children(expression, &mut |child| {
        let child = expression_properties(child, columns, storage, txid, stage, arena)?;
        result.volatile |= child.volatile;
        result.leaks_columns |= child.leaks_columns;
        result.columns |= child.columns;
        Ok(())
    })?;
    let type_of = |expression: &Expr<'_>| infer_routine_argument_oid(expression, columns);
    let procedure = match expression {
        Expr::Column { .. }
        | Expr::SchemaColumn { .. }
        | Expr::WholeRow(_)
        | Expr::RecursiveState { .. } => {
            result.columns = true;
            None
        }
        Expr::Binary {
            operator: BinaryOp::And | BinaryOp::Or,
            ..
        }
        | Expr::Unary {
            operator: UnaryOp::Not,
            ..
        } => None,
        Expr::Binary {
            operator,
            left,
            right,
            ..
        } => Some(match operator.operator_name() {
            Some(name) => operator_properties(
                storage,
                txid,
                None,
                name,
                Some(type_of(left)?),
                Some(type_of(right)?),
            )?,
            None => ProcedureProperties::UNKNOWN,
        }),
        Expr::Unary {
            operator, operand, ..
        } => {
            let name = match operator {
                UnaryOp::Neg => "-",
                UnaryOp::BitNot => "~",
                UnaryOp::SquareRoot => "|/",
                UnaryOp::CubeRoot => "||/",
                UnaryOp::AbsoluteValue => "@",
                UnaryOp::TextSearchNot => "!!",
                UnaryOp::Not => unreachable!(),
            };
            Some(operator_properties(
                storage,
                txid,
                None,
                name,
                None,
                Some(type_of(operand)?),
            )?)
        }
        // ROW is a structural constructor, not a procedure lookup. Its
        // effects are exactly those of its already-classified child nodes.
        Expr::Call {
            kind: crate::sql::ast::CallKind::SqlSyntax,
            name,
            bound_oid: None,
            ..
        } if *name == "row" => None,
        Expr::Call {
            kind: crate::sql::ast::CallKind::SqlSyntax,
            name,
            bound_oid: None,
            args,
            argument_names,
            star: false,
            variadic: false,
            ..
        } if argument_names.is_empty() && sql_value_expression(name) => {
            // SQL value expressions read transaction or session state; their
            // explicit precision expression was classified with the children.
            Some(ProcedureProperties::BINARY)
        }
        Expr::Call {
            kind: crate::sql::ast::CallKind::SqlSyntax,
            name,
            bound_oid: None,
            args,
            argument_names,
            star: false,
            variadic: false,
            ..
        } if argument_names.is_empty()
            && matches!(*name, "coalesce" | "greatest" | "least" | "nullif") =>
        {
            let name = *name;
            if args.is_empty() || name == "nullif" && args.len() != 2 {
                return Err(crate::sql::eval::arity_err(name, args.len()));
            }
            if name == "nullif" {
                // NULLIF is a selected equality operation; its implementation
                // and argument coercions can execute catalog-defined code.
                Some(operator_properties(
                    storage,
                    txid,
                    None,
                    "=",
                    Some(type_of(args[0])?),
                    Some(type_of(args[1])?),
                )?)
            } else {
                let target = type_of(expression)?;
                for argument in *args {
                    let source = type_of(argument)?;
                    // Identical declarations need no conversion, including
                    // domains retained by all arms of a common-type node.
                    if source != target {
                        let conversion = cast_properties(
                            storage,
                            txid,
                            source,
                            target,
                            CastBinding::Resolve,
                            -1,
                            crate::sql::eval::DefinedCastContext::Implicit,
                        )?;
                        result.volatile |= conversion.volatile;
                        result.leaks_columns |=
                            !conversion.leakproof && contains_columns(argument)?;
                    }
                }
                if name == "coalesce" {
                    None
                } else {
                    // MinMax uses the common type's comparison, independently
                    // of a search-path overload of the SQL function spelling.
                    Some(operator_properties(
                        storage,
                        txid,
                        Some("pg_catalog"),
                        if name == "greatest" { ">" } else { "<" },
                        Some(target),
                        Some(target),
                    )?)
                }
            }
        }
        Expr::Call {
            kind: crate::sql::ast::CallKind::SqlSyntax,
            name,
            bound_oid: None,
            args,
            argument_names,
            star: false,
            variadic: false,
            ..
        } if argument_names.is_empty() && sql_constructor(name).is_some() => {
            // PostgreSQL's volatile-expression walk sees explicit children,
            // but not constructor-internal casts or output functions. Those
            // still execute at the original projection phase (PG 18.6).
            Some(ProcedureProperties {
                volatile: false,
                leakproof: false,
            })
        }
        Expr::Call {
            kind,
            name,
            bound_oid,
            args,
            argument_names,
            variadic,
            ..
        } => {
            let mut types = [0; crate::storage::MAX_ROUTINE_ARGUMENTS];
            if args.len() > types.len() {
                return Err(sql_err!(
                    sqlstate::PROGRAM_LIMIT_EXCEEDED,
                    "function call exceeds {} arguments",
                    types.len()
                ));
            }
            for (slot, argument) in types.iter_mut().zip(*args) {
                *slot = infer_routine_argument_oid(argument, columns)?;
            }
            let types = &types[..args.len()];
            if let Some((schema, operator)) = (*kind == crate::sql::ast::CallKind::SqlSyntax)
                .then(|| crate::sql::ast::catalog_operator_call(name))
                .flatten()
            {
                let (left, right) = match types {
                    [right] => (None, Some(*right)),
                    [left, right] => (Some(*left), Some(*right)),
                    _ => {
                        return Ok(ExpressionProperties {
                            volatile: true,
                            leaks_columns: result.leaks_columns || result.columns,
                            ..result
                        });
                    }
                };
                Some(operator_properties(
                    storage, txid, schema, operator, left, right,
                )?)
            } else {
                let (routine, builtin_oid) = if let Some(oid) = bound_oid {
                    (
                        storage.function_for_captured_call(
                            *oid,
                            argument_names,
                            types,
                            *variadic,
                            txid,
                        ),
                        None,
                    )
                } else {
                    match storage.select_routine_call(
                        name,
                        argument_names,
                        types,
                        *variadic,
                        txid,
                        crate::storage::RoutineCallKind::Function,
                    ) {
                        crate::storage::RoutineCallSelection::Stored(slot) => {
                            let oid = crate::storage::routine_oid(&storage.routine_for(slot, txid));
                            (
                                storage.function_for_captured_call(
                                    oid,
                                    argument_names,
                                    types,
                                    *variadic,
                                    txid,
                                ),
                                None,
                            )
                        }
                        crate::storage::RoutineCallSelection::Builtin { oid, .. } => {
                            (None, Some(oid))
                        }
                        crate::storage::RoutineCallSelection::Ambiguous => {
                            return Err(sql_err!(
                                sqlstate::AMBIGUOUS_FUNCTION,
                                "function {} is not unique",
                                name
                            ));
                        }
                        crate::storage::RoutineCallSelection::WrongKind { .. } => {
                            return Err(sql_err!(
                                sqlstate::WRONG_OBJECT_TYPE,
                                "{} is not a function",
                                name
                            ));
                        }
                        crate::storage::RoutineCallSelection::Absent => (None, None),
                    }
                };
                Some(match routine {
                    Some(routine) => {
                        let mapping = routine
                            .call_input_mapping(argument_names, args.len(), *variadic)
                            .ok_or_else(|| {
                                sql_err!(
                                    sqlstate::INTERNAL_ERROR,
                                    "selected routine input mapping is unavailable"
                                )
                            })?;
                        if stage == AnalysisStage::RuntimeProjection
                            && (0..routine.argument_count).any(|input| {
                                !mapping[..args.len()].contains(&(input as u8))
                                    && routine
                                        .parameter_for_input(input)
                                        .is_some_and(|parameter| parameter.mode.default().is_some())
                            })
                        {
                            // Defaults can execute after planning; do not run
                            // an omitted input before sorting and limiting.
                            result.volatile = true;
                        }
                        for (position, argument) in args.iter().enumerate() {
                            let input = usize::from(mapping[position]);
                            let parameter = routine
                                .parameter_for_input(input)
                                .expect("selected routine input parameter");
                            let mut expected = routine.arguments()[input];
                            if !*variadic
                                && matches!(
                                    parameter.mode,
                                    crate::storage::RoutineParameterMode::Variadic { .. }
                                )
                            {
                                let ColType::Array(element) = expected.ctype else {
                                    return Err(sql_err!(
                                        sqlstate::INTERNAL_ERROR,
                                        "selected variadic parameter is not an array"
                                    ));
                                };
                                expected.ctype = element.to_coltype();
                            }
                            let expected_oid = storage
                                .routine_type_oid(expected.ctype, expected.user_type, txid)
                                .ok_or_else(|| {
                                    sql_err!(
                                        sqlstate::INTERNAL_ERROR,
                                        "selected routine parameter type is unavailable"
                                    )
                                })?;
                            let conversion = parameter_conversion_properties(
                                storage,
                                txid,
                                types[position],
                                expected_oid,
                                expected.ctype,
                            )?;
                            result.volatile |= conversion.volatile;
                            result.leaks_columns |=
                                !conversion.leakproof && contains_columns(argument)?;
                        }
                        // View pushdown precedes omitted-default insertion.
                        // Explicit argument effects are always child nodes.
                        ProcedureProperties {
                            volatile: routine.attributes.volatility == RoutineVolatility::Volatile,
                            leakproof: routine.attributes.leakproof,
                        }
                    }
                    None => ProcedureProperties::from_builtin(
                        crate::sql::catalog::builtin_routine_properties(
                            name,
                            builtin_oid.or(*bound_oid),
                            types,
                        ),
                    ),
                })
            }
        }
        Expr::Cast {
            operand,
            type_name,
            binding,
            type_mod,
            ..
        } => {
            let target = columns.named_type_oid(type_name);
            // ARRAY constructors receive their element type from an explicit
            // array cast, including nested empty constructors and domains.
            let source = if let Expr::Array(items) = operand
                && let Some(ColType::Array(element)) = target.and_then(|target| {
                    crate::sql::eval::constructor_type_from_oid(target, &|oid| {
                        columns.domain_base_type(oid)
                    })
                }) {
                crate::sql::eval::array_constructor_type(items, Some(element), &type_of, &|oid| {
                    columns.domain_base_type(oid)
                })?
                .element
                .array_oid()
            } else {
                type_of(operand)?
            };
            let mut properties = match target {
                Some(target) => cast_properties(
                    storage,
                    txid,
                    source,
                    target,
                    *binding,
                    *type_mod,
                    crate::sql::eval::DefinedCastContext::Explicit,
                )?,
                None => ProcedureProperties::UNKNOWN,
            };
            if *binding == CastBinding::Resolve
                && let Some(ColType::Array(element)) = target.and_then(|target| {
                    crate::sql::eval::constructor_type_from_oid(target, &|oid| {
                        columns.domain_base_type(oid)
                    })
                })
            {
                crate::sql::exec::visit_array_constructor_conversions(
                    operand,
                    element,
                    columns,
                    &mut |source, target| {
                        let conversion = cast_properties(
                            storage,
                            txid,
                            source,
                            target,
                            *binding,
                            -1,
                            crate::sql::eval::DefinedCastContext::Explicit,
                        )?;
                        properties.volatile |= conversion.volatile;
                        properties.leakproof &= conversion.leakproof;
                        Ok(())
                    },
                )?;
            }
            Some(properties)
        }
        Expr::Like {
            operand,
            pattern,
            case_insensitive,
            ..
        } => Some(operator_properties(
            storage,
            txid,
            Some("pg_catalog"),
            if *case_insensitive { "~~*" } else { "~~" },
            Some(type_of(operand)?),
            Some(type_of(pattern)?),
        )?),
        Expr::Match {
            operand,
            pattern,
            case_insensitive,
            ..
        } => Some(operator_properties(
            storage,
            txid,
            Some("pg_catalog"),
            if *case_insensitive { "~*" } else { "~" },
            Some(type_of(operand)?),
            Some(type_of(pattern)?),
        )?),
        Expr::InList { operand, list, .. } => {
            let mut comparison = ProcedureProperties::BINARY;
            for item in *list {
                let properties = operator_properties(
                    storage,
                    txid,
                    None,
                    "=",
                    Some(type_of(operand)?),
                    Some(type_of(item)?),
                )?;
                comparison.volatile |= properties.volatile;
                comparison.leakproof &= properties.leakproof;
            }
            Some(comparison)
        }
        Expr::AnyAll {
            operand,
            operator,
            array,
            ..
        } => Some(match ColType::from_oid(type_of(array)?) {
            Some(ColType::Array(element)) => match operator.operator_name() {
                Some(name) => operator_properties(
                    storage,
                    txid,
                    None,
                    name,
                    Some(type_of(operand)?),
                    Some(element.to_coltype().oid()),
                )?,
                None => ProcedureProperties::UNKNOWN,
            },
            _ => ProcedureProperties::UNKNOWN,
        }),
        // At run time an uncorrelated subquery calling no volatile or
        // user-defined function returns one result for the statement, as a
        // stable function does; it is not known to be leakproof. A view
        // pushes no qualifier containing a subquery, as PostgreSQL's
        // `qual_is_pushdown_safe` does not.
        Expr::Subquery(_)
        | Expr::RowSubquery { .. }
        | Expr::InSubquery { .. }
        | Expr::QuantifiedSubquery { .. }
        | Expr::Exists(_)
        | Expr::ArraySubquery(_)
            if stage == AnalysisStage::ViewPushdown =>
        {
            Some(ProcedureProperties::UNKNOWN)
        }
        Expr::Subquery(_)
        | Expr::RowSubquery { .. }
        | Expr::InSubquery { .. }
        | Expr::QuantifiedSubquery { .. }
        | Expr::Exists(_)
        | Expr::ArraySubquery(_) => {
            let mark = arena.mark();
            let stable = super::subquery::stable_uncorrelated(expression, storage, txid, arena);
            // SAFETY: the check returns a flag; nothing it allocated escapes.
            unsafe { arena.rewind_to(mark) };
            Some(if stable {
                ProcedureProperties {
                    volatile: false,
                    leakproof: false,
                }
            } else {
                ProcedureProperties::UNKNOWN
            })
        }
        _ => None,
    };
    if let Some(procedure) = procedure {
        result.volatile |= procedure.volatile;
        result.leaks_columns |= result.columns && !procedure.leakproof;
    }
    Ok(result)
}

fn contains_columns(expression: &Expr<'_>) -> Result<bool, SqlError> {
    if matches!(
        expression,
        Expr::Column { .. }
            | Expr::SchemaColumn { .. }
            | Expr::WholeRow(_)
            | Expr::RecursiveState { .. }
    ) {
        return Ok(true);
    }
    let mut columns = false;
    super::walk_children(expression, &mut |child| {
        columns |= contains_columns(child)?;
        Ok(())
    })?;
    Ok(columns)
}

fn parameter_conversion_properties(
    storage: &Storage,
    txid: u32,
    source: i32,
    target: i32,
    ctype: ColType,
) -> Result<ProcedureProperties, SqlError> {
    // Domain checks, including those in named arrays/composites, are executable
    // expressions. Equal outer OIDs do not establish a binary-only conversion.
    if matches!(
        crate::storage::NamedTypeKind::from_oid(target),
        Some((crate::storage::NamedTypeKind::Domain, _, _))
    ) || matches!(
        ctype,
        ColType::Composite(_)
            | ColType::Array(
                crate::sql::types::ArrElem::Domain { .. }
                    | crate::sql::types::ArrElem::Composite(_)
            )
    ) {
        return Ok(ProcedureProperties::UNKNOWN);
    }
    cast_properties(
        storage,
        txid,
        source,
        target,
        CastBinding::Resolve,
        -1,
        crate::sql::eval::DefinedCastContext::Implicit,
    )
}
