//! Conditional / null-handling built-ins.
//!
//! Covers `coalesce`, `num_nonnulls`/`num_nulls`, `greatest`/`least`, and
//! `nullif` — the variadic null-aware selectors and the common-type
//! greatest/least comparison.

use crate::sql::ast::Expr;
use crate::sql::numeric::Numeric;
use crate::sql::types::{ColType, Datum};
use crate::sql_err;

use super::super::{
    arity_err, compare_datums, eval_full, sqlstate, static_type, ColumnLookup, EvalHooks, SqlError,
};

/// Handles the conditional/null-handling family. Returns `None` if `name` is not
/// one of these functions, leaving the router to keep matching.
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
        "coalesce" | "num_nonnulls" | "num_nulls" | "greatest" | "least" | "nullif"
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
            "coalesce" => {
                for arg in args {
                    let v = eval_full(arg, arena, params, row, hooks)?;
                    if !v.is_null() {
                        return Ok(v);
                    }
                }
                Ok(Datum::Null)
            }
            // Count the non-null / null arguments (PostgreSQL's variadic
            // num_nonnulls / num_nulls; never NULL themselves).
            "num_nonnulls" | "num_nulls" => {
                if args.is_empty() {
                    return Err(arity_err(name, 0));
                }
                let mut nulls = 0i32;
                for arg in args {
                    if eval_full(arg, arena, params, row, hooks)?.is_null() {
                        nulls += 1;
                    }
                }
                Ok(Datum::Int4(if name == "num_nulls" {
                    nulls
                } else {
                    args.len() as i32 - nulls
                }))
            }
            "greatest" | "least" => {
                if star || args.is_empty() {
                    return Err(arity_err(name, args.len()));
                }
                // PostgreSQL types the result as the common type of all arguments,
                // so `least(1, 2.5)` is numeric even when the int wins. The rank
                // comes from each argument's *static* type (so a NULL float8 still
                // makes the result float8, as PostgreSQL does), falling back to the
                // runtime value for expressions whose static type is unknown.
                let rank = |d: &Datum| match d {
                    Datum::Int4(_) => 1,
                    Datum::Int8(_) => 2,
                    Datum::Numeric(_) => 3,
                    Datum::Float8(_) => 4,
                    _ => 0,
                };
                let static_rank = |t: ColType| match t {
                    ColType::Int4 => 1,
                    ColType::Int8 => 2,
                    ColType::Numeric => 3,
                    ColType::Float8 => 4,
                    _ => 0,
                };
                let mut best: Option<Datum> = None;
                let mut widest = 0u8;
                for a in args {
                    if let Some(t) = static_type(a, row) {
                        widest = widest.max(static_rank(t));
                    }
                    let v = eval_full(a, arena, params, row, hooks)?;
                    widest = widest.max(rank(&v));
                    if v.is_null() {
                        continue;
                    }
                    best = Some(match best {
                        None => v,
                        Some(cur) => {
                            let ord = compare_datums(&cur, &v)?;
                            let take_v = if name == "greatest" { ord.is_lt() } else { ord.is_gt() };
                            if take_v { v } else { cur }
                        }
                    });
                }
                let best = best.unwrap_or(Datum::Null);
                Ok(match (widest, best) {
                    (4, d) => Datum::Float8(match d {
                        Datum::Int4(x) => x as f64,
                        Datum::Int8(x) => x as f64,
                        Datum::Numeric(n) => n.to_f64(),
                        Datum::Float8(f) => f,
                        other => return Ok(other),
                    }),
                    (3, d) => match d {
                        Datum::Int4(x) => Datum::Numeric(Numeric::from_i64(x as i64, arena)?),
                        Datum::Int8(x) => Datum::Numeric(Numeric::from_i64(x, arena)?),
                        other => other,
                    },
                    (2, Datum::Int4(x)) => Datum::Int8(x as i64),
                    (_, d) => d,
                })
            }
            "nullif" => {
                arity(2)?;
                let a = eval_full(args[0], arena, params, row, hooks)?;
                let b = eval_full(args[1], arena, params, row, hooks)?;
                let (a, b) = if a.is_null() || b.is_null() {
                    (a, b)
                } else {
                    let a2 = crate::sql::eval::coerce_unknown_pub(a, &b)?;
                    let b2 = crate::sql::eval::coerce_unknown_pub(b, &a2)?;
                    (a2, b2)
                };
                if !a.is_null() && !b.is_null() && compare_datums(&a, &b)?.is_eq() {
                    Ok(Datum::Null)
                } else {
                    Ok(a)
                }
            }
            _ => unreachable!("dispatch guard admitted an unhandled name"),
        }
    })())
}
