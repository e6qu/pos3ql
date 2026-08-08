//! Array scalar built-ins.
//!
//! Covers size and bounds (`array_length`/`cardinality`/`array_upper`/
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
    ColumnLookup, EvalHooks, SqlError, arena_full, arity_err, cast_to, compare_datums, eval_full,
    load_array, sqlstate, text_arg, text_view, type_mismatch, unify_arr_elem,
};

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
    if !matches!(
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
                                return Err(type_mismatch(
                                    "array dimension must be an integer",
                                    &value,
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
                    Datum::Null => Ok(Datum::Null),
                    _ => Err(type_mismatch("array_length requires an array", &a)),
                }
            }
            "array_position" => {
                // 1-based index of the first element equal to the target (NULL-safe),
                // or NULL if absent.
                arity(2)?;
                let a = eval_full(args[0], arena, params, row, hooks)?;
                let target = eval_full(args[1], arena, params, row, hooks)?;
                let (element, raw) = match a {
                    Datum::Array { element, raw } => (element, raw),
                    Datum::Null => return Ok(Datum::Null),
                    _ => return Err(type_mismatch("array_position requires an array", &a)),
                };
                for i in 0..array::len(raw) {
                    let el = array::get(raw, element, i).unwrap_or(Datum::Null);
                    let hit = if target.is_null() {
                        el.is_null()
                    } else if el.is_null() {
                        false
                    } else {
                        compare_datums(&el, &target)?.is_eq()
                    };
                    if hit {
                        return Ok(Datum::Int4((i + 1) as i32));
                    }
                }
                Ok(Datum::Null)
            }
            "array_positions" => {
                // 1-based indices of every element equal to the target (NULL-safe),
                // as an int[] (`{}` when none); NULL only for a NULL array argument.
                arity(2)?;
                let a = eval_full(args[0], arena, params, row, hooks)?;
                let target = eval_full(args[1], arena, params, row, hooks)?;
                let (element, raw) = match a {
                    Datum::Array { element, raw } => (element, raw),
                    Datum::Null => return Ok(Datum::Null),
                    _ => return Err(type_mismatch("array_positions requires an array", &a)),
                };
                let matches = |el: &Datum| -> Result<bool, SqlError> {
                    Ok(if target.is_null() {
                        el.is_null()
                    } else if el.is_null() {
                        false
                    } else {
                        compare_datums(el, &target)?.is_eq()
                    })
                };
                let len = array::len(raw);
                let mut count = 0usize;
                for i in 0..len {
                    if matches(&array::get(raw, element, i).unwrap_or(Datum::Null))? {
                        count += 1;
                    }
                }
                let positions: &mut [Datum] = arena
                    .alloc_slice_with(count, |_| Datum::Null)
                    .map_err(|_| arena_full())?;
                let mut at = 0usize;
                for i in 0..len {
                    if matches(&array::get(raw, element, i).unwrap_or(Datum::Null))? {
                        positions[at] = Datum::Int4((i + 1) as i32);
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
                let arr = eval_full(args[array_index], arena, params, row, hooks)?;
                let elem = eval_full(args[elem_index], arena, params, row, hooks)?;
                let (source, raw) = match arr {
                    Datum::Array { element, raw } => (element, Some(raw)),
                    Datum::Null => (ArrElem::from_datum(&elem).unwrap_or(ArrElem::Text), None),
                    _ => {
                        return Err(type_mismatch(
                            "array_append/prepend requires an array",
                            &arr,
                        ));
                    }
                };
                // The result element type promotes to hold both the array's elements
                // and the new one (PostgreSQL's polymorphic anyarray/anyelement).
                let element = match ArrElem::from_datum(&elem) {
                    Some(e) => unify_arr_elem(source, e),
                    None => source,
                };
                let mut items = [Datum::Null; 1024];
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
                    cast_to(elem, element.to_coltype(), arena)?
                };
                if name == "array_prepend" {
                    items[n] = coerced;
                    n += 1;
                }
                if let Some(raw) = raw {
                    n = load_array(raw, source, element, &mut items, n, arena)?;
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
                let a = eval_full(args[0], arena, params, row, hooks)?;
                let b = eval_full(args[1], arena, params, row, hooks)?;
                match (a, b) {
                    (Datum::Array { .. }, Datum::Array { .. }) => {
                        super::super::array_concat(a, b, arena)
                    }
                    (Datum::Array { element, raw }, Datum::Null)
                    | (Datum::Null, Datum::Array { element, raw }) => {
                        Ok(Datum::Array { element, raw })
                    }
                    (Datum::Null, Datum::Null) => Ok(Datum::Null),
                    _ => Err(type_mismatch("array_cat requires arrays", &a)),
                }
            }
            // `array_remove(arr, elem)`: drop every element equal to `elem`
            // (NULL-safe). `array_replace(arr, from, to)`: replace every match.
            "array_remove" | "array_replace" => {
                let is_replace = name == "array_replace";
                arity(if is_replace { 3 } else { 2 })?;
                let arr = eval_full(args[0], arena, params, row, hooks)?;
                let target = eval_full(args[1], arena, params, row, hooks)?;
                let (source, raw) = match arr {
                    Datum::Array { element, raw } => (element, raw),
                    Datum::Null => return Ok(Datum::Null),
                    _ => return Err(type_mismatch(name, &arr)),
                };
                let shape = array::shape(raw).expect("array datum invariant");
                if !is_replace && shape.dimension_count() > 1 {
                    return Err(sql_err!(
                        sqlstate::FEATURE_NOT_SUPPORTED,
                        "array_remove does not support multidimensional arrays"
                    ));
                }
                let (element, replacement) = if is_replace {
                    let to = eval_full(args[2], arena, params, row, hooks)?;
                    // The result promotes to hold both the kept and replaced values.
                    let element = match ArrElem::from_datum(&to) {
                        Some(e) => unify_arr_elem(source, e),
                        None => source,
                    };
                    let replacement = if to.is_null() {
                        Datum::Null
                    } else {
                        cast_to(to, element.to_coltype(), arena)?
                    };
                    (element, replacement)
                } else {
                    (source, Datum::Null)
                };
                let to_coltype = element.to_coltype();
                let mut items = [Datum::Null; 1024];
                let mut n = 0usize;
                for i in 0..array::len(raw) {
                    let el = array::get(raw, source, i).unwrap_or(Datum::Null);
                    let matches = if target.is_null() {
                        el.is_null()
                    } else if el.is_null() {
                        false
                    } else {
                        compare_datums(&el, &target)?.is_eq()
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
                        array::build(&items[..n], arena)?
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
                    _ => return Err(type_mismatch("trim_array requires an array", &arr)),
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
                    _ => return Err(type_mismatch("trim_array count must be an integer", &count)),
                };
                if trim < 0 || trim as usize > total {
                    return Err(sql_err!(
                        sqlstate::ARRAY_SUBSCRIPT_ERROR,
                        "number of elements to trim must be between 0 and {}",
                        total
                    ));
                }
                let keep = total - trim as usize;
                let mut items = [Datum::Null; 1024];
                let n = load_array(raw, element, element, &mut items, 0, arena)?;
                Ok(Datum::Array {
                    element,
                    raw: array::build(&items[..keep.min(n)], arena)?,
                })
            }
            "array_ndims" | "array_dims" => {
                arity(1)?;
                let arr = eval_full(args[0], arena, params, row, hooks)?;
                let raw = match arr {
                    Datum::Array { raw, .. } => raw,
                    Datum::Null => return Ok(Datum::Null),
                    _ => return Err(type_mismatch(name, &arr)),
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
                        other => return Err(type_mismatch("array_to_string null string", &other)),
                    }
                } else {
                    None
                };
                let (element, raw) = match a {
                    Datum::Null => return Ok(Datum::Null),
                    Datum::Array { element, raw } => (element, raw),
                    other => return Err(type_mismatch("array_to_string", &other)),
                };
                let delim = match text_view(delim) {
                    Datum::Null => return Ok(Datum::Null),
                    Datum::Text(s) => s,
                    other => return Err(type_mismatch("array_to_string delimiter", &other)),
                };
                let count = array::len(raw);
                // Renders the i-th element as text, or `None` to omit it (a NULL
                // element with no null-string replacement).
                let elem_text = |i: usize| -> Result<Option<&'a str>, SqlError> {
                    match array::get(raw, element, i) {
                        Some(Datum::Null) | None => Ok(nullrep),
                        Some(v) => match cast_to(v, ColType::Text, arena)? {
                            Datum::Text(s) => Ok(Some(s)),
                            Datum::Null => Ok(nullrep),
                            other => Err(type_mismatch("array_to_string element", &other)),
                        },
                    }
                };
                // Pass 1: total byte length; pass 2: fill (elements re-rendered).
                let mut total = 0usize;
                let mut first = true;
                for i in 0..count {
                    if let Some(s) = elem_text(i)? {
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
                for i in 0..count {
                    if let Some(s) = elem_text(i)? {
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
            "array_fill" => {
                if args.len() != 2 && args.len() != 3 || star {
                    return Err(arity_err(name, args.len()));
                }
                let value = eval_full(args[0], arena, params, row, hooks)?;
                let dims = eval_full(args[1], arena, params, row, hooks)?;
                if dims.is_null() {
                    return Err(sql_err!(
                        sqlstate::NULL_VALUE_NOT_ALLOWED,
                        "dimension array or low bound array cannot be null"
                    ));
                }
                let Datum::Array { element, raw } = dims else {
                    return Err(type_mismatch(name, &dims));
                };
                let dims_shape = array::shape(raw).expect("array datum invariant");
                if dims_shape.dimension_count() > 1 || element != ArrElem::Int4 {
                    return Err(type_mismatch(
                        "array_fill dimensions must be an integer array",
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
                    let lows = eval_full(args[2], arena, params, row, hooks)?;
                    let Datum::Array {
                        element: low_element,
                        raw: low_raw,
                    } = lows
                    else {
                        return Err(type_mismatch(
                            "array_fill lower bounds must be an integer array",
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
                let mut items: [Datum; 1024] = [Datum::Null; 1024];
                let mut pieces: [&str; 1024] = [""; 1024];
                let n = super::super::split_pieces(s, delim, &mut pieces)?;
                for (k, &piece) in pieces[..n].iter().enumerate() {
                    items[k] = if null_str == Some(piece) {
                        Datum::Null
                    } else {
                        Datum::Text(piece)
                    };
                }
                Ok(Datum::Array {
                    element: ArrElem::Text,
                    raw: array::build(&items[..n], arena)?,
                })
            }
            _ => unreachable!("dispatch guard admitted an unhandled name"),
        }
    })())
}
