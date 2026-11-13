//! Array scalar built-ins.
//!
//! Covers comparison (`array_eq`/`array_ne`/`array_lt`/`array_le`/`array_gt`/
//! `array_ge`/`btarraycmp`), size and bounds (`array_length`/`cardinality`/`array_upper`/
//! `array_lower`, `array_ndims`/`array_dims`), search (`array_position`/
//! `array_positions`), mutation (`array_append`/`array_prepend`, `array_cat`,
//! `array_remove`/`array_replace`, `trim_array`), and conversions
//! (`array_to_string`, `string_to_array`, `array_fill`). The set-returning
//! `unnest` / `generate_subscripts` are expanded by the set-returning-function
//! machinery and stay in the router.

use crate::sql::array;
use crate::sql::ast::Expr;
use crate::sql::types::{ArrElem, ColType, Datum};
use crate::sql_err;

use super::super::{
    ColumnLookup, EvalHooks, SqlError, arena_full, arity_err, cast_to, eval_full, load_array,
    sqlstate, text_arg, text_view,
};

/// Compatible-array declarations promote PostgreSQL's catalog vectors to
/// ordinary arrays; the existing conversion preserves their zero lower bound.
fn compatible_array_input<'a>(
    value: Datum<'a>,
    arena: &'a crate::mem::arena::Arena,
) -> Result<Datum<'a>, SqlError> {
    match value {
        Datum::Int2Vector { .. } => cast_to(value, ColType::Array(ArrElem::Int2), arena),
        Datum::OidVector { .. } => cast_to(value, ColType::Array(ArrElem::Oid), arena),
        _ => Ok(value),
    }
}

/// How one argument takes part in an `anycompatible` call.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum CompatibleRole {
    Array,
    Element,
}

/// The `anycompatible` roles of the array built-ins' inputs, as declared in
/// PostgreSQL 18.6 `pg_proc`.
fn compatible_roles(name: &str) -> Option<&'static [CompatibleRole]> {
    use CompatibleRole::{Array, Element};
    Some(match name {
        "array_append" | "array_remove" | "array_position" | "array_positions" => &[Array, Element],
        "array_prepend" => &[Element, Array],
        "array_cat" => &[Array, Array],
        "array_replace" => &[Array, Element, Element],
        _ => return None,
    })
}

/// Resolves an `anycompatible` call as PostgreSQL's parse analysis does: the
/// common element type comes from the inputs' declared types, untyped literals
/// read their text as that type, and typed inputs convert implicitly. The
/// values in `values` are replaced by their coerced forms; the common element
/// is returned.
pub(crate) fn coerce_compatible_arguments<'a>(
    name: &str,
    roles: &[CompatibleRole],
    args: &[&Expr<'a>],
    values: &mut [Datum<'a>],
    arena: &'a crate::mem::arena::Arena,
    row: &impl ColumnLookup<'a>,
    hooks: &EvalHooks<'_, 'a>,
) -> Result<ArrElem, SqlError> {
    let mut inputs = [(false, 0); 3];
    for (index, role) in roles.iter().enumerate() {
        inputs[index] = (
            *role == CompatibleRole::Array,
            super::super::expression_type_identity(args[index], row, hooks)?.routine_argument_oid(),
        );
    }
    let inputs = &inputs[..roles.len()];
    let element_oid = if inputs
        .iter()
        .all(|(_, oid)| *oid == crate::sql::types::oid::UNKNOWN)
    {
        return Err(sql_err!(
            sqlstate::DATATYPE_MISMATCH,
            "could not determine polymorphic type anycompatible because input has type unknown"
        ));
    } else {
        match hooks.catalog {
            Some(catalog) => catalog.compatible_element_oid(inputs),
            None => crate::storage::builtin_compatible_element_oid(inputs),
        }
        .ok_or_else(|| {
            sql_err!(
                sqlstate::UNDEFINED_FUNCTION,
                "function {} does not exist",
                name
            )
        })?
    };
    let element = hooks
        .catalog
        .and_then(|catalog| catalog.domain_base_type(element_oid))
        .or_else(|| ColType::from_oid(element_oid))
        .and_then(ArrElem::from_coltype)
        .ok_or_else(|| {
            sql_err!(
                sqlstate::FEATURE_NOT_SUPPORTED,
                "{} does not support element type {}",
                name,
                element_oid
            )
        })?;
    for (index, (array, input_oid)) in inputs.iter().enumerate() {
        let value = values[index];
        if value.is_null() {
            continue;
        }
        let target = if *array {
            ColType::Array(element)
        } else {
            element.to_coltype()
        };
        values[index] = if *input_oid == crate::sql::types::oid::UNKNOWN && element.is_named() {
            let catalog = hooks.catalog.ok_or_else(|| {
                sql_err!(
                    sqlstate::INTERNAL_ERROR,
                    "{} named argument types require a catalog",
                    name
                )
            })?;
            let element_name = catalog.type_name(element_oid, arena)?.ok_or_else(|| {
                sql_err!(
                    sqlstate::UNDEFINED_OBJECT,
                    "{} argument type no longer exists",
                    name
                )
            })?;
            let type_name = if *array {
                let mut written = crate::util::StackStr::<264>::new();
                use core::fmt::Write as _;
                write!(written, "{element_name}[]").map_err(|_| arena_full())?;
                arena
                    .alloc_str(written.as_str())
                    .map_err(|_| arena_full())?
            } else {
                element_name
            };
            catalog
                .cast_user_type(type_name, value, arena, hooks.sequences)?
                .ok_or_else(|| {
                    sql_err!(
                        sqlstate::UNDEFINED_OBJECT,
                        "{} argument type no longer exists",
                        name
                    )
                })?
        } else {
            let value = compatible_array_input(value, arena)?;
            match value {
                Datum::Array {
                    element: actual, ..
                } if *array && actual == element => value,
                _ if !*array && ArrElem::from_datum(&value) == Some(element) => value,
                _ => cast_to(value, target, arena)?,
            }
        };
    }
    Ok(element)
}

/// The common type that PostgreSQL's `select_common_type` gives the inputs of
/// `COALESCE`, `GREATEST` and `LEAST`, or `None` when it is a catalog-defined
/// type or the typed inputs have none (their comparison reports the mismatch).
/// Untyped literals alone resolve to `text`. Returns each input's declared
/// type alongside, so callers convert exactly the untyped ones.
/// Each argument's declared type OID, `unknown` for an untyped literal.
pub(crate) fn declared_argument_oids<'a>(
    args: &[&Expr<'a>],
    arena: &'a crate::mem::arena::Arena,
    row: &impl ColumnLookup<'a>,
    hooks: &EvalHooks<'_, 'a>,
) -> Result<&'a [i32], SqlError> {
    let declared = arena
        .alloc_slice_with(args.len(), |_| crate::sql::types::oid::UNKNOWN)
        .map_err(|_| arena_full())?;
    for (input, argument) in declared.iter_mut().zip(args) {
        *input =
            super::super::expression_type_identity(argument, row, hooks)?.routine_argument_oid();
    }
    Ok(declared)
}

pub(crate) fn common_argument_type<'a>(
    args: &[&Expr<'a>],
    arena: &'a crate::mem::arena::Arena,
    row: &impl ColumnLookup<'a>,
    hooks: &EvalHooks<'_, 'a>,
) -> Result<(Option<ColType>, &'a [i32]), SqlError> {
    let declared = declared_argument_oids(args, arena, row, hooks)?;
    let inputs = arena
        .alloc_slice_with(args.len(), |index| (false, declared[index]))
        .map_err(|_| arena_full())?;
    if declared
        .iter()
        .all(|oid| *oid == crate::sql::types::oid::UNKNOWN)
    {
        return Ok((Some(ColType::Text), declared));
    }
    let common = match hooks.catalog {
        Some(catalog) => catalog.compatible_element_oid(inputs),
        None => crate::storage::builtin_compatible_element_oid(inputs),
    };
    Ok((
        common
            .filter(|oid| crate::storage::NamedTypeKind::from_oid(*oid).is_none())
            .and_then(ColType::from_oid),
        declared,
    ))
}

#[derive(Clone, Copy)]
enum ComparisonFunction {
    Equal,
    NotEqual,
    Less,
    LessEqual,
    Greater,
    GreaterEqual,
    Compare,
}

impl ComparisonFunction {
    fn named(name: &str) -> Option<Self> {
        Some(match name {
            "array_eq" => Self::Equal,
            "array_ne" => Self::NotEqual,
            "array_lt" => Self::Less,
            "array_le" => Self::LessEqual,
            "array_gt" => Self::Greater,
            "array_ge" => Self::GreaterEqual,
            "btarraycmp" => Self::Compare,
            _ => return None,
        })
    }

    fn result_type(self) -> ColType {
        match self {
            Self::Compare => ColType::Int4,
            _ => ColType::Bool,
        }
    }

    fn result(self, order: core::cmp::Ordering) -> Datum<'static> {
        match self {
            Self::Equal => Datum::Bool(order.is_eq()),
            Self::NotEqual => Datum::Bool(!order.is_eq()),
            Self::Less => Datum::Bool(order.is_lt()),
            Self::LessEqual => Datum::Bool(!order.is_gt()),
            Self::Greater => Datum::Bool(order.is_gt()),
            Self::GreaterEqual => Datum::Bool(!order.is_lt()),
            Self::Compare => Datum::Int4(match order {
                core::cmp::Ordering::Less => -1,
                core::cmp::Ordering::Equal => 0,
                core::cmp::Ordering::Greater => 1,
            }),
        }
    }
}

fn comparison_array_type(
    name: &str,
    arguments: [i32; 2],
    domain_base: impl Fn(i32) -> Option<ColType>,
) -> Result<ColType, SqlError> {
    let mut selected = None;
    for oid in arguments {
        if oid == crate::sql::types::oid::UNKNOWN {
            continue;
        }
        let Some(ctype @ ColType::Array(_)) = domain_base(oid).or_else(|| ColType::from_oid(oid))
        else {
            return Err(sql_err!(
                sqlstate::UNDEFINED_FUNCTION,
                "function {} does not exist for input types {} and {}",
                name,
                arguments[0],
                arguments[1]
            ));
        };
        if selected.is_some_and(|prior| prior != ctype) {
            return Err(sql_err!(
                sqlstate::UNDEFINED_FUNCTION,
                "function {} does not exist for input types {} and {}",
                name,
                arguments[0],
                arguments[1]
            ));
        }
        selected = Some(ctype);
    }
    selected.ok_or_else(|| {
        sql_err!(
            sqlstate::DATATYPE_MISMATCH,
            "could not determine polymorphic type because input has type unknown"
        )
    })
}

pub(crate) fn describe_comparison(
    name: &str,
    args: &[&Expr<'_>],
    star: bool,
    variadic: bool,
    argument_names: &[Option<&str>],
    columns: &dyn crate::sql::exec::ColTypeResolver,
) -> Option<Result<(i32, i16), SqlError>> {
    let function = ComparisonFunction::named(name)?;
    Some((|| {
        if star || variadic || !argument_names.is_empty() || args.len() != 2 {
            return Err(arity_err(name, args.len()));
        }
        comparison_array_type(
            name,
            [
                crate::sql::exec::infer_routine_argument_oid(args[0], columns)?,
                crate::sql::exec::infer_routine_argument_oid(args[1], columns)?,
            ],
            |oid| columns.domain_base_type(oid),
        )?;
        let result = function.result_type();
        Ok((result.oid(), result.typlen()))
    })())
}

#[allow(clippy::too_many_arguments)]
fn vector_bounds<'a>(
    name: &str,
    length: usize,
    lower_bound: i32,
    args: &[&Expr<'a>],
    arena: &'a crate::mem::arena::Arena,
    params: &[Datum<'a>],
    row: &impl ColumnLookup<'a>,
    hooks: &EvalHooks<'_, 'a>,
) -> Result<Datum<'a>, SqlError> {
    if name == "cardinality" {
        return Ok(Datum::Int4(length as i32));
    }
    let dimension = match eval_full(args[1], arena, params, row, hooks)? {
        Datum::Int2(value) => value as i64,
        Datum::Int4(value) => value as i64,
        Datum::Int8(value) => value,
        Datum::Null => return Ok(Datum::Null),
        value => return Err(crate::sql::eval::function_argument_mismatch(name, &value)),
    };
    if dimension != 1 {
        return Ok(Datum::Null);
    }
    Ok(match name {
        "array_length" => Datum::Int4(length as i32),
        "array_upper" => Datum::Int4((i64::from(lower_bound) + length as i64 - 1) as i32),
        "array_lower" => Datum::Int4(lower_bound),
        _ => unreachable!(),
    })
}

/// Handles the array scalar family. Returns `None` if `name` is not one of
/// these functions, leaving the router to keep matching.
#[allow(clippy::too_many_arguments)]
pub(crate) fn dispatch<'a>(
    name: &str,
    args: &[&Expr<'a>],
    star: bool,
    arena: &'a crate::mem::arena::Arena,
    params: &[Datum<'a>],
    row: &impl ColumnLookup<'a>,
    hooks: &EvalHooks<'_, 'a>,
) -> Option<Result<Datum<'a>, SqlError>> {
    if ComparisonFunction::named(name).is_none()
        && !matches!(
            name,
            "array_length"
                | "cardinality"
                | "array_upper"
                | "array_lower"
                | "array_position"
                | "array_positions"
                | "array_append"
                | "array_prepend"
                | "array_cat"
                | "array_remove"
                | "array_replace"
                | "trim_array"
                | "array_ndims"
                | "array_dims"
                | "array_to_string"
                | "array_fill"
                | "string_to_array"
                | "array_reverse"
                | "array_sort"
                | "array_sample"
                | "array_shuffle"
        )
    {
        return None;
    }
    let arity = |n: usize| -> Result<(), SqlError> {
        if args.len() != n || star {
            Err(sql_err!(
                sqlstate::UNDEFINED_FUNCTION,
                "function {}(...) with {} arguments does not exist",
                name,
                if star { 1 } else { args.len() }
            ))
        } else {
            Ok(())
        }
    };
    Some((|| -> Result<Datum<'a>, SqlError> {
        match name {
            name if ComparisonFunction::named(name).is_some() => {
                arity(2)?;
                let input_oids = [
                    super::super::expression_type_identity(args[0], row, hooks)?
                        .routine_argument_oid(),
                    super::super::expression_type_identity(args[1], row, hooks)?
                        .routine_argument_oid(),
                ];
                let target = comparison_array_type(name, input_oids, |oid| {
                    hooks
                        .catalog
                        .and_then(|catalog| catalog.domain_base_type(oid))
                })?;
                let mut values = [Datum::Null; 2];
                for (index, value) in values.iter_mut().enumerate() {
                    *value = eval_full(args[index], arena, params, row, hooks)?;
                    if input_oids[index] == crate::sql::types::oid::UNKNOWN {
                        if !value.is_null()
                            && matches!(target, ColType::Array(element) if element.is_named())
                        {
                            let catalog = hooks.catalog.ok_or_else(|| {
                                sql_err!(
                                    sqlstate::INTERNAL_ERROR,
                                    "array input type catalog is unavailable"
                                )
                            })?;
                            let name =
                                catalog.type_name(target.oid(), arena)?.ok_or_else(|| {
                                    sql_err!(
                                        sqlstate::UNDEFINED_OBJECT,
                                        "array input type no longer exists"
                                    )
                                })?;
                            *value = catalog
                                .cast_user_type(name, *value, arena, hooks.sequences)?
                                .ok_or_else(|| {
                                    sql_err!(
                                        sqlstate::UNDEFINED_OBJECT,
                                        "array input type no longer exists"
                                    )
                                })?;
                        } else {
                            *value = cast_to(*value, target, arena)?;
                        }
                    }
                }
                if values.iter().any(Datum::is_null) {
                    return Ok(Datum::Null);
                }
                let (Datum::Array { element: left, .. }, Datum::Array { element: right, .. }) =
                    (values[0], values[1])
                else {
                    return Err(crate::sql::eval::operand_type_mismatch(&values[0]));
                };
                if left != right {
                    return Err(arity_err(name, args.len()));
                }
                let collation = super::super::resolve_comparison_collation(
                    args[0],
                    args[1],
                    row,
                    hooks.catalog,
                )?;
                let order = super::super::compare_datums_with_catalog(
                    collation,
                    hooks.catalog,
                    &values[0],
                    &values[1],
                )?;
                Ok(ComparisonFunction::named(name)
                    .expect("matched comparator")
                    .result(order))
            }
            "array_length" | "cardinality" | "array_upper" | "array_lower" => {
                arity(if name == "cardinality" { 1 } else { 2 })?;
                let a = eval_full(args[0], arena, params, row, hooks)?;
                match a {
                    Datum::Array { raw, .. } => {
                        let shape = array::shape(raw).expect("array datum invariant");
                        if name == "cardinality" {
                            return Ok(Datum::Int4(shape.element_count() as i32));
                        }
                        let dimension = match eval_full(args[1], arena, params, row, hooks)? {
                            Datum::Int2(value) => value as i64,
                            Datum::Int4(value) => value as i64,
                            Datum::Int8(value) => value,
                            Datum::Null => return Ok(Datum::Null),
                            value => {
                                return Err(crate::sql::eval::function_argument_mismatch(
                                    name, &value,
                                ));
                            }
                        };
                        if dimension < 1 || dimension as usize > shape.dimension_count() {
                            return Ok(Datum::Null);
                        }
                        let index = dimension as usize - 1;
                        Ok(match name {
                            "array_length" => Datum::Int4(shape.dimension(index).unwrap() as i32),
                            "array_upper" => Datum::Int4(shape.upper_bound(index).unwrap()),
                            "array_lower" => Datum::Int4(shape.lower_bound(index).unwrap()),
                            _ => unreachable!(),
                        })
                    }
                    Datum::Int2Vector { raw, lower_bound } => vector_bounds(
                        name,
                        raw.len() / 2,
                        lower_bound,
                        args,
                        arena,
                        params,
                        row,
                        hooks,
                    ),
                    Datum::OidVector { raw, lower_bound } => vector_bounds(
                        name,
                        raw.len() / 4,
                        lower_bound,
                        args,
                        arena,
                        params,
                        row,
                        hooks,
                    ),
                    Datum::Null => Ok(Datum::Null),
                    _ => Err(crate::sql::eval::function_argument_mismatch(
                        "array_length",
                        &a,
                    )),
                }
            }
            "array_position" => {
                // Finds the first matching PostgreSQL subscript, rather than a
                // storage offset. The optional start is a subscript too.
                if !(2..=3).contains(&args.len()) || star {
                    return Err(sql_err!(
                        sqlstate::UNDEFINED_FUNCTION,
                        "function array_position(...) with {} arguments does not exist",
                        if star { 1 } else { args.len() }
                    ));
                }
                let mut values = [
                    eval_full(args[0], arena, params, row, hooks)?,
                    eval_full(args[1], arena, params, row, hooks)?,
                ];
                coerce_compatible_arguments(
                    name,
                    compatible_roles(name).expect("compatible array function"),
                    args,
                    &mut values,
                    arena,
                    row,
                    hooks,
                )?;
                let [a, target] = values;
                let (element, raw) = match a {
                    Datum::Array { element, raw } => (element, raw),
                    Datum::Null => return Ok(Datum::Null),
                    _ => {
                        return Err(crate::sql::eval::function_argument_mismatch(
                            "array_position",
                            &a,
                        ));
                    }
                };
                let shape = array::shape(raw).expect("array datum invariant");
                if shape.dimension_count() > 1 {
                    return Err(sql_err!(
                        sqlstate::FEATURE_NOT_SUPPORTED,
                        "searching for elements in multidimensional arrays is not supported"
                    ));
                }
                require_search_equality(element, shape.dimension_count(), &target)?;
                let lower = shape.lower_bound(0).unwrap_or(1);
                let start = if args.len() == 3 {
                    match eval_full(args[2], arena, params, row, hooks)? {
                        Datum::Int2(value) => i64::from(value),
                        Datum::Int4(value) => i64::from(value),
                        Datum::Int8(value) => value,
                        Datum::Null => {
                            return Err(sql_err!(
                                sqlstate::NULL_VALUE_NOT_ALLOWED,
                                "initial position must not be null"
                            ));
                        }
                        value => {
                            return Err(crate::sql::eval::function_argument_mismatch(
                                "array_position",
                                &value,
                            ));
                        }
                    }
                } else {
                    i64::from(lower)
                };
                let first = usize::try_from(start.saturating_sub(i64::from(lower)).max(0))
                    .unwrap_or(usize::MAX);
                for (offset, el) in array::elements(raw, element).skip(first).enumerate() {
                    let hit = if target.is_null() {
                        el.is_null()
                    } else if el.is_null() {
                        false
                    } else {
                        search_equal(&el, &target, hooks.catalog)?
                    };
                    if hit {
                        let i = first + offset;
                        let index = i32::try_from(i).map_err(|_| {
                            sql_err!(sqlstate::PROGRAM_LIMIT_EXCEEDED, "array value too large")
                        })?;
                        return Ok(Datum::Int4(lower.checked_add(index).ok_or_else(|| {
                            sql_err!(
                                sqlstate::NUMERIC_OUT_OF_RANGE,
                                "array subscript out of range"
                            )
                        })?));
                    }
                }
                Ok(Datum::Null)
            }
            "array_positions" => {
                // 1-based indices of every element equal to the target (NULL-safe),
                // as an int[] (`{}` when none); NULL only for a NULL array argument.
                arity(2)?;
                let mut values = [
                    eval_full(args[0], arena, params, row, hooks)?,
                    eval_full(args[1], arena, params, row, hooks)?,
                ];
                coerce_compatible_arguments(
                    name,
                    compatible_roles(name).expect("compatible array function"),
                    args,
                    &mut values,
                    arena,
                    row,
                    hooks,
                )?;
                let [a, target] = values;
                let (element, raw) = match a {
                    Datum::Array { element, raw } => (element, raw),
                    Datum::Null => return Ok(Datum::Null),
                    _ => {
                        return Err(crate::sql::eval::function_argument_mismatch(
                            "array_positions",
                            &a,
                        ));
                    }
                };
                let shape = array::shape(raw).expect("array datum invariant");
                if shape.dimension_count() > 1 {
                    return Err(sql_err!(
                        sqlstate::FEATURE_NOT_SUPPORTED,
                        "searching for elements in multidimensional arrays is not supported"
                    ));
                }
                require_search_equality(element, shape.dimension_count(), &target)?;
                let lower = shape.lower_bound(0).unwrap_or(1);
                let matches = |el: &Datum| -> Result<bool, SqlError> {
                    Ok(if target.is_null() {
                        el.is_null()
                    } else if el.is_null() {
                        false
                    } else {
                        search_equal(el, &target, hooks.catalog)?
                    })
                };
                let mut count = 0usize;
                for el in array::elements(raw, element) {
                    if matches(&el)? {
                        count += 1;
                    }
                }
                let positions: &mut [Datum] = arena
                    .alloc_slice_with(count, |_| Datum::Null)
                    .map_err(|_| arena_full())?;
                let mut at = 0usize;
                for (i, el) in array::elements(raw, element).enumerate() {
                    if matches(&el)? {
                        let index = i32::try_from(i).map_err(|_| {
                            sql_err!(sqlstate::PROGRAM_LIMIT_EXCEEDED, "array value too large")
                        })?;
                        positions[at] = Datum::Int4(lower.checked_add(index).ok_or_else(|| {
                            sql_err!(
                                sqlstate::NUMERIC_OUT_OF_RANGE,
                                "array subscript out of range"
                            )
                        })?);
                        at += 1;
                    }
                }
                Ok(Datum::Array {
                    element: ArrElem::Int4,
                    raw: array::build(positions, arena)?,
                })
            }
            // `array_append(arr, elem)` / `array_prepend(elem, arr)`: a NULL array
            // is treated as empty (its element type taken from `elem`).
            "array_append" | "array_prepend" => {
                arity(2)?;
                let (array_index, elem_index) = if name == "array_append" {
                    (0, 1)
                } else {
                    (1, 0)
                };
                let mut values = [
                    eval_full(args[0], arena, params, row, hooks)?,
                    eval_full(args[1], arena, params, row, hooks)?,
                ];
                let element = coerce_compatible_arguments(
                    name,
                    compatible_roles(name).expect("compatible array function"),
                    args,
                    &mut values,
                    arena,
                    row,
                    hooks,
                )?;
                let (arr, elem) = (values[array_index], values[elem_index]);
                let (source, raw) = match arr {
                    Datum::Array { element, raw } => (element, Some(raw)),
                    Datum::Null => (element, None),
                    _ => {
                        return Err(crate::sql::eval::function_argument_mismatch(name, &arr));
                    }
                };
                // An untyped literal that fails its element cast is positioned.
                let untyped = matches!(args[elem_index], Expr::Str(_)) && raw.is_some();
                let source_count = raw.map_or(0, array::len);
                let items = array::alloc_items(arena, source_count + 1)?;
                let mut n = 0usize;
                let source_shape = raw.map(|raw| array::shape(raw).expect("array datum invariant"));
                if source_shape.is_some_and(|shape| shape.dimension_count() > 1) {
                    return Err(sql_err!(
                        sqlstate::ARRAY_SUBSCRIPT_ERROR,
                        "cannot append an element to a multidimensional array"
                    ));
                }
                let coerced = if elem.is_null() {
                    Datum::Null
                } else {
                    let cast = cast_to(elem, element.to_coltype(), arena);
                    durable_element(
                        match (cast, args[elem_index]) {
                            (Err(error), Expr::Str(literal)) if untyped => {
                                return Err(crate::sql::error_position::at_subject(error, literal));
                            }
                            (cast, _) => cast?,
                        },
                        element,
                        hooks.catalog,
                        arena,
                    )?
                };
                if name == "array_prepend" {
                    items[n] = coerced;
                    n += 1;
                }
                if let Some(raw) = raw {
                    n = load_array(raw, source, element, &mut *items, n, arena)?;
                }
                if name == "array_append" {
                    if n == items.len() {
                        return Err(sql_err!(
                            sqlstate::PROGRAM_LIMIT_EXCEEDED,
                            "array value too large"
                        ));
                    }
                    items[n] = coerced;
                    n += 1;
                }
                Ok(Datum::Array {
                    element,
                    raw: array::build_shaped(
                        &items[..n],
                        array::Shape::new(
                            &[n],
                            &[match (source_shape, name) {
                                (Some(shape), "array_prepend") => shape.lower_bound(0).unwrap(),
                                (Some(shape), _) => shape.lower_bound(0).unwrap(),
                                (None, _) => 1,
                            }],
                        )?,
                        arena,
                    )?,
                })
            }
            // `array_cat(a, b)`: concatenate two arrays of the same element type.
            "array_cat" => {
                arity(2)?;
                let mut values = [
                    eval_full(args[0], arena, params, row, hooks)?,
                    eval_full(args[1], arena, params, row, hooks)?,
                ];
                coerce_compatible_arguments(
                    name,
                    compatible_roles(name).expect("compatible array function"),
                    args,
                    &mut values,
                    arena,
                    row,
                    hooks,
                )?;
                let [a, b] = values;
                match (a, b) {
                    (Datum::Array { .. }, Datum::Array { .. }) => {
                        super::super::array_concat(a, b, arena)
                    }
                    (Datum::Array { element, raw }, Datum::Null)
                    | (Datum::Null, Datum::Array { element, raw }) => {
                        Ok(Datum::Array { element, raw })
                    }
                    (Datum::Null, Datum::Null) => Ok(Datum::Null),
                    _ => Err(crate::sql::eval::function_argument_mismatch(
                        "array_cat",
                        &a,
                    )),
                }
            }
            // `array_remove(arr, elem)`: drop every element equal to `elem`
            // (NULL-safe). `array_replace(arr, from, to)`: replace every match.
            "array_remove" | "array_replace" => {
                let is_replace = name == "array_replace";
                arity(if is_replace { 3 } else { 2 })?;
                let mut values = [Datum::Null; 3];
                for (value, argument) in values.iter_mut().zip(args) {
                    *value = eval_full(argument, arena, params, row, hooks)?;
                }
                let element = coerce_compatible_arguments(
                    name,
                    compatible_roles(name).expect("compatible array function"),
                    args,
                    &mut values[..args.len()],
                    arena,
                    row,
                    hooks,
                )?;
                let [arr, target, to] = values;
                let (source, raw) = match arr {
                    Datum::Array { element, raw } => (element, raw),
                    Datum::Null => return Ok(Datum::Null),
                    _ => return Err(crate::sql::eval::function_argument_mismatch(name, &arr)),
                };
                let shape = array::shape(raw).expect("array datum invariant");
                if !is_replace && shape.dimension_count() > 1 {
                    return Err(sql_err!(
                        sqlstate::FEATURE_NOT_SUPPORTED,
                        "array_remove does not support multidimensional arrays"
                    ));
                }
                // PostgreSQL needs the element equality for any non-empty
                // array, even to remove or replace NULLs.
                if shape.dimension_count() > 0 && !source.to_coltype().has_default_equality() {
                    return Err(crate::sql::eval::operators::could_not_identify_equality(
                        source.to_coltype(),
                    ));
                }
                // The arguments share one element type; a replacement entering
                // a named-composite array takes the elements' durable form.
                debug_assert_eq!(source, element);
                let replacement = if to.is_null() {
                    Datum::Null
                } else {
                    durable_element(to, element, hooks.catalog, arena)?
                };
                let to_coltype = element.to_coltype();
                let items = array::alloc_items(arena, shape.element_count())?;
                let mut n = 0usize;
                for el in array::elements(raw, source) {
                    let matches = if target.is_null() {
                        el.is_null()
                    } else if el.is_null() {
                        false
                    } else {
                        search_equal(&el, &target, hooks.catalog)?
                    };
                    if is_replace {
                        items[n] = if matches {
                            replacement
                        } else if el.is_null() || source == element {
                            el
                        } else {
                            cast_to(el, to_coltype, arena)?
                        };
                        n += 1;
                    } else if !matches {
                        items[n] = el;
                        n += 1;
                    }
                }
                Ok(Datum::Array {
                    element,
                    raw: if is_replace {
                        array::build_shaped(&items[..n], shape, arena)?
                    } else {
                        array::build_shaped(
                            &items[..n],
                            array::Shape::new(&[n], &[shape.lower_bound(0).unwrap_or(1)])?,
                            arena,
                        )?
                    },
                })
            }
            // `trim_array(arr, n)`: drop the last `n` elements; `n` must be in range.
            "trim_array" => {
                arity(2)?;
                let arr = eval_full(args[0], arena, params, row, hooks)?;
                let count = eval_full(args[1], arena, params, row, hooks)?;
                let (element, raw) = match arr {
                    Datum::Array { element, raw } => (element, raw),
                    Datum::Null => return Ok(Datum::Null),
                    _ => {
                        return Err(crate::sql::eval::function_argument_mismatch(
                            "trim_array",
                            &arr,
                        ));
                    }
                };
                if array::shape(raw)
                    .expect("array datum invariant")
                    .dimension_count()
                    > 1
                {
                    return Err(sql_err!(
                        sqlstate::FEATURE_NOT_SUPPORTED,
                        "trim_array does not support multidimensional arrays"
                    ));
                }
                if count.is_null() {
                    return Ok(Datum::Null);
                }
                let total = array::len(raw);
                let trim = match count {
                    Datum::Int2(v) => v as i64,
                    Datum::Int4(v) => v as i64,
                    Datum::Int8(v) => v,
                    _ => {
                        return Err(crate::sql::eval::function_argument_mismatch(
                            "trim_array",
                            &count,
                        ));
                    }
                };
                if trim < 0 || trim as usize > total {
                    return Err(sql_err!(
                        sqlstate::ARRAY_SUBSCRIPT_ERROR,
                        "number of elements to trim must be between 0 and {}",
                        total
                    ));
                }
                let keep = total - trim as usize;
                let items = array::alloc_items(arena, keep)?;
                for (item, value) in items.iter_mut().zip(array::elements(raw, element)) {
                    *item = value;
                }
                Ok(Datum::Array {
                    element,
                    raw: array::build(items, arena)?,
                })
            }
            "array_ndims" | "array_dims" => {
                arity(1)?;
                let arr = eval_full(args[0], arena, params, row, hooks)?;
                let raw = match arr {
                    Datum::Array { raw, .. } => raw,
                    Datum::Int2Vector { raw, lower_bound } => {
                        if name == "array_ndims" {
                            return Ok(Datum::Int4(1));
                        }
                        let bounds = crate::stack_format!(
                            32,
                            "[{}:{}]",
                            lower_bound,
                            i64::from(lower_bound) + raw.len() as i64 / 2 - 1
                        );
                        return Ok(Datum::Text(
                            arena.alloc_str(bounds.as_str()).map_err(|_| arena_full())?,
                        ));
                    }
                    Datum::OidVector { raw, lower_bound } => {
                        if name == "array_ndims" {
                            return Ok(Datum::Int4(1));
                        }
                        let bounds = crate::stack_format!(
                            32,
                            "[{}:{}]",
                            lower_bound,
                            i64::from(lower_bound) + raw.len() as i64 / 4 - 1
                        );
                        return Ok(Datum::Text(
                            arena.alloc_str(bounds.as_str()).map_err(|_| arena_full())?,
                        ));
                    }
                    Datum::Null => return Ok(Datum::Null),
                    _ => return Err(crate::sql::eval::function_argument_mismatch(name, &arr)),
                };
                let shape = array::shape(raw).expect("array datum invariant");
                if shape.dimension_count() == 0 {
                    return Ok(Datum::Null);
                }
                if name == "array_ndims" {
                    Ok(Datum::Int4(shape.dimension_count() as i32))
                } else {
                    let mut text = crate::util::StackStr::<128>::new();
                    for index in 0..shape.dimension_count() {
                        use core::fmt::Write as _;
                        write!(
                            text,
                            "[{}:{}]",
                            shape.lower_bound(index).unwrap(),
                            shape.upper_bound(index).unwrap()
                        )
                        .map_err(|_| arena_full())?;
                    }
                    Ok(Datum::Text(
                        arena.alloc_str(text.as_str()).map_err(|_| arena_full())?,
                    ))
                }
            }
            "array_to_string" => {
                if args.len() != 2 && args.len() != 3 {
                    return Err(sql_err!(
                        sqlstate::UNDEFINED_FUNCTION,
                        "array_to_string expects 2 or 3 arguments"
                    ));
                }
                let a = eval_full(args[0], arena, params, row, hooks)?;
                let delim = eval_full(args[1], arena, params, row, hooks)?;
                // Third argument is the string substituted for NULL elements; when
                // absent (or itself NULL) NULL elements are omitted entirely.
                let nullrep = if args.len() == 3 {
                    match text_view(eval_full(args[2], arena, params, row, hooks)?) {
                        Datum::Null => None,
                        Datum::Text(s) => Some(s),
                        other => {
                            return Err(crate::sql::eval::function_argument_mismatch(
                                "array_to_string",
                                &other,
                            ));
                        }
                    }
                } else {
                    None
                };
                let (element, raw) = match a {
                    Datum::Null => return Ok(Datum::Null),
                    Datum::Array { element, raw } => (element, raw),
                    other => {
                        return Err(crate::sql::eval::function_argument_mismatch(
                            "array_to_string",
                            &other,
                        ));
                    }
                };
                let delim = match text_view(delim) {
                    Datum::Null => return Ok(Datum::Null),
                    Datum::Text(s) => s,
                    other => {
                        return Err(crate::sql::eval::function_argument_mismatch(
                            "array_to_string",
                            &other,
                        ));
                    }
                };
                // Renders the i-th element as text, or `None` to omit it (a NULL
                // element with no null-string replacement).
                let elem_text = |value: Datum<'a>| -> Result<Option<&'a str>, SqlError> {
                    match value {
                        Datum::Null => Ok(nullrep),
                        v => match cast_to(v, ColType::Text, arena)? {
                            Datum::Text(s) => Ok(Some(s)),
                            Datum::Null => Ok(nullrep),
                            other => Err(crate::sql::eval::function_argument_mismatch(
                                "array_to_string",
                                &other,
                            )),
                        },
                    }
                };
                // Pass 1: total byte length; pass 2: fill (elements re-rendered).
                let mut total = 0usize;
                let mut first = true;
                for value in array::elements(raw, element) {
                    if let Some(s) = elem_text(value)? {
                        if !first {
                            total += delim.len();
                        }
                        total += s.len();
                        first = false;
                    }
                }
                let out = arena
                    .alloc_slice_with(total, |_| 0u8)
                    .map_err(|_| arena_full())?;
                let mut at = 0;
                let mut first = true;
                for value in array::elements(raw, element) {
                    if let Some(s) = elem_text(value)? {
                        if !first {
                            out[at..at + delim.len()].copy_from_slice(delim.as_bytes());
                            at += delim.len();
                        }
                        out[at..at + s.len()].copy_from_slice(s.as_bytes());
                        at += s.len();
                        first = false;
                    }
                }
                Ok(Datum::Text(unsafe { core::str::from_utf8_unchecked(out) }))
            }
            // PostgreSQL 18's first-dimension reorderings: each item along the
            // first dimension (a sub-array of a multidimensional array) moves
            // as one block.
            "array_reverse" | "array_sort" | "array_sample" | "array_shuffle" => {
                let (minimum, maximum) = match name {
                    "array_sort" => (1, 3),
                    "array_sample" => (2, 2),
                    _ => (1, 1),
                };
                if star || !(minimum..=maximum).contains(&args.len()) {
                    return Err(arity_err(name, args.len()));
                }
                let mut values = [Datum::Null; 3];
                for (value, argument) in values.iter_mut().zip(args) {
                    *value = eval_full(argument, arena, params, row, hooks)?;
                }
                if values[..args.len()].iter().any(Datum::is_null) {
                    return Ok(Datum::Null);
                }
                let (element, raw) = match compatible_array_input(values[0], arena)? {
                    Datum::Array { element, raw } => (element, raw),
                    other => {
                        return Err(crate::sql::eval::function_argument_mismatch(name, &other));
                    }
                };
                let shape = array::shape(raw).expect("array datum invariant");
                let count = shape.dimension(0).unwrap_or(0);
                if shape.dimension_count() == 0 || count == 0 {
                    if name == "array_sample" {
                        let Datum::Int4(size) = cast_to(values[1], ColType::Int4, arena)? else {
                            unreachable!("integer cast")
                        };
                        if size != 0 {
                            return Err(sample_size_error(0));
                        }
                    }
                    return Ok(values[0]);
                }
                let block = shape.element_count() / count;
                let items = array::alloc_items(arena, shape.element_count())?;
                for (item, value) in items.iter_mut().zip(array::elements(raw, element)) {
                    *item = value;
                }
                let order = arena
                    .alloc_slice_with(count, |index| index)
                    .map_err(|_| arena_full())?;
                let mut kept = count;
                match name {
                    "array_reverse" => order.reverse(),
                    "array_shuffle" | "array_sample" => {
                        if name == "array_sample" {
                            let Datum::Int4(size) = cast_to(values[1], ColType::Int4, arena)?
                            else {
                                unreachable!("integer cast")
                            };
                            kept = usize::try_from(size)
                                .ok()
                                .filter(|size| *size <= count)
                                .ok_or_else(|| sample_size_error(count))?;
                        }
                        // PostgreSQL's partial Fisher-Yates over the session
                        // random sequence (`setseed` repeats it).
                        for index in 0..kept {
                            let draw = crate::sql::guc::active_random()?;
                            let span = (count - index) as f64;
                            let pick = index + ((draw * span) as usize).min(count - index - 1);
                            order.swap(index, pick);
                        }
                    }
                    _ => {
                        let descending = matches!(values[1], Datum::Bool(true));
                        let nulls_first = match args.len() {
                            3 => matches!(values[2], Datum::Bool(true)),
                            _ => descending,
                        };
                        // PostgreSQL looks up the ordering only to compare two items.
                        if count > 1 && !element.to_coltype().has_builtin_ordering() {
                            return Err(sql_err!(
                                sqlstate::FEATURE_NOT_SUPPORTED,
                                "could not identify a comparison function for type {}",
                                element.to_coltype().name()
                            ));
                        }
                        let collation = super::super::resolve_comparison_collation(
                            args[0],
                            args[0],
                            row,
                            hooks.catalog,
                        )?;
                        let compare =
                            |left: usize, right: usize| -> Result<core::cmp::Ordering, SqlError> {
                                for offset in 0..block {
                                    let (a, b) = (
                                        items[left * block + offset],
                                        items[right * block + offset],
                                    );
                                    let order = match (a.is_null(), b.is_null()) {
                                        (true, true) => core::cmp::Ordering::Equal,
                                        (true, false) if block == 1 => {
                                            return Ok(if nulls_first {
                                                core::cmp::Ordering::Less
                                            } else {
                                                core::cmp::Ordering::Greater
                                            });
                                        }
                                        (false, true) if block == 1 => {
                                            return Ok(if nulls_first {
                                                core::cmp::Ordering::Greater
                                            } else {
                                                core::cmp::Ordering::Less
                                            });
                                        }
                                        // Inside sub-arrays NULL sorts last, as in array comparison.
                                        (true, false) => core::cmp::Ordering::Greater,
                                        (false, true) => core::cmp::Ordering::Less,
                                        (false, false) => {
                                            super::super::operators::compare_datums_with_catalog(
                                                collation,
                                                hooks.catalog,
                                                &a,
                                                &b,
                                            )?
                                        }
                                    };
                                    if order.is_ne() {
                                        return Ok(if descending {
                                            order.reverse()
                                        } else {
                                            order
                                        });
                                    }
                                }
                                Ok(core::cmp::Ordering::Equal)
                            };
                        // Insertion sort keeps memory fixed and stops at the first error.
                        for next in 1..count {
                            let mut at = next;
                            while at > 0 && compare(order[at - 1], order[at])?.is_gt() {
                                order.swap(at - 1, at);
                                at -= 1;
                            }
                        }
                    }
                }
                let output = array::alloc_items(arena, kept * block)?;
                for (slot, index) in order[..kept].iter().enumerate() {
                    output[slot * block..(slot + 1) * block]
                        .copy_from_slice(&items[index * block..(index + 1) * block]);
                }
                let mut dimensions = [0usize; array::MAX_DIMENSIONS];
                let mut lower_bounds = [1i32; array::MAX_DIMENSIONS];
                for index in 0..shape.dimension_count() {
                    dimensions[index] = shape.dimension(index).unwrap_or(0);
                    lower_bounds[index] = shape.lower_bound(index).unwrap_or(1);
                }
                dimensions[0] = kept;
                if name == "array_sample" {
                    lower_bounds[0] = 1;
                }
                let dimension_count = shape.dimension_count();
                Ok(Datum::Array {
                    element,
                    raw: array::build_shaped(
                        output,
                        array::Shape::new(
                            &dimensions[..dimension_count],
                            &lower_bounds[..dimension_count],
                        )?,
                        arena,
                    )?,
                })
            }
            "array_fill" => {
                if args.len() != 2 && args.len() != 3 || star {
                    return Err(arity_err(name, args.len()));
                }
                // The dimension and bound inputs are declared integer[]; an
                // untyped literal is read as that type.
                let integer_array = |index: usize| -> Result<Datum<'a>, SqlError> {
                    let value = eval_full(args[index], arena, params, row, hooks)?;
                    if !value.is_null()
                        && super::super::expression_type_identity(args[index], row, hooks)?
                            .routine_argument_oid()
                            == crate::sql::types::oid::UNKNOWN
                    {
                        cast_to(value, ColType::Array(ArrElem::Int4), arena)
                    } else {
                        Ok(value)
                    }
                };
                let value = eval_full(args[0], arena, params, row, hooks)?;
                let dims = integer_array(1)?;
                if dims.is_null() {
                    return Err(sql_err!(
                        sqlstate::NULL_VALUE_NOT_ALLOWED,
                        "dimension array or low bound array cannot be null"
                    ));
                }
                let Datum::Array { element, raw } = dims else {
                    return Err(crate::sql::eval::function_argument_mismatch(name, &dims));
                };
                let dims_shape = array::shape(raw).expect("array datum invariant");
                if dims_shape.dimension_count() > 1 || element != ArrElem::Int4 {
                    return Err(crate::sql::eval::function_argument_mismatch(
                        "array_fill",
                        &dims,
                    ));
                }
                let dimension_count = array::len(raw);
                if dimension_count > array::MAX_DIMENSIONS {
                    return Err(sql_err!(
                        sqlstate::PROGRAM_LIMIT_EXCEEDED,
                        "array has too many dimensions"
                    ));
                }
                let mut dimensions = [0usize; array::MAX_DIMENSIONS];
                for (index, destination) in dimensions.iter_mut().take(dimension_count).enumerate()
                {
                    let Some(Datum::Int4(value)) = array::get(raw, element, index) else {
                        return Err(sql_err!(
                            sqlstate::NULL_VALUE_NOT_ALLOWED,
                            "dimension values cannot be null"
                        ));
                    };
                    if value < 0 {
                        return Err(sql_err!(
                            sqlstate::ARRAY_SUBSCRIPT_ERROR,
                            "array size exceeds the maximum allowed"
                        ));
                    }
                    *destination = value as usize;
                }
                let mut lower_bounds = [1i32; array::MAX_DIMENSIONS];
                if args.len() == 3 {
                    let lows = integer_array(2)?;
                    let Datum::Array {
                        element: low_element,
                        raw: low_raw,
                    } = lows
                    else {
                        return Err(crate::sql::eval::function_argument_mismatch(
                            "array_fill",
                            &lows,
                        ));
                    };
                    if low_element != ArrElem::Int4 || array::len(low_raw) != dimension_count {
                        return Err(sql_err!(
                            sqlstate::ARRAY_SUBSCRIPT_ERROR,
                            "dimension array and lower bound array must have the same length"
                        ));
                    }
                    for (index, destination) in
                        lower_bounds.iter_mut().take(dimension_count).enumerate()
                    {
                        let Some(Datum::Int4(value)) = array::get(low_raw, low_element, index)
                        else {
                            return Err(sql_err!(
                                sqlstate::NULL_VALUE_NOT_ALLOWED,
                                "low bound values cannot be null"
                            ));
                        };
                        *destination = value;
                    }
                }
                let shape = array::Shape::new(
                    &dimensions[..dimension_count],
                    &lower_bounds[..dimension_count],
                )?;
                let elem = ArrElem::from_datum(&value).unwrap_or(ArrElem::Int4);
                let value = durable_element(value, elem, hooks.catalog, arena)?;
                let filled = arena
                    .alloc_slice_with(shape.element_count(), |_| value)
                    .map_err(|_| arena_full())?;
                Ok(Datum::Array {
                    element: elem,
                    raw: array::build_shaped(filled, shape, arena)?,
                })
            }
            "string_to_array" => {
                // (string, delimiter [, null_string]) -> text[]. A NULL delimiter
                // splits into individual characters; elements equal to null_string
                // become NULL.
                if !(2..=3).contains(&args.len()) {
                    return Err(arity_err(name, args.len()));
                }
                let Some(s) = text_arg(name, args, 0, arena, params, row, hooks)? else {
                    return Ok(Datum::Null);
                };
                let delim = text_arg(name, args, 1, arena, params, row, hooks)?;
                let null_str = if args.len() == 3 {
                    text_arg(name, args, 2, arena, params, row, hooks)?
                } else {
                    None
                };
                let count = super::super::count_split_pieces(s, delim)?;
                let items = array::alloc_items(arena, count)?;
                super::super::for_each_split_piece(s, delim, |piece, index| {
                    items[index] = if null_str == Some(piece) {
                        Datum::Null
                    } else {
                        Datum::Text(piece)
                    };
                    Ok(())
                })?;
                Ok(Datum::Array {
                    element: ArrElem::Text,
                    raw: array::build(items, arena)?,
                })
            }
            _ => unreachable!("dispatch guard admitted an unhandled name"),
        }
    })())
}

/// `array_position` and `array_positions` need the element equality only to
/// search a non-empty array for a non-NULL value, as in PostgreSQL.
fn require_search_equality(
    element: ArrElem,
    dimensions: usize,
    target: &Datum<'_>,
) -> Result<(), SqlError> {
    if dimensions > 0 && !target.is_null() && !element.to_coltype().has_default_equality() {
        return Err(crate::sql::eval::operators::could_not_identify_equality(
            element.to_coltype(),
        ));
    }
    Ok(())
}

fn sample_size_error(count: usize) -> SqlError {
    sql_err!(
        sqlstate::INVALID_PARAMETER_VALUE,
        "sample size must be between 0 and {}",
        count
    )
}

/// Element equality for the array search and replace functions. Stored
/// named-composite elements compare by their catalog fields; text compares
/// bytewise, which agrees with equality under every deterministic collation.
fn search_equal(
    element: &Datum<'_>,
    target: &Datum<'_>,
    catalog: Option<&dyn crate::sql::eval::CatalogAccess>,
) -> Result<bool, SqlError> {
    Ok(super::super::compare_datums_with_catalog(
        crate::sql::ast::Collation::C,
        catalog,
        element,
        target,
    )?
    .is_eq())
}

/// A value entering a named-composite array takes the elements' durable
/// form; every other element is stored as it is.
fn durable_element<'a>(
    value: Datum<'a>,
    element: ArrElem,
    catalog: Option<&dyn crate::sql::eval::CatalogAccess>,
    arena: &'a crate::mem::arena::Arena,
) -> Result<Datum<'a>, SqlError> {
    match (element, value) {
        (ArrElem::Composite(Some(slot)), Datum::Composite { .. } | Datum::Record(_)) => catalog
            .ok_or_else(|| {
                sql_err!(
                    sqlstate::FEATURE_NOT_SUPPORTED,
                    "named composite catalog access is unavailable"
                )
            })?
            .composite_array_element(value, slot, arena),
        _ => Ok(value),
    }
}
