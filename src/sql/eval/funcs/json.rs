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
    text_view,
    arena_full, arity_err, eval_full, json_path_parts, json_to_text, json_tree_arg, sqlstate,
    type_mismatch, ColumnLookup, EvalHooks, SqlError,
};

/// Handles the JSON/JSONB scalar family. Returns `None` if `name` is not one of
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
    if !matches!(
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
    ) {
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
                    return Err(sql_err!(sqlstate::PROGRAM_LIMIT_EXCEEDED, "array_to_json value exceeds the supported size"));
                }
                Ok(Datum::Json { text: arena.alloc_str(buffer.as_str()).map_err(|_| arena_full())?, jsonb: false })
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
                    _ => Err(sql_err!(sqlstate::INVALID_PARAMETER_VALUE, "cannot get array length of a scalar")),
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
                    json::Json::Array(_) => "array",
                    json::Json::Object(_) => "object",
                }))
            }
            // `json_extract_path(json, VARIADIC keys)` / `_text`: navigate by keys.
            "json_extract_path" | "jsonb_extract_path" | "json_extract_path_text"
            | "jsonb_extract_path_text" => {
                if star || args.is_empty() {
                    return Err(sql_err!(sqlstate::UNDEFINED_FUNCTION, "function {}(...) does not exist", name));
                }
                let (text, jsonb) = match text_view(eval_full(args[0], arena, params, row, hooks)?) {
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
                        return Ok(Datum::Text(json::decode_string(str_value, arena)?));
                    }
                    if matches!(node, json::Json::Null) {
                        return Ok(Datum::Null);
                    }
                    return Ok(Datum::Text(json_to_text(&node, arena)?));
                }
                Ok(Datum::Json { text: json_to_text(&node, arena)?, jsonb })
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
                        other => return Err(type_mismatch("create_if_missing must be boolean", &other)),
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
                                return Err(type_mismatch("null_value_treatment must be text", &other))
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
                            ))
                        }
                        _ => {
                            return Err(sql_err!(
                                sqlstate::INVALID_PARAMETER_VALUE,
                                "null_value_treatment must be \"delete_key\", \"return_target\", \"use_json_null\", or \"raise_exception\""
                            ))
                        }
                    };
                    return Ok(Datum::Json { text: json_to_text(&result, arena)?, jsonb: true });
                }
                let value = json_tree_arg(raw_value, arena)?;
                let result = json::set(root, path, value, create, arena)?;
                Ok(Datum::Json { text: json_to_text(&result, arena)?, jsonb: true })
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
                Ok(Datum::Json { text: json_to_text(&result, arena)?, jsonb: true })
            }
            // `jsonb_strip_nulls` / `json_strip_nulls`: drop null-valued members.
            "jsonb_strip_nulls" | "json_strip_nulls" => {
                arity(1)?;
                let d = eval_full(args[0], arena, params, row, hooks)?;
                if d.is_null() {
                    return Ok(Datum::Null);
                }
                let jsonb = matches!(d, Datum::Json { jsonb: true, .. }) || name.starts_with("jsonb");
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
                    return Err(sql_err!(sqlstate::UNDEFINED_FUNCTION, "function {}() does not exist", name));
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
                        return Err(sql_err!(sqlstate::NULL_VALUE_NOT_ALLOWED, "argument {}: key must not be null", 1));
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
                    return Err(sql_err!(sqlstate::UNDEFINED_FUNCTION, "function {}() does not exist", name));
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
