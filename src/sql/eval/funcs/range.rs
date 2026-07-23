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
    arity_err, eval_full, range_mismatch, sqlstate, text_arg, type_mismatch, ColumnLookup,
    EvalHooks, SqlError,
};

/// Handles the range/multirange family. Returns `None` if `name` is not one of
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
            | "isempty"
            | "lower_inc"
            | "upper_inc"
            | "lower_inf"
            | "upper_inf"
            | "range_merge"
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
                Ok(Datum::Range { text: range::construct(lo, hi, flags, kind, arena)?, kind })
            }
            "int4multirange" | "int8multirange" | "nummultirange" | "datemultirange"
            | "tsmultirange" | "tstzmultirange" => {
                let kind = RangeKind::from_multirange_name(name).expect("matched a multirange name");
                // Each argument is a range of the matching subtype; non-empty
                // component texts are collected then canonicalized. A NULL argument
                // makes the whole result NULL, matching PostgreSQL's strict
                // multirange constructors.
                let mut comps: [&str; range::MAX_MULTIRANGE] =
                    [""; range::MAX_MULTIRANGE];
                let mut n = 0usize;
                for arg in args.iter() {
                    match eval_full(arg, arena, params, row, hooks)? {
                        Datum::Null => return Ok(Datum::Null),
                        Datum::Range { text, kind: k } if k == kind => {
                            if !range::is_empty(text) {
                                if n == range::MAX_MULTIRANGE {
                                    return Err(sql_err!(sqlstate::PROGRAM_LIMIT_EXCEEDED, "multirange has too many component ranges"));
                                }
                                comps[n] = text;
                                n += 1;
                            }
                        }
                        other => return Err(type_mismatch(name, &other)),
                    }
                }
                let text = range::canonicalize_multirange(&mut comps[..n], kind, arena)?;
                Ok(Datum::Multirange { text, kind })
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
                    other => Err(type_mismatch(name, &other)),
                }
            }
            "range_merge" => {
                arity(2)?;
                let a = eval_full(args[0], arena, params, row, hooks)?;
                let b = eval_full(args[1], arena, params, row, hooks)?;
                if a.is_null() || b.is_null() {
                    return Ok(Datum::Null);
                }
                let (Datum::Range { text: at, kind: ak }, Datum::Range { text: bt, kind: bk }) = (a, b)
                else {
                    return Err(type_mismatch(name, &a));
                };
                if ak != bk {
                    return Err(range_mismatch());
                }
                Ok(Datum::Range { text: range::merge(at, bt, ak, arena)?, kind: ak })
            }
            _ => unreachable!("dispatch guard admitted an unhandled name"),
        }
    })())
}
