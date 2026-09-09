//! Range and multirange built-ins.
//!
//! Covers the range constructors (`int4range`/`int8range`/`numrange`/
//! `daterange`/`tsrange`/`tstzrange`), the multirange constructors
//! (`int4multirange`/…/`tstzmultirange`), and the inspection functions
//! `isempty`, `lower_inc`/`upper_inc`, `lower_inf`/`upper_inf`, and
//! `range_merge`. The `lower`/`upper` bound accessors live with the string
//! family (they overload the text case functions).

use crate::sql::ast::Expr;
use crate::sql::range;
use crate::sql::types::{Datum, RangeKind};
use crate::sql_err;

use super::super::{
    ColumnLookup, EvalHooks, SqlError, arity_err, eval_full, range_mismatch, sqlstate, text_arg,
    type_mismatch,
};

/// Handles the range/multirange family. Returns `None` if `name` is not one of
/// these functions, leaving the router to keep matching.
#[allow(clippy::too_many_arguments)]
pub(crate) fn dispatch<'a>(
    name: &str,
    args: &[&Expr<'a>],
    star: bool,
    variadic: bool,
    arena: &'a crate::mem::arena::Arena,
    params: &[Datum<'a>],
    row: &impl ColumnLookup<'a>,
    hooks: &EvalHooks<'_, 'a>,
) -> Option<Result<Datum<'a>, SqlError>> {
    if !matches!(
        name,
        "int4range"
            | "int8range"
            | "numrange"
            | "daterange"
            | "tsrange"
            | "tstzrange"
            | "int4multirange"
            | "int8multirange"
            | "nummultirange"
            | "datemultirange"
            | "tsmultirange"
            | "tstzmultirange"
            | "multirange"
            | "isempty"
            | "lower_inc"
            | "upper_inc"
            | "lower_inf"
            | "upper_inf"
            | "range_merge"
            | "range_eq"
            | "range_ne"
            | "range_lt"
            | "range_le"
            | "range_ge"
            | "range_gt"
            | "range_cmp"
            | "range_overlaps"
            | "range_contains_elem"
            | "range_contains"
            | "elem_contained_by_range"
            | "range_contained_by"
            | "range_adjacent"
            | "range_before"
            | "range_after"
            | "range_overleft"
            | "range_overright"
            | "range_union"
            | "range_intersect"
            | "range_minus"
            | "multirange_eq"
            | "multirange_ne"
            | "multirange_lt"
            | "multirange_le"
            | "multirange_ge"
            | "multirange_gt"
            | "multirange_cmp"
            | "multirange_overlaps_range"
            | "range_overlaps_multirange"
            | "multirange_overlaps_multirange"
            | "multirange_contains_elem"
            | "multirange_contains_range"
            | "multirange_contains_multirange"
            | "range_contains_multirange"
            | "elem_contained_by_multirange"
            | "range_contained_by_multirange"
            | "multirange_contained_by_multirange"
            | "multirange_contained_by_range"
            | "range_adjacent_multirange"
            | "multirange_adjacent_range"
            | "multirange_adjacent_multirange"
            | "range_before_multirange"
            | "multirange_before_range"
            | "multirange_before_multirange"
            | "range_after_multirange"
            | "multirange_after_range"
            | "multirange_after_multirange"
            | "range_overleft_multirange"
            | "multirange_overleft_range"
            | "multirange_overleft_multirange"
            | "range_overright_multirange"
            | "multirange_overright_range"
            | "multirange_overright_multirange"
            | "multirange_union"
            | "multirange_minus"
            | "multirange_intersect"
            | "hash_range"
            | "hash_range_extended"
            | "hash_multirange"
            | "hash_multirange_extended"
            | "int4range_canonical"
            | "int8range_canonical"
            | "daterange_canonical"
            | "int4range_subdiff"
            | "int8range_subdiff"
            | "numrange_subdiff"
            | "daterange_subdiff"
            | "tsrange_subdiff"
            | "tstzrange_subdiff"
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
            "int4range" | "int8range" | "numrange" | "daterange" | "tsrange" | "tstzrange" => {
                let kind = RangeKind::from_name(name).expect("matched a range name");
                if !(2..=3).contains(&args.len()) {
                    return Err(arity_err(name, args.len()));
                }
                let lo = eval_full(args[0], arena, params, row, hooks)?;
                let hi = eval_full(args[1], arena, params, row, hooks)?;
                let flags = if args.len() == 3 {
                    text_arg(name, args, 2, arena, params, row, hooks)?
                } else {
                    None
                };
                Ok(Datum::Range {
                    text: range::construct(lo, hi, flags, kind, arena)?,
                    kind,
                })
            }
            "int4multirange" | "int8multirange" | "nummultirange" | "datemultirange"
            | "tsmultirange" | "tstzmultirange" => {
                let kind =
                    RangeKind::from_multirange_name(name).expect("matched a multirange name");
                // Each argument is a range of the matching subtype; non-empty
                // component texts are collected then canonicalized. A NULL argument
                // makes the whole result NULL, matching PostgreSQL's strict
                // multirange constructors.
                let mut comps: [&str; range::MAX_MULTIRANGE] = [""; range::MAX_MULTIRANGE];
                let mut n = 0usize;
                let mut add = |value: Datum<'a>| -> Result<bool, SqlError> {
                    match value {
                        Datum::Null => Ok(true),
                        Datum::Range { text, kind: k } if k == kind => {
                            if !range::is_empty(text) {
                                if n == range::MAX_MULTIRANGE {
                                    return Err(sql_err!(
                                        sqlstate::PROGRAM_LIMIT_EXCEEDED,
                                        "multirange has too many component ranges"
                                    ));
                                }
                                comps[n] = text;
                                n += 1;
                            }
                            Ok(false)
                        }
                        other => Err(type_mismatch(name, &other)),
                    }
                };
                if args.len() == 1 {
                    let value = eval_full(args[0], arena, params, row, hooks)?;
                    if let Datum::Array { element, raw } = value {
                        if !variadic {
                            return Err(arity_err(name, args.len()));
                        }
                        if element.to_coltype() != crate::sql::types::ColType::Range(kind) {
                            return Err(type_mismatch(name, &value));
                        }
                        for index in 0..crate::sql::array::len(raw) {
                            if add(
                                crate::sql::array::get(raw, element, index).unwrap_or(Datum::Null)
                            )? {
                                return Ok(Datum::Null);
                            }
                        }
                    } else if add(value)? {
                        return Ok(Datum::Null);
                    }
                } else {
                    for arg in args.iter() {
                        if add(eval_full(arg, arena, params, row, hooks)?)? {
                            return Ok(Datum::Null);
                        }
                    }
                }
                let text = range::canonicalize_multirange(&mut comps[..n], kind, arena)?;
                Ok(Datum::Multirange { text, kind })
            }
            "multirange" => {
                arity(1)?;
                match eval_full(args[0], arena, params, row, hooks)? {
                    Datum::Null => Ok(Datum::Null),
                    Datum::Range { text, kind } => Ok(Datum::Multirange {
                        text: range::multirange_from_range(text, kind, arena)?,
                        kind,
                    }),
                    other => Err(type_mismatch(name, &other)),
                }
            }
            "isempty" => {
                arity(1)?;
                match eval_full(args[0], arena, params, row, hooks)? {
                    Datum::Null => Ok(Datum::Null),
                    Datum::Range { text, .. } => Ok(Datum::Bool(range::is_empty(text))),
                    Datum::Multirange { text, .. } => Ok(Datum::Bool(text.trim() == "{}")),
                    other => Err(type_mismatch(name, &other)),
                }
            }
            "lower_inc" | "upper_inc" => {
                arity(1)?;
                match eval_full(args[0], arena, params, row, hooks)? {
                    Datum::Null => Ok(Datum::Null),
                    Datum::Range { text, kind: _ } => {
                        Ok(Datum::Bool(range::bound_inc(text, name == "lower_inc")?))
                    }
                    Datum::Multirange { text, kind: _ } => {
                        let mut components = [""; range::MAX_MULTIRANGE];
                        let count = range::split_components(text, &mut components)?;
                        if count == 0 {
                            Ok(Datum::Bool(false))
                        } else {
                            let component = if name == "upper_inc" {
                                components[count - 1]
                            } else {
                                components[0]
                            };
                            Ok(Datum::Bool(range::bound_inc(
                                component,
                                name == "lower_inc",
                            )?))
                        }
                    }
                    other => Err(type_mismatch(name, &other)),
                }
            }
            "lower_inf" | "upper_inf" => {
                arity(1)?;
                match eval_full(args[0], arena, params, row, hooks)? {
                    Datum::Null => Ok(Datum::Null),
                    Datum::Range { text, kind: _ } => Ok(Datum::Bool(if name == "lower_inf" {
                        range::lower_inf(text)?
                    } else {
                        range::upper_inf(text)?
                    })),
                    Datum::Multirange { text, kind: _ } => {
                        let mut components = [""; range::MAX_MULTIRANGE];
                        let count = range::split_components(text, &mut components)?;
                        if count == 0 {
                            Ok(Datum::Bool(false))
                        } else {
                            let component = if name == "upper_inf" {
                                components[count - 1]
                            } else {
                                components[0]
                            };
                            Ok(Datum::Bool(if name == "lower_inf" {
                                range::lower_inf(component)?
                            } else {
                                range::upper_inf(component)?
                            }))
                        }
                    }
                    other => Err(type_mismatch(name, &other)),
                }
            }
            "range_merge" => {
                if args.len() == 1 {
                    let value = eval_full(args[0], arena, params, row, hooks)?;
                    return match value {
                        Datum::Null => Ok(Datum::Null),
                        Datum::Multirange { text, kind } => {
                            let mut components = [""; range::MAX_MULTIRANGE];
                            let count = range::split_components(text, &mut components)?;
                            if count == 0 {
                                Ok(Datum::Range {
                                    text: "empty",
                                    kind,
                                })
                            } else {
                                Ok(Datum::Range {
                                    text: range::merge(
                                        components[0],
                                        components[count - 1],
                                        kind,
                                        arena,
                                    )?,
                                    kind,
                                })
                            }
                        }
                        other => Err(type_mismatch(name, &other)),
                    };
                }
                arity(2)?;
                let a = eval_full(args[0], arena, params, row, hooks)?;
                let b = eval_full(args[1], arena, params, row, hooks)?;
                if a.is_null() || b.is_null() {
                    return Ok(Datum::Null);
                }
                let (Datum::Range { text: at, kind: ak }, Datum::Range { text: bt, kind: bk }) =
                    (a, b)
                else {
                    return Err(type_mismatch(name, &a));
                };
                if ak != bk {
                    return Err(range_mismatch());
                }
                Ok(Datum::Range {
                    text: range::merge(at, bt, ak, arena)?,
                    kind: ak,
                })
            }
            "range_eq" | "range_ne" | "range_lt" | "range_le" | "range_ge" | "range_gt"
            | "multirange_eq" | "multirange_ne" | "multirange_lt" | "multirange_le"
            | "multirange_ge" | "multirange_gt" => {
                arity(2)?;
                let left = eval_full(args[0], arena, params, row, hooks)?;
                let right = eval_full(args[1], arena, params, row, hooks)?;
                let operator = if name.ends_with("_eq") {
                    crate::sql::ast::BinaryOp::Eq
                } else if name.ends_with("_ne") {
                    crate::sql::ast::BinaryOp::NotEq
                } else if name.ends_with("_lt") {
                    crate::sql::ast::BinaryOp::Lt
                } else if name.ends_with("_le") {
                    crate::sql::ast::BinaryOp::LtEq
                } else if name.ends_with("_ge") {
                    crate::sql::ast::BinaryOp::GtEq
                } else {
                    crate::sql::ast::BinaryOp::Gt
                };
                super::super::operators::compare(operator, left, right, false, false)
            }
            "range_cmp" | "multirange_cmp" => {
                arity(2)?;
                let left = eval_full(args[0], arena, params, row, hooks)?;
                let right = eval_full(args[1], arena, params, row, hooks)?;
                if left.is_null() || right.is_null() {
                    return Ok(Datum::Null);
                }
                let ordering = match (left, right) {
                    (Datum::Range { text: a, kind: ak }, Datum::Range { text: b, kind: bk })
                        if ak == bk =>
                    {
                        range::cmp_ranges(a, b, ak)?
                    }
                    (
                        Datum::Multirange { text: a, kind: ak },
                        Datum::Multirange { text: b, kind: bk },
                    ) if ak == bk => range::cmp_multiranges(a, b, ak)?,
                    _ => return Err(range_mismatch()),
                };
                Ok(Datum::Int4(match ordering {
                    core::cmp::Ordering::Less => -1,
                    core::cmp::Ordering::Equal => 0,
                    core::cmp::Ordering::Greater => 1,
                }))
            }
            "range_overlaps"
            | "range_contains_elem"
            | "range_contains"
            | "elem_contained_by_range"
            | "range_contained_by"
            | "range_adjacent"
            | "range_before"
            | "range_after"
            | "range_overleft"
            | "range_overright"
            | "multirange_overlaps_range"
            | "range_overlaps_multirange"
            | "multirange_overlaps_multirange"
            | "multirange_contains_elem"
            | "multirange_contains_range"
            | "multirange_contains_multirange"
            | "range_contains_multirange"
            | "elem_contained_by_multirange"
            | "range_contained_by_multirange"
            | "multirange_contained_by_multirange"
            | "range_adjacent_multirange"
            | "multirange_contained_by_range"
            | "multirange_adjacent_range"
            | "multirange_adjacent_multirange"
            | "range_before_multirange"
            | "multirange_before_range"
            | "multirange_before_multirange"
            | "range_after_multirange"
            | "multirange_after_range"
            | "multirange_after_multirange"
            | "range_overleft_multirange"
            | "multirange_overleft_range"
            | "multirange_overleft_multirange"
            | "range_overright_multirange"
            | "multirange_overright_range"
            | "multirange_overright_multirange" => {
                arity(2)?;
                let left = eval_full(args[0], arena, params, row, hooks)?;
                let right = eval_full(args[1], arena, params, row, hooks)?;
                let operator = if name.contains("overlaps") {
                    crate::sql::ast::BinaryOp::Overlaps
                } else if name.contains("contained_by") || name.starts_with("elem_contained") {
                    crate::sql::ast::BinaryOp::ContainedBy
                } else if name.contains("contains") {
                    crate::sql::ast::BinaryOp::Contains
                } else if name.contains("adjacent") {
                    crate::sql::ast::BinaryOp::Adjacent
                } else if name.contains("before") {
                    crate::sql::ast::BinaryOp::Shl
                } else if name.contains("after") {
                    crate::sql::ast::BinaryOp::Shr
                } else if name.contains("overleft") {
                    crate::sql::ast::BinaryOp::NotRightOf
                } else {
                    crate::sql::ast::BinaryOp::NotLeftOf
                };
                if matches!(left, Datum::Multirange { .. })
                    || matches!(right, Datum::Multirange { .. })
                {
                    super::super::operators::multirange_op(operator, left, right, arena)
                } else {
                    super::super::operators::range_op(operator, left, right, arena)
                }
            }
            "range_union" | "range_intersect" | "range_minus" => {
                arity(2)?;
                let left = eval_full(args[0], arena, params, row, hooks)?;
                let right = eval_full(args[1], arena, params, row, hooks)?;
                let operator = match name {
                    "range_union" => crate::sql::ast::BinaryOp::Add,
                    "range_intersect" => crate::sql::ast::BinaryOp::Mul,
                    _ => crate::sql::ast::BinaryOp::Sub,
                };
                super::super::operators::range_setop(operator, left, right, arena)
            }
            "multirange_union" | "multirange_intersect" | "multirange_minus" => {
                arity(2)?;
                let left = eval_full(args[0], arena, params, row, hooks)?;
                let right = eval_full(args[1], arena, params, row, hooks)?;
                let operator = match name {
                    "multirange_union" => crate::sql::ast::BinaryOp::Add,
                    "multirange_intersect" => crate::sql::ast::BinaryOp::Mul,
                    _ => crate::sql::ast::BinaryOp::Sub,
                };
                super::super::operators::multirange_setop(operator, left, right, arena)
            }
            "hash_range"
            | "hash_range_extended"
            | "hash_multirange"
            | "hash_multirange_extended" => {
                let extended = name.ends_with("_extended");
                arity(if extended { 2 } else { 1 })?;
                let value = eval_full(args[0], arena, params, row, hooks)?;
                if value.is_null() {
                    return Ok(Datum::Null);
                }
                let seed = if extended {
                    match eval_full(args[1], arena, params, row, hooks)? {
                        Datum::Int8(seed) => Some(seed),
                        Datum::Int4(seed) => Some(i64::from(seed)),
                        Datum::Null => return Ok(Datum::Null),
                        other => return Err(type_mismatch(name, &other)),
                    }
                } else {
                    None
                };
                let hash = match value {
                    Datum::Range { text, kind } => range::hash_range(text, kind, seed, arena)?,
                    Datum::Multirange { text, kind } => {
                        range::hash_multirange(text, kind, seed, arena)?
                    }
                    other => return Err(type_mismatch(name, &other)),
                };
                Ok(if extended {
                    Datum::Int8(hash as i64)
                } else {
                    Datum::Int4(hash as u32 as i32)
                })
            }
            "int4range_canonical" | "int8range_canonical" | "daterange_canonical" => {
                arity(1)?;
                match eval_full(args[0], arena, params, row, hooks)? {
                    Datum::Null => Ok(Datum::Null),
                    value @ Datum::Range { .. } => Ok(value),
                    other => Err(type_mismatch(name, &other)),
                }
            }
            "int4range_subdiff" | "int8range_subdiff" | "numrange_subdiff"
            | "daterange_subdiff" | "tsrange_subdiff" | "tstzrange_subdiff" => {
                arity(2)?;
                let left = eval_full(args[0], arena, params, row, hooks)?;
                let right = eval_full(args[1], arena, params, row, hooks)?;
                if left.is_null() || right.is_null() {
                    return Ok(Datum::Null);
                }
                let difference = match (left, right) {
                    (Datum::Int4(a), Datum::Int4(b)) => f64::from(a) - f64::from(b),
                    (Datum::Int8(a), Datum::Int8(b)) => a as f64 - b as f64,
                    (Datum::Numeric(a), Datum::Numeric(b)) => a.to_f64() - b.to_f64(),
                    (Datum::Date(a), Datum::Date(b)) => f64::from(a) - f64::from(b),
                    (Datum::Timestamp(a), Datum::Timestamp(b))
                    | (Datum::Timestamptz(a), Datum::Timestamptz(b)) => {
                        (a as f64 - b as f64) / 1_000_000.0
                    }
                    _ => return Err(type_mismatch(name, &left)),
                };
                Ok(Datum::Float8(difference))
            }
            _ => unreachable!("dispatch guard admitted an unhandled name"),
        }
    })())
}
