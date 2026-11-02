//! Resolved expression effects govern projection reuse and security barriers.

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
) -> ProcedureProperties {
    if source == target && type_mod < 0 && ColType::from_oid(target).is_some() {
        return ProcedureProperties::BINARY;
    }
    let mut properties = if binding == CastBinding::Resolve
        && let Some((_, cast)) = storage.cast_for_oids(source, target, txid)
    {
        match cast.method {
            crate::storage::CastMethod::Function(function) => {
                routine_properties(storage, txid, function)
            }
            crate::storage::CastMethod::Binary => ProcedureProperties::BINARY,
            crate::storage::CastMethod::InOut => ProcedureProperties::from_builtin(
                crate::sql::catalog::builtin_inout_cast_properties(source, target),
            ),
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
    properties
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
                let conversion =
                    cast_properties(storage, txid, actual, expected, CastBinding::Resolve, -1);
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

/// A non-leakproof function matters at a barrier only when its arguments can
/// expose a row column. A volatile constant call can still move independently.
pub(super) fn expression_properties(
    expression: &Expr<'_>,
    columns: &dyn ColTypeResolver,
    storage: &Storage,
    txid: u32,
    stage: AnalysisStage,
) -> Result<ExpressionProperties, SqlError> {
    let mut result = ExpressionProperties {
        volatile: false,
        leaks_columns: false,
        columns: false,
    };
    super::walk_children(expression, &mut |child| {
        let child = expression_properties(child, columns, storage, txid, stage)?;
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
        Expr::Unary { operator, operand } => {
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
        Expr::Call {
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
            if let Some((schema, operator)) = crate::sql::ast::catalog_operator_call(name) {
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
                let routine = if let Some(oid) = bound_oid {
                    storage.function_for_captured_call(*oid, argument_names, types, *variadic, txid)
                } else if argument_names.is_empty() {
                    storage.function_for_call_syntax_oids(name, types, *variadic, txid)
                } else {
                    storage.function_for_named_call_oids(
                        name,
                        argument_names,
                        types,
                        *variadic,
                        txid,
                    )
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
                            );
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
                        crate::sql::catalog::builtin_routine_properties(name, *bound_oid, types),
                    ),
                })
            }
        }
        Expr::Cast {
            operand,
            type_name,
            binding,
            type_mod,
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
            Some(target.map_or(ProcedureProperties::UNKNOWN, |target| {
                cast_properties(storage, txid, source, target, *binding, *type_mod)
            }))
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
        Expr::Subquery(_)
        | Expr::RowSubquery { .. }
        | Expr::InSubquery { .. }
        | Expr::QuantifiedSubquery { .. }
        | Expr::Exists(_)
        | Expr::ArraySubquery(_) => Some(ProcedureProperties::UNKNOWN),
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
) -> ProcedureProperties {
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
        return ProcedureProperties::UNKNOWN;
    }
    cast_properties(storage, txid, source, target, CastBinding::Resolve, -1)
}
