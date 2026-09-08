//! JSON / JSONB scalar built-ins.
//!
//! Covers construction (`array_to_json`, `row_to_json`/`to_json`/`to_jsonb`,
//! `json_build_object`/`json_build_array` and their jsonb forms), inspection
//! (`json_array_length`, `json_typeof`, `json_extract_path`/`_text`), and
//! mutation (`jsonb_set`/`jsonb_set_lax`, `jsonb_insert`, `jsonb_strip_nulls`,
//! `jsonb_pretty`). The set-returning `json_each` / `jsonb_object_keys` are
//! expanded by the set-returning-function machinery and stay in the router.

use core::fmt::Write;

use crate::sql::ast::Expr;
use crate::sql::json;
use crate::sql::types::Datum;
use crate::sql_err;

use super::super::{
    ColumnLookup, EvalHooks, SqlError, arena_full, arity_err, eval_full, json_path_parts,
    json_to_text, json_tree_arg, sqlstate, text_view, type_mismatch,
};

/// Validates the common `jsonb_path_*` arguments and executes the path.  A
/// `None` result represents SQL NULL from any strict argument.
pub(crate) fn path_query_outcome<'a>(
    target: Datum<'a>,
    path: Datum<'a>,
    variables: Option<Datum<'a>>,
    silent: Option<Datum<'a>>,
    use_timezone: bool,
    arena: &'a crate::mem::arena::Arena,
) -> Result<Option<crate::sql::jsonpath::QueryOutcome<'a>>, SqlError> {
    if target.is_null() || path.is_null() {
        return Ok(None);
    }
    let target = match target {
        Datum::Json { text, jsonb: true } => text,
        Datum::Text(text) => match crate::sql::eval::cast_to(
            Datum::Text(text),
            crate::sql::types::ColType::Jsonb,
            arena,
        )? {
            Datum::Json { text, .. } => text,
            _ => unreachable!(),
        },
        other => return Err(type_mismatch("target must be jsonb", &other)),
    };
    let path = match path {
        Datum::JsonPath(text) => text,
        Datum::Text(text) => crate::sql::jsonpath::canonicalize(text, arena)?,
        other => return Err(type_mismatch("path must be jsonpath", &other)),
    };
    let variables = match variables {
        Some(Datum::Json { text, jsonb: true }) => Some(text),
        Some(Datum::Text(text)) => match crate::sql::eval::cast_to(
            Datum::Text(text),
            crate::sql::types::ColType::Jsonb,
            arena,
        )? {
            Datum::Json { text, .. } => Some(text),
            _ => unreachable!(),
        },
        Some(Datum::Null) => return Ok(None),
        Some(other) => return Err(type_mismatch("vars must be jsonb", &other)),
        None => None,
    };
    let silent = match silent {
        Some(Datum::Bool(value)) => value,
        Some(value @ Datum::Text(_)) => {
            match crate::sql::eval::cast_to(value, crate::sql::types::ColType::Bool, arena)? {
                Datum::Bool(value) => value,
                _ => unreachable!(),
            }
        }
        Some(Datum::Null) => return Ok(None),
        Some(other) => return Err(type_mismatch("silent must be boolean", &other)),
        None => false,
    };
    crate::sql::jsonpath::query_outcome(target, path, variables, silent, use_timezone, arena)
        .map(Some)
}

pub(crate) fn path_query_values<'a>(
    target: Datum<'a>,
    path: Datum<'a>,
    variables: Option<Datum<'a>>,
    silent: Option<Datum<'a>>,
    use_timezone: bool,
    arena: &'a crate::mem::arena::Arena,
) -> Result<Option<&'a [json::Json<'a>]>, SqlError> {
    path_query_outcome(target, path, variables, silent, use_timezone, arena)
        .map(|outcome| outcome.map(|outcome| outcome.values))
}

fn sql_json_result<'a>(
    text: &str,
    name: &str,
    arena: &'a crate::mem::arena::Arena,
) -> Result<Datum<'a>, SqlError> {
    let text = arena.alloc_str(text).map_err(|_| arena_full())?;
    if name.ends_with("_jsonb") {
        let value = json::parse(text, arena)?;
        return Ok(Datum::Json {
            text: json_to_text(&value, arena)?,
            jsonb: true,
        });
    }
    if name.ends_with("_text") {
        Ok(Datum::Text(text))
    } else if name.ends_with("_bytea") {
        Ok(Datum::Bytea(text.as_bytes()))
    } else {
        Ok(Datum::Json { text, jsonb: false })
    }
}

pub(crate) fn sql_json_passing<'a>(
    name: &str,
    args: &[&'a Expr<'a>],
    start: usize,
    arena: &'a crate::mem::arena::Arena,
    params: &[Datum<'a>],
    row: &impl ColumnLookup<'a>,
    hooks: &EvalHooks<'_, 'a>,
) -> Result<Option<&'a str>, SqlError> {
    if args.len() == start {
        return Ok(None);
    }
    if !(args.len() - start).is_multiple_of(2) {
        return Err(arity_err(name, args.len()));
    }
    let mut names = [""; 511];
    let mut name_count = 0usize;
    let mut buffer = crate::util::StackStr::<65536>::new();
    buffer.write_char('{').map_err(|_| arena_full())?;
    for pair in args[start..].as_chunks::<2>().0 {
        let variable = match *pair[1] {
            Expr::Str(variable) => variable,
            _ => unreachable!("parser supplies a JSON path variable name"),
        };
        if names[..name_count].contains(&variable) {
            return Err(sql_err!(
                sqlstate::DUPLICATE_ALIAS,
                "duplicate JSON path variable name \"{}\"",
                variable
            ));
        }
        names[name_count] = variable;
        name_count += 1;
        if name_count > 1 {
            buffer.write_str(", ").map_err(|_| arena_full())?;
        }
        json::write_json_raw_string(variable, &mut buffer).map_err(|_| arena_full())?;
        buffer.write_str(": ").map_err(|_| arena_full())?;
        let value = eval_full(pair[0], arena, params, row, hooks)?;
        if value.is_null() {
            buffer.write_str("null").map_err(|_| arena_full())?;
        } else {
            json::write_datum_json(&value, false, &mut buffer).map_err(|_| arena_full())?;
        }
    }
    buffer.write_char('}').map_err(|_| arena_full())?;
    if buffer.is_truncated() {
        return Err(sql_err!(
            sqlstate::PROGRAM_LIMIT_EXCEEDED,
            "JSON passing variables exceed the supported size"
        ));
    }
    arena
        .alloc_str(buffer.as_str())
        .map(Some)
        .map_err(|_| arena_full())
}

pub(crate) fn sql_json_cast<'a>(
    value: Datum<'a>,
    type_name: &str,
    type_mod: i32,
    arena: &'a crate::mem::arena::Arena,
    hooks: &EvalHooks<'_, 'a>,
) -> Result<Datum<'a>, SqlError> {
    if value.is_null() {
        return Ok(Datum::Null);
    }
    if let Some(target) = crate::sql::types::ColType::from_sql_name(type_name) {
        let value = crate::sql::eval::cast_to(value, target, arena)?;
        return if type_mod == -1 {
            Ok(value)
        } else {
            crate::sql::exec::apply_cast_typmod(value, target, type_mod, arena)
        };
    }
    if let Some(catalog) = hooks.catalog {
        if let Datum::Json { text, .. } = value
            && let Some(type_oid) = catalog.user_type_oid(type_name)
            && matches!(
                crate::sql::types::ColType::from_oid(type_oid),
                Some(crate::sql::types::ColType::Composite(_))
                    | Some(crate::sql::types::ColType::Array(
                        crate::sql::types::ArrElem::Composite(_)
                    ))
            )
        {
            return json_value_to_type(
                json::parse(text, arena)?,
                type_oid,
                type_mod,
                Datum::Null,
                arena,
                hooks,
            );
        }
        if let Some(value) = catalog.cast_user_type(type_name, value, arena)? {
            return Ok(value);
        }
    }
    Err(sql_err!(
        sqlstate::UNDEFINED_OBJECT,
        "type \"{}\" does not exist",
        type_name
    ))
}

#[allow(clippy::too_many_arguments)]
fn sql_json_behavior<'a>(
    code: &str,
    default: &'a Expr<'a>,
    original: SqlError,
    type_name: &str,
    type_mod: i32,
    arena: &'a crate::mem::arena::Arena,
    params: &[Datum<'a>],
    row: &impl ColumnLookup<'a>,
    hooks: &EvalHooks<'_, 'a>,
) -> Result<Datum<'a>, SqlError> {
    match code {
        "null" => Ok(Datum::Null),
        "default" => sql_json_cast(
            eval_full(default, arena, params, row, hooks)?,
            type_name,
            type_mod,
            arena,
            hooks,
        ),
        _ => Err(original),
    }
}

fn sql_json_query_type(name: &str) -> &'static str {
    if name.ends_with("_jsonb") {
        "jsonb"
    } else if name.ends_with("_json") {
        "json"
    } else if name.ends_with("_bytea") {
        "bytea"
    } else {
        "text"
    }
}

#[allow(clippy::too_many_arguments)]
fn sql_json_query_behavior<'a>(
    code: &str,
    default: &'a Expr<'a>,
    original: SqlError,
    constructor: &str,
    arena: &'a crate::mem::arena::Arena,
    params: &[Datum<'a>],
    row: &impl ColumnLookup<'a>,
    hooks: &EvalHooks<'_, 'a>,
) -> Result<Datum<'a>, SqlError> {
    match code {
        "null" => Ok(Datum::Null),
        "array" => sql_json_result("[]", constructor, arena),
        "object" => sql_json_result("{}", constructor, arena),
        "default" => sql_json_cast(
            eval_full(default, arena, params, row, hooks)?,
            sql_json_query_type(constructor),
            -1,
            arena,
            hooks,
        ),
        _ => Err(original),
    }
}

pub(crate) fn populate_record<'a>(
    base_expression: &'a Expr<'a>,
    json_expression: &'a Expr<'a>,
    arena: &'a crate::mem::arena::Arena,
    params: &[Datum<'a>],
    row: &impl ColumnLookup<'a>,
    hooks: &EvalHooks<'_, 'a>,
) -> Result<Datum<'a>, SqlError> {
    let base = eval_full(base_expression, arena, params, row, hooks)?;
    let source = eval_full(json_expression, arena, params, row, hooks)?;
    let text = match source {
        Datum::Json { text, .. } | Datum::Text(text) => text,
        Datum::Null => return Ok(Datum::Null),
        other => return Err(type_mismatch("populate_record requires JSON", &other)),
    };
    let source = json::parse(text, arena)?;
    populate_record_from_json(base_expression, base, source, arena, row, hooks)
}

/// Populates one record from already-evaluated arguments. Table and project-set
/// execution use this boundary so a volatile base expression and the JSON
/// document are each evaluated once, even when the document yields many rows.
pub(crate) fn populate_record_from_json<'a>(
    base_expression: &'a Expr<'a>,
    base: Datum<'a>,
    source: json::Json<'a>,
    arena: &'a crate::mem::arena::Arena,
    row: &impl ColumnLookup<'a>,
    hooks: &EvalHooks<'_, 'a>,
) -> Result<Datum<'a>, SqlError> {
    let catalog = hooks.catalog.ok_or_else(|| {
        sql_err!(
            sqlstate::FEATURE_NOT_SUPPORTED,
            "named composite catalog access is unavailable"
        )
    })?;
    let declared_type_oid =
        match super::super::expression_type_identity(base_expression, row, hooks)? {
            super::super::ExpressionTypeIdentity::Known(type_oid) => Some(type_oid),
            super::super::ExpressionTypeIdentity::Unresolved => None,
        };
    let (slot, base_fields) = match base {
        Datum::Composite { slot, fields } => (slot, fields),
        Datum::CompositeText {
            slot,
            physical_fields,
            text,
        } => match catalog.materialize_composite(slot, physical_fields, text, arena)? {
            Datum::Composite { fields, .. } => (slot, fields),
            _ => unreachable!("catalog materializes a named composite"),
        },
        Datum::Null => {
            let Some(type_oid) = declared_type_oid else {
                return Err(sql_err!(
                    sqlstate::DATATYPE_MISMATCH,
                    "record type has not been registered"
                ));
            };
            let composite_oid = catalog.composite_base_type_oid(type_oid).ok_or_else(|| {
                sql_err!(
                    sqlstate::DATATYPE_MISMATCH,
                    "first argument of populate_record must be a row type"
                )
            })?;
            let fields = catalog
                .null_composite_fields(composite_oid, arena)?
                .ok_or_else(|| {
                    sql_err!(
                        sqlstate::DATATYPE_MISMATCH,
                        "first argument of populate_record must be a row type"
                    )
                })?;
            let slot = u16::try_from(composite_oid - crate::sql::types::oid::FIRST_COMPOSITE)
                .map_err(|_| {
                    sql_err!(
                        sqlstate::DATATYPE_MISMATCH,
                        "first argument of populate_record must be a row type"
                    )
                })?;
            (slot, fields)
        }
        other => {
            return Err(type_mismatch(
                "first argument of populate_record must be a row type",
                &other,
            ));
        }
    };
    let json::Json::Object(members) = source else {
        return Err(sql_err!(
            sqlstate::INVALID_PARAMETER_VALUE,
            "cannot call populate_composite on a scalar"
        ));
    };
    let fields = arena
        .alloc_slice_copy(base_fields)
        .map_err(|_| arena_full())?;
    let composite_oid = crate::sql::types::oid::composite_oid(slot);
    for (index, field) in fields.iter_mut().enumerate() {
        if let Some((_, value)) = members.iter().find(|(name, _)| *name == field.name) {
            field.value = json_value_to_type(
                *value,
                field.type_oid,
                catalog
                    .composite_field_type_mod(composite_oid, index)
                    .unwrap_or(-1),
                field.value,
                arena,
                hooks,
            )?;
        }
    }
    let result = Datum::Composite { slot, fields };
    let composite_oid = crate::sql::types::oid::composite_oid(slot);
    let Some(declared_type_oid) = declared_type_oid.filter(|oid| *oid != composite_oid) else {
        return Ok(result);
    };
    let type_name = catalog
        .type_name(declared_type_oid, arena)?
        .ok_or_else(|| sql_err!(sqlstate::UNDEFINED_OBJECT, "domain type does not exist"))?;
    catalog
        .cast_user_type(type_name, result, arena)?
        .ok_or_else(|| sql_err!(sqlstate::UNDEFINED_OBJECT, "domain type does not exist"))
}

fn validate_json_populate_option<'a>(
    name: &str,
    args: &[&'a Expr<'a>],
    arena: &'a crate::mem::arena::Arena,
    params: &[Datum<'a>],
    row: &impl ColumnLookup<'a>,
    hooks: &EvalHooks<'_, 'a>,
) -> Result<(), SqlError> {
    let valid_arity = if name.starts_with("jsonb_") {
        args.len() == 2
    } else {
        matches!(args.len(), 2 | 3)
    };
    if !valid_arity {
        return Err(arity_err(name, args.len()));
    }
    if let Some(option) = args.get(2) {
        crate::sql::eval::cast_to(
            eval_full(option, arena, params, row, hooks)?,
            crate::sql::types::ColType::Bool,
            arena,
        )?;
    }
    Ok(())
}

pub(crate) fn json_value_to_type<'a>(
    value: json::Json<'a>,
    type_oid: i32,
    type_mod: i32,
    base: Datum<'a>,
    arena: &'a crate::mem::arena::Arena,
    hooks: &EvalHooks<'_, 'a>,
) -> Result<Datum<'a>, SqlError> {
    if matches!(value, json::Json::Null) {
        return Ok(Datum::Null);
    }
    let catalog = hooks.catalog.ok_or_else(|| {
        sql_err!(
            sqlstate::FEATURE_NOT_SUPPORTED,
            "JSON conversion requires catalog access"
        )
    })?;
    if let Some(crate::sql::types::ColType::Composite(slot)) =
        crate::sql::types::ColType::from_oid(type_oid)
    {
        let json::Json::Object(members) = value else {
            return Err(sql_err!(
                sqlstate::INVALID_PARAMETER_VALUE,
                "cannot populate composite from a non-object"
            ));
        };
        let base_fields = match base {
            Datum::Composite {
                slot: base_slot,
                fields,
            } if base_slot == slot => fields,
            Datum::CompositeText {
                slot: base_slot,
                physical_fields,
                text,
            } if base_slot == slot => {
                match catalog.materialize_composite(base_slot, physical_fields, text, arena)? {
                    Datum::Composite { fields, .. } => fields,
                    _ => unreachable!("catalog materializes a named composite"),
                }
            }
            _ => catalog
                .null_composite_fields(type_oid, arena)?
                .ok_or_else(|| {
                    sql_err!(sqlstate::DATATYPE_MISMATCH, "composite type is not visible")
                })?,
        };
        let fields = arena
            .alloc_slice_copy(base_fields)
            .map_err(|_| arena_full())?;
        for (index, field) in fields.iter_mut().enumerate() {
            if let Some((_, child)) = members.iter().find(|(name, _)| *name == field.name) {
                field.value = json_value_to_type(
                    *child,
                    field.type_oid,
                    catalog
                        .composite_field_type_mod(type_oid, index)
                        .unwrap_or(-1),
                    field.value,
                    arena,
                    hooks,
                )?;
            }
        }
        return Ok(Datum::Composite { slot, fields });
    }
    if let Some(crate::sql::types::ColType::Array(element)) =
        crate::sql::types::ColType::from_oid(type_oid)
    {
        return json_value_to_array(value, element, arena, hooks);
    }
    let input = match value {
        json::Json::Str(text) => Datum::Text(text),
        json::Json::Bool(value) => Datum::Text(if value { "true" } else { "false" }),
        json::Json::Number(text) => Datum::Text(text),
        json::Json::Temporal { text, .. } => Datum::Text(text),
        value @ (json::Json::Array(_) | json::Json::Object(_)) => {
            Datum::Text(super::super::json_to_text_pub(&value, arena)?)
        }
        json::Json::Null => unreachable!(),
    };
    let mut converted = if type_oid == crate::sql::types::oid::JSON {
        let Datum::Text(text) = input else {
            unreachable!()
        };
        Datum::Json { text, jsonb: false }
    } else if type_oid == crate::sql::types::oid::JSONB {
        let Datum::Text(text) = input else {
            unreachable!()
        };
        Datum::Json {
            text: super::super::json_to_text_pub(&json::parse(text, arena)?, arena)?,
            jsonb: true,
        }
    } else if matches!(
        crate::sql::types::ColType::from_oid(type_oid),
        Some(crate::sql::types::ColType::Enum(_))
    ) {
        let type_name = catalog.type_name(type_oid, arena)?.ok_or_else(|| {
            sql_err!(
                sqlstate::UNDEFINED_OBJECT,
                "type with OID {} does not exist",
                type_oid
            )
        })?;
        catalog
            .cast_user_type(type_name, input, arena)?
            .ok_or_else(|| sql_err!(sqlstate::UNDEFINED_OBJECT, "type does not exist"))?
    } else if let Some(ctype) = crate::sql::types::ColType::from_oid(type_oid) {
        super::super::cast_to(input, ctype, arena)?
    } else {
        let type_name = catalog.type_name(type_oid, arena)?.ok_or_else(|| {
            sql_err!(
                sqlstate::UNDEFINED_OBJECT,
                "type with OID {} does not exist",
                type_oid
            )
        })?;
        catalog
            .cast_user_type(type_name, input, arena)?
            .ok_or_else(|| sql_err!(sqlstate::UNDEFINED_OBJECT, "type does not exist"))?
    };
    if type_mod != -1
        && let Some(ctype) = crate::sql::types::ColType::from_oid(type_oid)
    {
        converted = crate::sql::exec::apply_typmod(converted, ctype, type_mod, arena)?;
    }
    Ok(converted)
}

fn json_value_to_array<'a>(
    value: json::Json<'a>,
    element: crate::sql::types::ArrElem,
    arena: &'a crate::mem::arena::Arena,
    hooks: &EvalHooks<'_, 'a>,
) -> Result<Datum<'a>, SqlError> {
    let json::Json::Array(items) = value else {
        return Err(sql_err!(
            sqlstate::INVALID_PARAMETER_VALUE,
            "expected JSON array"
        ));
    };
    let converted = arena
        .alloc_slice_with(items.len(), |_| Datum::Null)
        .map_err(|_| arena_full())?;
    let nested = items
        .iter()
        .find(|item| !matches!(item, json::Json::Null))
        .is_some_and(|item| matches!(item, json::Json::Array(_)));
    for (slot, item) in converted.iter_mut().zip(items) {
        let value = if nested {
            if matches!(item, json::Json::Null) {
                return Err(sql_err!(
                    sqlstate::NULL_VALUE_NOT_ALLOWED,
                    "multidimensional arrays must have matching dimensions"
                ));
            }
            json_value_to_array(*item, element, arena, hooks)?
        } else {
            json_value_to_type(*item, element.element_oid(), -1, Datum::Null, arena, hooks)?
        };
        *slot = match (element, value) {
            (crate::sql::types::ArrElem::Composite(composite_slot), value) if !value.is_null() => {
                hooks
                    .catalog
                    .ok_or_else(|| {
                        sql_err!(
                            sqlstate::FEATURE_NOT_SUPPORTED,
                            "JSON conversion requires catalog access"
                        )
                    })?
                    .composite_array_element(value, composite_slot, arena)?
            }
            (_, value) => value,
        };
    }
    if nested {
        crate::sql::array::stack(converted, arena)
    } else {
        Ok(Datum::Array {
            element,
            raw: crate::sql::array::build(converted, arena)?,
        })
    }
}

/// Handles the JSON/JSONB scalar family. Returns `None` if `name` is not one of
/// these functions, leaving the router to keep matching.
#[allow(clippy::too_many_arguments)]
pub(crate) fn dispatch<'a>(
    name: &str,
    args: &[&'a Expr<'a>],
    star: bool,
    arena: &'a crate::mem::arena::Arena,
    params: &[Datum<'a>],
    row: &impl ColumnLookup<'a>,
    hooks: &EvalHooks<'_, 'a>,
) -> Option<Result<Datum<'a>, SqlError>> {
    if !name.starts_with("__is_json_")
        && !matches!(name, "__json" | "__json_unique")
        && !name.starts_with("__json_array_")
        && !name.starts_with("__json_object_")
        && !name.starts_with("__json_serialize_")
        && !name.starts_with("__json_exists_")
        && name != "__json_value"
        && !name.starts_with("__json_query_")
        && name != "__jsonb_subscript_set"
        && name != "__json_format"
        && !matches!(
            name,
            "array_to_json"
                | "jsonb_array_length"
                | "json_array_length"
                | "jsonb_typeof"
                | "json_typeof"
                | "json_extract_path"
                | "jsonb_extract_path"
                | "json_extract_path_text"
                | "jsonb_extract_path_text"
                | "row_to_json"
                | "to_json"
                | "to_jsonb"
                | "jsonb_set"
                | "jsonb_set_lax"
                | "jsonb_insert"
                | "jsonb_strip_nulls"
                | "json_strip_nulls"
                | "jsonb_pretty"
                | "json_build_object"
                | "jsonb_build_object"
                | "json_build_array"
                | "jsonb_build_array"
                | "json_scalar"
                | "jsonb_path_exists"
                | "jsonb_path_exists_tz"
                | "jsonb_path_match"
                | "jsonb_path_match_tz"
                | "jsonb_path_query_array"
                | "jsonb_path_query_array_tz"
                | "jsonb_path_query_first"
                | "jsonb_path_query_first_tz"
                | "json_populate_record"
                | "jsonb_populate_record"
                | "jsonb_populate_record_valid"
                | "json_populate_recordset"
                | "jsonb_populate_recordset"
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
            "json_populate_recordset" | "jsonb_populate_recordset" => {
                validate_json_populate_option(name, args, arena, params, row, hooks)?;
                let source = eval_full(args[1], arena, params, row, hooks)?;
                let text = match source {
                    Datum::Json { text, .. } | Datum::Text(text) => text,
                    Datum::Null => return Ok(Datum::Null),
                    other => return Err(type_mismatch("populate_recordset requires JSON", &other)),
                };
                let json::Json::Array(items) = json::parse(text, arena)? else {
                    return Err(sql_err!(
                        sqlstate::INVALID_PARAMETER_VALUE,
                        "cannot call populate_recordset on a non-array"
                    ));
                };
                let index = hooks.srf_index.ok_or_else(|| {
                    sql_err!(
                        sqlstate::FEATURE_NOT_SUPPORTED,
                        "set-returning function called where not allowed"
                    )
                })?;
                let Some(item) = items.get(index - 1) else {
                    return Ok(Datum::Null);
                };
                let text = super::super::json_to_text_pub(item, arena)?;
                let source = arena.alloc(Expr::Str(text)).map_err(|_| arena_full())?;
                populate_record(args[0], source, arena, params, row, hooks)
            }
            "json_populate_record" | "jsonb_populate_record" | "jsonb_populate_record_valid" => {
                validate_json_populate_option(name, args, arena, params, row, hooks)?;
                let result = populate_record(args[0], args[1], arena, params, row, hooks);
                if name == "jsonb_populate_record_valid" {
                    return match result {
                        Ok(Datum::Null) => Ok(Datum::Null),
                        Ok(_) => Ok(Datum::Bool(true)),
                        Err(error)
                            if error.sqlstate == sqlstate::FEATURE_NOT_SUPPORTED
                                || error.sqlstate == sqlstate::DATATYPE_MISMATCH =>
                        {
                            Err(error)
                        }
                        Err(_) => Ok(Datum::Bool(false)),
                    };
                }
                result
            }
            "__jsonb_subscript_set" => {
                if args.len() < 3 || star {
                    return Err(arity_err("jsonb subscripting assignment", args.len()));
                }
                let base = eval_full(args[0], arena, params, row, hooks)?;
                if let Datum::Geometry { kind, text } = base {
                    if args.len() != 3 {
                        return Err(arity_err("geometric subscripting assignment", args.len()));
                    }
                    let index = match eval_full(args[2], arena, params, row, hooks)? {
                        Datum::Int2(index) => i64::from(index),
                        Datum::Int4(index) => i64::from(index),
                        Datum::Int8(index) => index,
                        Datum::Null => {
                            return Err(sql_err!(
                                sqlstate::NULL_VALUE_NOT_ALLOWED,
                                "geometric subscript in assignment must not be null"
                            ));
                        }
                        other => {
                            return Err(type_mismatch(
                                "geometric subscript must be integer",
                                &other,
                            ));
                        }
                    };
                    let value = eval_full(args[1], arena, params, row, hooks)?;
                    return super::geometry::set_subscript(kind, text, index, value, arena);
                }
                let mut path = [json::JsonSubscript::Key(""); 64];
                if args.len() - 2 > path.len() {
                    return Err(sql_err!(
                        sqlstate::PROGRAM_LIMIT_EXCEEDED,
                        "too many jsonb subscripts"
                    ));
                }
                for (slot, expression) in path.iter_mut().zip(&args[2..]) {
                    *slot = match eval_full(expression, arena, params, row, hooks)? {
                        Datum::Text(key) | Datum::Bpchar(key) => json::JsonSubscript::Key(key),
                        Datum::Int2(index) => json::JsonSubscript::Index(i64::from(index)),
                        Datum::Int4(index) => json::JsonSubscript::Index(i64::from(index)),
                        Datum::Int8(index) => json::JsonSubscript::Index(index),
                        Datum::Null => {
                            return Err(sql_err!(
                                sqlstate::NULL_VALUE_NOT_ALLOWED,
                                "jsonb subscript in assignment must not be null"
                            ));
                        }
                        other => return Err(type_mismatch("invalid jsonb subscript", &other)),
                    };
                }
                let value = match eval_full(args[1], arena, params, row, hooks)? {
                    Datum::Json { text, .. } | Datum::Text(text) => json::parse(text, arena)?,
                    Datum::Null => json::Json::Null,
                    other => return Err(type_mismatch("jsonb assignment requires jsonb", &other)),
                };
                let root = match base {
                    Datum::Json { text, jsonb: true } => json::parse(text, arena)?,
                    Datum::Null => json::Json::Null,
                    other => {
                        return Err(type_mismatch("cannot subscript a non-jsonb value", &other));
                    }
                };
                let result = json::set_subscript(root, &path[..args.len() - 2], value, arena)?;
                Ok(Datum::Json {
                    text: json_to_text(&result, arena)?,
                    jsonb: true,
                })
            }
            constructor if constructor.starts_with("__json_query_") => {
                if args.len() < 8 || !(args.len() - 8).is_multiple_of(2) || star {
                    return Err(arity_err("json_query", args.len()));
                }
                let tail = args.len() - 6;
                let (empty_default, error_default) = (args[tail], args[tail + 1]);
                let marker = |index: usize| match *args[index] {
                    Expr::Str(value) => value,
                    _ => unreachable!("parser supplies SQL/JSON option markers"),
                };
                let empty_code = marker(tail + 2);
                let error_code = marker(tail + 3);
                let wrapper = marker(tail + 4);
                let quotes = marker(tail + 5);
                let target = eval_full(args[0], arena, params, row, hooks)?;
                let path = eval_full(args[1], arena, params, row, hooks)?;
                let variables =
                    sql_json_passing("json_query", &args[..tail], 2, arena, params, row, hooks)?;
                if target.is_null() || path.is_null() {
                    return Ok(Datum::Null);
                }
                let target = match target {
                    Datum::Json { text, .. } | Datum::Text(text) => text,
                    other => return Err(type_mismatch("JSON_QUERY requires JSON", &other)),
                };
                let path = match path {
                    Datum::JsonPath(text) | Datum::Text(text) => text,
                    other => return Err(type_mismatch("JSON_QUERY path must be text", &other)),
                };
                let values =
                    match crate::sql::jsonpath::query(target, path, variables, false, arena) {
                        Ok(values) => values,
                        Err(error) => {
                            return sql_json_query_behavior(
                                error_code,
                                error_default,
                                error,
                                constructor,
                                arena,
                                params,
                                row,
                                hooks,
                            );
                        }
                    };
                if values.is_empty() {
                    return sql_json_query_behavior(
                        empty_code,
                        empty_default,
                        sql_err!(sqlstate::DATA_EXCEPTION, "no SQL/JSON item"),
                        constructor,
                        arena,
                        params,
                        row,
                        hooks,
                    );
                }
                let wrap = wrapper == "unconditional"
                    || (wrapper == "conditional"
                        && (values.len() != 1
                            || !matches!(values[0], json::Json::Array(_) | json::Json::Object(_))));
                if wrapper == "without" && values.len() != 1 {
                    return sql_json_query_behavior(
                        error_code,
                        error_default,
                        sql_err!(
                            sqlstate::DATA_EXCEPTION,
                            "JSON path expression in JSON_QUERY must return single item when no wrapper is requested"
                        ),
                        constructor,
                        arena,
                        params,
                        row,
                        hooks,
                    );
                }
                if quotes == "omit" && !wrap && matches!(values, [json::Json::Str(_)]) {
                    let json::Json::Str(text) = values[0] else {
                        unreachable!()
                    };
                    return if constructor.ends_with("_text") {
                        Ok(Datum::Text(text))
                    } else if constructor.ends_with("_bytea") {
                        Ok(Datum::Bytea(text.as_bytes()))
                    } else {
                        sql_json_query_behavior(
                            error_code,
                            error_default,
                            sql_err!(
                                sqlstate::DATA_EXCEPTION,
                                "cannot omit quotes from a JSON result type"
                            ),
                            constructor,
                            arena,
                            params,
                            row,
                            hooks,
                        )
                    };
                }
                let mut buffer = crate::util::StackStr::<65536>::new();
                if wrap {
                    buffer.write_char('[').map_err(|_| arena_full())?;
                    for (index, value) in values.iter().enumerate() {
                        if index > 0 {
                            buffer.write_str(", ").map_err(|_| arena_full())?;
                        }
                        value.write(&mut buffer).map_err(|_| arena_full())?;
                    }
                    buffer.write_char(']').map_err(|_| arena_full())?;
                } else {
                    values[0].write(&mut buffer).map_err(|_| arena_full())?;
                }
                if buffer.is_truncated() {
                    return Err(sql_err!(
                        sqlstate::PROGRAM_LIMIT_EXCEEDED,
                        "JSON query result exceeds the supported size"
                    ));
                }
                sql_json_result(buffer.as_str(), constructor, arena)
            }
            "__json_value" => {
                if args.len() < 8 || !(args.len() - 8).is_multiple_of(2) || star {
                    return Err(arity_err("json_value", args.len()));
                }
                let tail = args.len() - 6;
                let (empty_default, error_default) = (args[tail], args[tail + 1]);
                let empty_code = match *args[tail + 2] {
                    Expr::Str(value) => value,
                    _ => unreachable!(),
                };
                let error_code = match *args[tail + 3] {
                    Expr::Str(value) => value,
                    _ => unreachable!(),
                };
                let type_name = match *args[tail + 4] {
                    Expr::Str(value) => value,
                    _ => unreachable!(),
                };
                let type_mod = match *args[tail + 5] {
                    Expr::Int(value) => i32::try_from(value).unwrap_or(-1),
                    _ => unreachable!(),
                };
                let target = eval_full(args[0], arena, params, row, hooks)?;
                let path = eval_full(args[1], arena, params, row, hooks)?;
                let variables =
                    sql_json_passing("json_value", &args[..tail], 2, arena, params, row, hooks)?;
                if target.is_null() || path.is_null() {
                    return Ok(Datum::Null);
                }
                let target = match target {
                    Datum::Json { text, .. } | Datum::Text(text) => text,
                    other => return Err(type_mismatch("JSON_VALUE requires JSON", &other)),
                };
                let path = match path {
                    Datum::JsonPath(text) | Datum::Text(text) => text,
                    other => return Err(type_mismatch("JSON_VALUE path must be text", &other)),
                };
                let values =
                    match crate::sql::jsonpath::query(target, path, variables, false, arena) {
                        Ok(values) => values,
                        Err(error) => {
                            return sql_json_behavior(
                                error_code,
                                error_default,
                                error,
                                type_name,
                                type_mod,
                                arena,
                                params,
                                row,
                                hooks,
                            );
                        }
                    };
                let raw = match values {
                    [] => {
                        return sql_json_behavior(
                            empty_code,
                            empty_default,
                            sql_err!(sqlstate::DATA_EXCEPTION, "no SQL/JSON item"),
                            type_name,
                            type_mod,
                            arena,
                            params,
                            row,
                            hooks,
                        );
                    }
                    [json::Json::Null] => Datum::Null,
                    [json::Json::Str(value)] | [json::Json::Number(value)] => Datum::Text(value),
                    [json::Json::Bool(value)] => Datum::Text(if *value { "true" } else { "false" }),
                    _ => {
                        return sql_json_behavior(
                            error_code,
                            error_default,
                            sql_err!(
                                sqlstate::DATA_EXCEPTION,
                                "JSON path expression in JSON_VALUE must return single scalar item"
                            ),
                            type_name,
                            type_mod,
                            arena,
                            params,
                            row,
                            hooks,
                        );
                    }
                };
                match sql_json_cast(raw, type_name, type_mod, arena, hooks) {
                    Ok(value) => Ok(value),
                    Err(error) => sql_json_behavior(
                        error_code,
                        error_default,
                        error,
                        type_name,
                        type_mod,
                        arena,
                        params,
                        row,
                        hooks,
                    ),
                }
            }
            predicate if predicate.starts_with("__json_exists_") => {
                if args.len() < 2 || !(args.len() - 2).is_multiple_of(2) || star {
                    return Err(arity_err("json_exists", args.len()));
                }
                let target = eval_full(args[0], arena, params, row, hooks)?;
                let path = eval_full(args[1], arena, params, row, hooks)?;
                let variables =
                    sql_json_passing("json_exists", args, 2, arena, params, row, hooks)?;
                if target.is_null() || path.is_null() {
                    return Ok(Datum::Null);
                }
                let target = match target {
                    Datum::Json { text, .. } | Datum::Text(text) => text,
                    other => return Err(type_mismatch("JSON_EXISTS requires JSON", &other)),
                };
                let path = match path {
                    Datum::JsonPath(text) | Datum::Text(text) => text,
                    other => return Err(type_mismatch("JSON_EXISTS path must be text", &other)),
                };
                match crate::sql::jsonpath::query(target, path, variables, false, arena) {
                    Ok(values) => Ok(Datum::Bool(!values.is_empty())),
                    Err(_) if predicate.ends_with("_true") => Ok(Datum::Bool(true)),
                    Err(_) if predicate.ends_with("_unknown") => Ok(Datum::Null),
                    Err(_) if predicate.ends_with("_false") => Ok(Datum::Bool(false)),
                    Err(error) => Err(error),
                }
            }
            "__json_serialize_text" | "__json_serialize_bytea" => {
                arity(1)?;
                let value = eval_full(args[0], arena, params, row, hooks)?;
                let text = match value {
                    Datum::Json { text, .. } | Datum::Text(text) => text,
                    Datum::Bytea(bytes) => core::str::from_utf8(bytes).map_err(|_| {
                        sql_err!(
                            sqlstate::INVALID_TEXT_REPRESENTATION,
                            "invalid JSON encoding"
                        )
                    })?,
                    Datum::Null => return Ok(Datum::Null),
                    other => return Err(type_mismatch("JSON_SERIALIZE requires JSON", &other)),
                };
                json::validate(text, arena)?;
                if name.ends_with("_bytea") {
                    Ok(Datum::Bytea(text.as_bytes()))
                } else {
                    Ok(Datum::Text(text))
                }
            }
            "__json_format" => {
                arity(1)?;
                let value = eval_full(args[0], arena, params, row, hooks)?;
                let text = match value {
                    Datum::Text(text) | Datum::Json { text, .. } => text,
                    Datum::Bytea(bytes) => core::str::from_utf8(bytes).map_err(|_| {
                        sql_err!(
                            sqlstate::INVALID_TEXT_REPRESENTATION,
                            "invalid JSON encoding"
                        )
                    })?,
                    Datum::Null => return Ok(Datum::Null),
                    other => {
                        return Err(type_mismatch(
                            "cannot use non-string types with explicit FORMAT JSON clause",
                            &other,
                        ));
                    }
                };
                json::validate(text, arena)?;
                Ok(Datum::Json { text, jsonb: false })
            }
            constructor if constructor.starts_with("__json_array_") => {
                let absent = constructor.contains("_absent_");
                let mut buffer = crate::util::StackStr::<65536>::new();
                buffer.write_char('[').map_err(|_| arena_full())?;
                let mut emitted = 0usize;
                for argument in args {
                    let value = eval_full(argument, arena, params, row, hooks)?;
                    if absent && value.is_null() {
                        continue;
                    }
                    if emitted > 0 {
                        buffer.write_str(", ").map_err(|_| arena_full())?;
                    }
                    if value.is_null() {
                        buffer.write_str("null").map_err(|_| arena_full())?;
                    } else {
                        json::write_datum_json(&value, false, &mut buffer)
                            .map_err(|_| arena_full())?;
                    }
                    emitted += 1;
                }
                buffer.write_char(']').map_err(|_| arena_full())?;
                if buffer.is_truncated() {
                    return Err(sql_err!(
                        sqlstate::PROGRAM_LIMIT_EXCEEDED,
                        "JSON array result exceeds the supported size"
                    ));
                }
                sql_json_result(buffer.as_str(), constructor, arena)
            }
            constructor if constructor.starts_with("__json_object_") => {
                if args.is_empty() || args.len().is_multiple_of(2) {
                    return Err(arity_err("json_object", args.len()));
                }
                let absent = matches!(*args[args.len() - 1], Expr::Bool(true));
                let unique = constructor.contains("_unique_");
                let mut keys = [""; 512];
                let mut key_count = 0usize;
                let mut buffer = crate::util::StackStr::<65536>::new();
                buffer.write_char('{').map_err(|_| arena_full())?;
                let mut emitted = 0usize;
                for pair in args[..args.len() - 1].as_chunks::<2>().0 {
                    let key_value = eval_full(pair[0], arena, params, row, hooks)?;
                    if key_value.is_null() {
                        return Err(sql_err!(
                            sqlstate::NULL_VALUE_NOT_ALLOWED,
                            "null value not allowed for object key"
                        ));
                    }
                    let key = match crate::sql::eval::cast_to(
                        key_value,
                        crate::sql::types::ColType::Text,
                        arena,
                    )? {
                        Datum::Text(text) => text,
                        _ => unreachable!(),
                    };
                    let value = eval_full(pair[1], arena, params, row, hooks)?;
                    if absent && value.is_null() {
                        continue;
                    }
                    if unique && keys[..key_count].contains(&key) {
                        return Err(sql_err!(
                            sqlstate::DUPLICATE_JSON_OBJECT_KEY_VALUE,
                            "duplicate JSON object key value"
                        ));
                    }
                    if key_count == keys.len() {
                        return Err(sql_err!(
                            sqlstate::PROGRAM_LIMIT_EXCEEDED,
                            "JSON object has too many members"
                        ));
                    }
                    keys[key_count] = key;
                    key_count += 1;
                    if emitted > 0 {
                        buffer.write_str(", ").map_err(|_| arena_full())?;
                    }
                    json::write_json_raw_string(key, &mut buffer).map_err(|_| arena_full())?;
                    buffer.write_str(" : ").map_err(|_| arena_full())?;
                    if value.is_null() {
                        buffer.write_str("null").map_err(|_| arena_full())?;
                    } else {
                        json::write_datum_json(&value, false, &mut buffer)
                            .map_err(|_| arena_full())?;
                    }
                    emitted += 1;
                }
                buffer.write_char('}').map_err(|_| arena_full())?;
                if buffer.is_truncated() {
                    return Err(sql_err!(
                        sqlstate::PROGRAM_LIMIT_EXCEEDED,
                        "JSON object result exceeds the supported size"
                    ));
                }
                sql_json_result(buffer.as_str(), constructor, arena)
            }
            "__json" | "__json_unique" => {
                arity(1)?;
                let value = eval_full(args[0], arena, params, row, hooks)?;
                let text = match value {
                    Datum::Text(text) | Datum::Json { text, .. } => text,
                    Datum::Bytea(bytes) => core::str::from_utf8(bytes).map_err(|_| {
                        sql_err!(
                            sqlstate::INVALID_TEXT_REPRESENTATION,
                            "invalid JSON encoding"
                        )
                    })?,
                    Datum::Null => return Ok(Datum::Null),
                    other => {
                        return Err(type_mismatch("JSON() requires character or bytea", &other));
                    }
                };
                let parsed = json::parse_source_order(text, arena)?;
                if name == "__json_unique" && !json::has_unique_keys(&parsed) {
                    return Err(sql_err!(
                        sqlstate::DUPLICATE_JSON_OBJECT_KEY_VALUE,
                        "duplicate JSON object key value"
                    ));
                }
                Ok(Datum::Json { text, jsonb: false })
            }
            "json_scalar" => {
                arity(1)?;
                let value = eval_full(args[0], arena, params, row, hooks)?;
                if value.is_null() {
                    return Ok(Datum::Null);
                }
                let mut buffer = crate::util::StackStr::<65536>::new();
                json::write_datum_json(&value, false, &mut buffer).map_err(|_| arena_full())?;
                if buffer.is_truncated() {
                    return Err(sql_err!(
                        sqlstate::PROGRAM_LIMIT_EXCEEDED,
                        "JSON scalar result exceeds the supported size"
                    ));
                }
                Ok(Datum::Json {
                    text: arena.alloc_str(buffer.as_str()).map_err(|_| arena_full())?,
                    jsonb: false,
                })
            }
            predicate if predicate.starts_with("__is_json_") => {
                arity(1)?;
                let value = eval_full(args[0], arena, params, row, hooks)?;
                let text = match value {
                    Datum::Text(text) | Datum::Json { text, .. } => text,
                    Datum::Bytea(bytes) => match core::str::from_utf8(bytes) {
                        Ok(text) => text,
                        Err(_) => return Ok(Datum::Bool(false)),
                    },
                    Datum::Null => return Ok(Datum::Null),
                    other => {
                        return Err(type_mismatch("IS JSON requires character or bytea", &other));
                    }
                };
                let parsed = match json::parse_source_order(text, arena) {
                    Ok(value) => value,
                    Err(_) => return Ok(Datum::Bool(false)),
                };
                let kind_matches = if predicate.contains("_scalar") {
                    !matches!(parsed, json::Json::Array(_) | json::Json::Object(_))
                } else if predicate.contains("_array") {
                    matches!(parsed, json::Json::Array(_))
                } else if predicate.contains("_object") {
                    matches!(parsed, json::Json::Object(_))
                } else {
                    true
                };
                let unique = !predicate.ends_with("_unique") || json::has_unique_keys(&parsed);
                Ok(Datum::Bool(kind_matches && unique))
            }
            "jsonb_path_exists"
            | "jsonb_path_exists_tz"
            | "jsonb_path_match"
            | "jsonb_path_match_tz"
            | "jsonb_path_query_array"
            | "jsonb_path_query_array_tz"
            | "jsonb_path_query_first"
            | "jsonb_path_query_first_tz" => {
                if !(2..=4).contains(&args.len()) || star {
                    return Err(arity_err(name, args.len()));
                }
                let target = eval_full(args[0], arena, params, row, hooks)?;
                let path = eval_full(args[1], arena, params, row, hooks)?;
                let variables = if args.len() >= 3 {
                    Some(eval_full(args[2], arena, params, row, hooks)?)
                } else {
                    None
                };
                let silent = if args.len() == 4 {
                    Some(eval_full(args[3], arena, params, row, hooks)?)
                } else {
                    None
                };
                let Some(outcome) = path_query_outcome(
                    target,
                    path,
                    variables,
                    silent,
                    name.ends_with("_tz"),
                    arena,
                )?
                else {
                    return Ok(Datum::Null);
                };
                let values = outcome.values;
                if name.contains("_exists") {
                    if outcome.suppressed_error {
                        return Ok(Datum::Null);
                    }
                    return Ok(Datum::Bool(!values.is_empty()));
                }
                if name.contains("_match") {
                    return match values {
                        [] => Ok(Datum::Null),
                        [json::Json::Bool(value)] => Ok(Datum::Bool(*value)),
                        [json::Json::Null] => Ok(Datum::Null),
                        [_] => Err(sql_err!(
                            sqlstate::INVALID_PARAMETER_VALUE,
                            "single boolean result is expected"
                        )),
                        _ => Err(sql_err!(
                            sqlstate::INVALID_PARAMETER_VALUE,
                            "single boolean result is expected"
                        )),
                    };
                }
                if name.contains("_query_first") {
                    return match values.first() {
                        Some(value) => Ok(Datum::Json {
                            text: json_to_text(value, arena)?,
                            jsonb: true,
                        }),
                        None => Ok(Datum::Null),
                    };
                }
                let mut buffer = crate::util::StackStr::<65536>::new();
                let _ = buffer.write_char('[');
                for (index, value) in values.iter().enumerate() {
                    if index > 0 {
                        let _ = buffer.write_str(", ");
                    }
                    let _ = value.write(&mut buffer);
                }
                let _ = buffer.write_char(']');
                if buffer.is_truncated() {
                    return Err(sql_err!(
                        sqlstate::PROGRAM_LIMIT_EXCEEDED,
                        "JSON path result exceeds the supported size"
                    ));
                }
                Ok(Datum::Json {
                    text: arena.alloc_str(buffer.as_str()).map_err(|_| arena_full())?,
                    jsonb: true,
                })
            }
            "array_to_json" => {
                if !(1..=2).contains(&args.len()) {
                    return Err(arity_err(name, args.len()));
                }
                let array = eval_full(args[0], arena, params, row, hooks)?;
                if array.is_null() {
                    return Ok(Datum::Null);
                }
                if !matches!(array, Datum::Array { .. }) {
                    return Err(type_mismatch("array_to_json requires an array", &array));
                }
                let mut buffer = crate::util::StackStr::<16384>::new();
                let _ = json::write_datum_json(&array, false, &mut buffer);
                if buffer.is_truncated() {
                    return Err(sql_err!(
                        sqlstate::PROGRAM_LIMIT_EXCEEDED,
                        "array_to_json value exceeds the supported size"
                    ));
                }
                Ok(Datum::Json {
                    text: arena.alloc_str(buffer.as_str()).map_err(|_| arena_full())?,
                    jsonb: false,
                })
            }
            "jsonb_array_length" | "json_array_length" => {
                arity(1)?;
                let s = match text_view(eval_full(args[0], arena, params, row, hooks)?) {
                    Datum::Json { text, .. } => text,
                    Datum::Text(s) => s,
                    Datum::Null => return Ok(Datum::Null),
                    other => return Err(type_mismatch(name, &other)),
                };
                match json::parse(s, arena)? {
                    json::Json::Array(items) => Ok(Datum::Int4(items.len() as i32)),
                    _ => Err(sql_err!(
                        sqlstate::INVALID_PARAMETER_VALUE,
                        "cannot get array length of a scalar"
                    )),
                }
            }
            // The JSON type name of the value, as PostgreSQL's json_typeof.
            "jsonb_typeof" | "json_typeof" => {
                arity(1)?;
                let s = match text_view(eval_full(args[0], arena, params, row, hooks)?) {
                    Datum::Json { text, .. } => text,
                    Datum::Text(s) => s,
                    Datum::Null => return Ok(Datum::Null),
                    other => return Err(type_mismatch(name, &other)),
                };
                Ok(Datum::Text(match json::parse(s, arena)? {
                    json::Json::Null => "null",
                    json::Json::Bool(_) => "boolean",
                    json::Json::Number(_) => "number",
                    json::Json::Str(_) => "string",
                    json::Json::Temporal { .. } => "string",
                    json::Json::Array(_) => "array",
                    json::Json::Object(_) => "object",
                }))
            }
            // `json_extract_path(json, VARIADIC keys)` / `_text`: navigate by keys.
            "json_extract_path"
            | "jsonb_extract_path"
            | "json_extract_path_text"
            | "jsonb_extract_path_text" => {
                if star || args.is_empty() {
                    return Err(sql_err!(
                        sqlstate::UNDEFINED_FUNCTION,
                        "function {}(...) does not exist",
                        name
                    ));
                }
                let (text, jsonb) = match text_view(eval_full(args[0], arena, params, row, hooks)?)
                {
                    Datum::Json { text, jsonb } => (text, jsonb),
                    Datum::Text(s) => (s, name.starts_with("jsonb")),
                    Datum::Null => return Ok(Datum::Null),
                    other => return Err(type_mismatch(name, &other)),
                };
                let as_text = name.ends_with("_text");
                let mut node = json::parse(text, arena)?;
                for key_arg in &args[1..] {
                    let step = text_view(eval_full(key_arg, arena, params, row, hooks)?);
                    let Datum::Text(key) = step else {
                        return Ok(Datum::Null);
                    };
                    let next = match &node {
                        json::Json::Object(_) => node.get_field(key),
                        json::Json::Array(_) => {
                            key.parse::<i64>().ok().and_then(|n| node.get_index(n))
                        }
                        _ => None,
                    };
                    let Some(next) = next else {
                        return Ok(Datum::Null);
                    };
                    node = next;
                }
                if as_text {
                    if let json::Json::Str(str_value) = node {
                        return Ok(Datum::Text(str_value));
                    }
                    if matches!(node, json::Json::Null) {
                        return Ok(Datum::Null);
                    }
                    return Ok(Datum::Text(json_to_text(&node, arena)?));
                }
                Ok(Datum::Json {
                    text: json_to_text(&node, arena)?,
                    jsonb,
                })
            }
            "row_to_json" | "to_json" | "to_jsonb" => {
                if star || args.is_empty() || args.len() > 2 {
                    return Err(sql_err!(
                        sqlstate::UNDEFINED_FUNCTION,
                        "function {}(...) does not exist",
                        name
                    ));
                }
                let v = eval_full(args[0], arena, params, row, hooks)?;
                let jsonb = name == "to_jsonb";
                let mut buf = crate::util::StackStr::<16384>::default();
                let _ = json::write_datum_json(&v, jsonb, &mut buf);
                debug_assert!(!buf.is_truncated());
                let text = arena.alloc_str(buf.as_str()).map_err(|_| arena_full())?;
                Ok(Datum::Json { text, jsonb })
            }
            // `jsonb_set(target, path, new_value [, create_if_missing])`.
            "jsonb_set" | "jsonb_set_lax" => {
                let lax = name == "jsonb_set_lax";
                let max_args = if lax { 5 } else { 4 };
                if !(3..=max_args).contains(&args.len()) {
                    return Err(arity_err(name, args.len()));
                }
                let target = eval_full(args[0], arena, params, row, hooks)?;
                if target.is_null() {
                    return Ok(Datum::Null);
                }
                let root = json_tree_arg(target, arena)?;
                let path = json_path_parts(eval_full(args[1], arena, params, row, hooks)?, arena)?;
                let raw_value = eval_full(args[2], arena, params, row, hooks)?;
                let create = if args.len() >= 4 {
                    match eval_full(args[3], arena, params, row, hooks)? {
                        Datum::Bool(b) => b,
                        Datum::Null => return Ok(Datum::Null),
                        other => {
                            return Err(type_mismatch("create_if_missing must be boolean", &other));
                        }
                    }
                } else {
                    true
                };
                // jsonb_set_lax's reason to exist: an SQL NULL new value is
                // handled per the fifth argument instead of nulling the result.
                if lax && raw_value.is_null() {
                    let treatment = if args.len() == 5 {
                        match text_view(eval_full(args[4], arena, params, row, hooks)?) {
                            Datum::Text(t) => t,
                            Datum::Null => return Ok(Datum::Null),
                            other => {
                                return Err(type_mismatch(
                                    "null_value_treatment must be text",
                                    &other,
                                ));
                            }
                        }
                    } else {
                        "use_json_null"
                    };
                    let result = match treatment {
                        "use_json_null" => json::set(root, path, json::Json::Null, create, arena)?,
                        "delete_key" => json::delete_path(root, path, arena)?,
                        "return_target" => root,
                        "raise_exception" => {
                            return Err(sql_err!(
                                sqlstate::NULL_VALUE_NOT_ALLOWED,
                                "JSON value must not be null"
                            ));
                        }
                        _ => {
                            return Err(sql_err!(
                                sqlstate::INVALID_PARAMETER_VALUE,
                                "null_value_treatment must be \"delete_key\", \"return_target\", \"use_json_null\", or \"raise_exception\""
                            ));
                        }
                    };
                    return Ok(Datum::Json {
                        text: json_to_text(&result, arena)?,
                        jsonb: true,
                    });
                }
                let value = json_tree_arg(raw_value, arena)?;
                let result = json::set(root, path, value, create, arena)?;
                Ok(Datum::Json {
                    text: json_to_text(&result, arena)?,
                    jsonb: true,
                })
            }
            // `jsonb_insert(target, path, new_value [, insert_after])`.
            "jsonb_insert" => {
                if !(3..=4).contains(&args.len()) {
                    return Err(arity_err(name, args.len()));
                }
                let target = eval_full(args[0], arena, params, row, hooks)?;
                if target.is_null() {
                    return Ok(Datum::Null);
                }
                let root = json_tree_arg(target, arena)?;
                let path = json_path_parts(eval_full(args[1], arena, params, row, hooks)?, arena)?;
                let value = json_tree_arg(eval_full(args[2], arena, params, row, hooks)?, arena)?;
                let after = if args.len() == 4 {
                    match eval_full(args[3], arena, params, row, hooks)? {
                        Datum::Bool(b) => b,
                        Datum::Null => return Ok(Datum::Null),
                        other => return Err(type_mismatch("insert_after must be boolean", &other)),
                    }
                } else {
                    false
                };
                let result = json::insert(root, path, value, after, arena)?;
                Ok(Datum::Json {
                    text: json_to_text(&result, arena)?,
                    jsonb: true,
                })
            }
            // `jsonb_strip_nulls` / `json_strip_nulls`: drop null-valued members.
            "jsonb_strip_nulls" | "json_strip_nulls" => {
                arity(1)?;
                let d = eval_full(args[0], arena, params, row, hooks)?;
                if d.is_null() {
                    return Ok(Datum::Null);
                }
                let jsonb =
                    matches!(d, Datum::Json { jsonb: true, .. }) || name.starts_with("jsonb");
                let result = json::strip_nulls(json_tree_arg(d, arena)?, arena)?;
                // A json result re-serializes compactly, a jsonb one in the
                // canonical spaced form — PostgreSQL's split exactly.
                let text = if jsonb {
                    json_to_text(&result, arena)?
                } else {
                    super::super::json_to_text_compact(&result, arena)?
                };
                Ok(Datum::Json { text, jsonb })
            }
            // `jsonb_pretty`: indented rendering of a jsonb value.
            "jsonb_pretty" => {
                arity(1)?;
                let d = eval_full(args[0], arena, params, row, hooks)?;
                if d.is_null() {
                    return Ok(Datum::Null);
                }
                let tree = json_tree_arg(d, arena)?;
                Ok(Datum::Text(json::pretty_to_arena(&tree, arena)?))
            }
            // `json_build_object(k1, v1, ...)` / `jsonb_build_object(...)`: an
            // object from alternating key/value arguments. json uses `" : "`
            // spacing, jsonb the canonical `": "`; both separate with `, `.
            "json_build_object" | "jsonb_build_object" => {
                if star {
                    return Err(sql_err!(
                        sqlstate::UNDEFINED_FUNCTION,
                        "function {}() does not exist",
                        name
                    ));
                }
                if !args.len().is_multiple_of(2) {
                    return Err(sql_err!(
                        sqlstate::INVALID_PARAMETER_VALUE,
                        "argument list must have even number of elements"
                    ));
                }
                let jsonb = name == "jsonb_build_object";
                let colon = if jsonb { ": " } else { " : " };
                let mut buf = crate::util::StackStr::<16384>::default();
                let _ = buf.write_char('{');
                for pair in args.chunks(2) {
                    let key = eval_full(pair[0], arena, params, row, hooks)?;
                    if key.is_null() {
                        return Err(sql_err!(
                            sqlstate::NULL_VALUE_NOT_ALLOWED,
                            "argument {}: key must not be null",
                            1
                        ));
                    }
                    let value = eval_full(pair[1], arena, params, row, hooks)?;
                    if !core::ptr::eq(pair.as_ptr(), args.as_ptr()) {
                        let _ = buf.write_str(", ");
                    }
                    let mut key_text = crate::util::StackStr::<4096>::default();
                    let _ = write!(key_text, "{key}");
                    let _ = json::write_json_raw_string(key_text.as_str(), &mut buf);
                    let _ = buf.write_str(colon);
                    let _ = json::write_datum_json_styled(&value, colon, ", ", &mut buf);
                }
                let _ = buf.write_char('}');
                debug_assert!(!buf.is_truncated());
                let text = arena.alloc_str(buf.as_str()).map_err(|_| arena_full())?;
                Ok(Datum::Json { text, jsonb })
            }
            // `json_build_array(v1, v2, ...)` / `jsonb_build_array(...)`.
            "json_build_array" | "jsonb_build_array" => {
                if star {
                    return Err(sql_err!(
                        sqlstate::UNDEFINED_FUNCTION,
                        "function {}() does not exist",
                        name
                    ));
                }
                let jsonb = name == "jsonb_build_array";
                let colon = if jsonb { ": " } else { " : " };
                let mut buf = crate::util::StackStr::<16384>::default();
                let _ = buf.write_char('[');
                for (i, a) in args.iter().enumerate() {
                    if i > 0 {
                        let _ = buf.write_str(", ");
                    }
                    let value = eval_full(a, arena, params, row, hooks)?;
                    let _ = json::write_datum_json_styled(&value, colon, ", ", &mut buf);
                }
                let _ = buf.write_char(']');
                debug_assert!(!buf.is_truncated());
                let text = arena.alloc_str(buf.as_str()).map_err(|_| arena_full())?;
                Ok(Datum::Json { text, jsonb })
            }
            _ => unreachable!("dispatch guard admitted an unhandled name"),
        }
    })())
}
