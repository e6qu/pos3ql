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
    ColumnLookup, EvalHooks, SqlError, arity_err, eval_full, sqlstate, static_type,
};

/// The type an unknown-type literal argument of `coalesce`, `greatest` or
/// `least` takes: the arguments' common numeric type, or else the first typed
/// argument's type. None leaves the literal as text.
pub(crate) fn literal_argument_type<'e, 'a: 'e>(
    args: &'e [&'e Expr<'a>],
    infer: &dyn Fn(&'e Expr<'a>) -> Option<ColType>,
) -> Option<ColType> {
    let rank = |t: ColType| match t {
        ColType::Int2 => 1,
        ColType::Int4 => 2,
        ColType::Int8 => 3,
        ColType::Numeric => 4,
        ColType::Float4 => 5,
        ColType::Float8 => 6,
        _ => 0,
    };
    let mut first = None;
    let mut widest: Option<ColType> = None;
    for argument in args {
        if matches!(argument, Expr::Str(_) | Expr::Null) {
            continue;
        }
        let Some(ctype) = infer(argument) else {
            continue;
        };
        first.get_or_insert(ctype);
        if rank(ctype) > widest.map_or(0, rank) {
            widest = Some(ctype);
        }
    }
    widest.or(first).filter(|ctype| {
        !matches!(
            ctype,
            ColType::Text | ColType::Varchar | ColType::Bpchar | ColType::Name
        )
    })
}

/// Evaluates one argument of `coalesce`, `greatest` or `least`, converting
/// an unknown-type literal to the arguments' type as PostgreSQL's analysis does.
fn eval_argument<'a>(
    argument: &Expr<'a>,
    literal_type: Option<ColType>,
    arena: &'a crate::mem::arena::Arena,
    params: &[Datum<'a>],
    row: &impl ColumnLookup<'a>,
    hooks: &EvalHooks<'_, 'a>,
) -> Result<Datum<'a>, SqlError> {
    let value = eval_full(argument, arena, params, row, hooks)?;
    match (argument, literal_type) {
        (Expr::Str(text), Some(target)) => {
            crate::sql::eval::cast_to(Datum::Text(text), target, arena)
                .map_err(|error| crate::sql::error_position::at_subject(error, text))
        }
        _ => Ok(value),
    }
}

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
        "coalesce"
            | "num_nonnulls"
            | "num_nulls"
            | "greatest"
            | "least"
            | "nullif"
            | "booleq"
            | "boolne"
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
            "booleq" | "boolne" => {
                arity(2)?;
                let left = eval_full(args[0], arena, params, row, hooks)?;
                let right = eval_full(args[1], arena, params, row, hooks)?;
                match (left, right) {
                    (Datum::Null, _) | (_, Datum::Null) => Ok(Datum::Null),
                    (Datum::Bool(left), Datum::Bool(right)) => {
                        Ok(Datum::Bool((left == right) == (name == "booleq")))
                    }
                    _ => Err(arity_err(name, 2)),
                }
            }
            "coalesce" => {
                let literal_type =
                    literal_argument_type(args, &|argument| static_type(argument, row));
                for arg in args {
                    let v = eval_argument(arg, literal_type, arena, params, row, hooks)?;
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
                    Datum::Int2(_) | Datum::Int4(_) => 1,
                    Datum::Int8(_) => 2,
                    Datum::Numeric(_) => 3,
                    Datum::Float4(_) => 4,
                    Datum::Float8(_) => 5,
                    _ => 0,
                };
                let static_rank = |t: ColType| match t {
                    ColType::Int2 | ColType::Int4 => 1,
                    ColType::Int8 => 2,
                    ColType::Numeric => 3,
                    ColType::Float4 => 4,
                    ColType::Float8 => 5,
                    _ => 0,
                };
                let mut best: Option<Datum> = None;
                let mut widest = 0u8;
                let literal_type =
                    literal_argument_type(args, &|argument| static_type(argument, row));
                for a in args {
                    if let Some(t) = static_type(a, row) {
                        widest = widest.max(static_rank(t));
                    }
                    let v = eval_argument(a, literal_type, arena, params, row, hooks)?;
                    widest = widest.max(rank(&v));
                    if v.is_null() {
                        continue;
                    }
                    best = Some(match best {
                        None => v,
                        Some(cur) => {
                            let collation = crate::sql::eval::resolve_comparison_collation(
                                a,
                                args[0],
                                row,
                                hooks.catalog,
                            )?;
                            let ord = crate::sql::eval::compare_datums_with_catalog(
                                collation,
                                hooks.catalog,
                                &cur,
                                &v,
                            )?;
                            let take_v = if name == "greatest" {
                                ord.is_lt()
                            } else {
                                ord.is_gt()
                            };
                            if take_v { v } else { cur }
                        }
                    });
                }
                let best = best.unwrap_or(Datum::Null);
                Ok(match (widest, best) {
                    (5, d) => Datum::Float8(match d {
                        Datum::Int2(x) => x as f64,
                        Datum::Int4(x) => x as f64,
                        Datum::Int8(x) => x as f64,
                        Datum::Numeric(n) => n.to_f64(),
                        Datum::Float4(f) => f64::from(f),
                        Datum::Float8(f) => f,
                        other => return Ok(other),
                    }),
                    // real wins over int and numeric, so the result widens to
                    // real (double precision would need a float8 present).
                    (4, d) => Datum::Float4(match d {
                        Datum::Int2(x) => x as f32,
                        Datum::Int4(x) => x as f32,
                        Datum::Int8(x) => x as f32,
                        Datum::Numeric(n) => n.to_f64() as f32,
                        Datum::Float4(f) => f,
                        other => return Ok(other),
                    }),
                    (3, d) => match d {
                        Datum::Int2(x) => Datum::Numeric(Numeric::from_i64(x as i64, arena)?),
                        Datum::Int4(x) => Datum::Numeric(Numeric::from_i64(x as i64, arena)?),
                        Datum::Int8(x) => Datum::Numeric(Numeric::from_i64(x, arena)?),
                        other => other,
                    },
                    (2, Datum::Int2(x)) => Datum::Int8(x as i64),
                    (2, Datum::Int4(x)) => Datum::Int8(x as i64),
                    (_, d) => d,
                })
            }
            "nullif" => {
                arity(2)?;
                let collation = crate::sql::eval::resolve_comparison_collation(
                    args[0],
                    args[1],
                    row,
                    hooks.catalog,
                )?;
                let left = super::super::materialize_named_composite(
                    eval_full(args[0], arena, params, row, hooks)?,
                    hooks,
                    arena,
                )?;
                let right = super::super::materialize_named_composite(
                    eval_full(args[1], arena, params, row, hooks)?,
                    hooks,
                    arena,
                )?;
                let equality = super::super::eval_binary_values(
                    super::super::BinaryExpression {
                        operator: crate::sql::ast::BinaryOp::Eq,
                        left: args[0],
                        right: args[1],
                        search_catalog: true,
                        nulls: super::super::OperatorNulls::ReturnNull,
                    },
                    left,
                    right,
                    Some(collation),
                    arena,
                    row,
                    hooks,
                )?;
                match equality.value {
                    Datum::Bool(true) => Ok(Datum::Null),
                    Datum::Bool(false) | Datum::Null => Ok(equality.first_argument),
                    _ => Err(sql_err!(
                        sqlstate::DATATYPE_MISMATCH,
                        "NULLIF requires = operator to yield boolean"
                    )),
                }
            }
            _ => unreachable!("dispatch guard admitted an unhandled name"),
        }
    })())
}
