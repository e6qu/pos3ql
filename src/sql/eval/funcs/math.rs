//! Numeric / mathematical built-ins.
//!
//! Covers absolute value and sign, rounding (`floor`/`ceil`/`round`/`trunc`),
//! roots and exponentials (`sqrt`/`exp`/`ln`/`log`/`power`), integer arithmetic
//! (`mod`/`gcd`/`lcm`/`div`), numeric-scale inspection (`scale`/`min_scale`/
//! `trim_scale`), `width_bucket`, the trigonometric family, `atan2`, `pi`, and
//! `factorial`. These share the numeric-domain helpers (`datum_numeric`,
//! `datum_f64`, `num_f64`) and the arbitrary-precision `numeric` module.

use crate::sql::array;
use crate::sql::ast::Expr;
use crate::sql::numeric::{self, Numeric};
use crate::sql::types::Datum;
use crate::{sql_err, stack_format};

use super::super::{
    arity_err, compare_datums, datum_f64, datum_numeric, eval_full, int_arg, log_domain_check,
    num_f64, overflow, sqlstate, type_mismatch, width_bucket_f64, width_bucket_numeric,
    ColumnLookup, EvalHooks, SqlError,
};

/// Handles the numeric/mathematical family. Returns `None` if `name` is not one
/// of these functions, leaving the router to keep matching.
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
        "abs"
            | "floor"
            | "ceil"
            | "ceiling"
            | "trunc"
            | "round"
            | "sign"
            | "sqrt"
            | "exp"
            | "ln"
            | "log"
            | "log10"
            | "power"
            | "pow"
            | "mod"
            | "gcd"
            | "lcm"
            | "width_bucket"
            | "div"
            | "scale"
            | "min_scale"
            | "trim_scale"
            | "cbrt"
            | "sin"
            | "cos"
            | "tan"
            | "cot"
            | "asin"
            | "acos"
            | "atan"
            | "sinh"
            | "cosh"
            | "tanh"
            | "asinh"
            | "acosh"
            | "atanh"
            | "degrees"
            | "radians"
            | "atan2"
            | "pi"
            | "factorial"
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
            "abs" => {
                arity(1)?;
                match eval_full(args[0], arena, params, row, hooks)? {
                    Datum::Null => Ok(Datum::Null),
                    Datum::Int4(v) => v
                        .checked_abs()
                        .map(Datum::Int4)
                        .ok_or_else(|| overflow("integer")),
                    Datum::Int8(v) => v
                        .checked_abs()
                        .map(Datum::Int8)
                        .ok_or_else(|| overflow("bigint")),
                    Datum::Float8(v) => Ok(Datum::Float8(v.abs())),
                    Datum::Numeric(n) => Ok(Datum::Numeric(Numeric {
                        sign: match n.sign {
                            numeric::Sign::Neg => numeric::Sign::Pos,
                            other => other,
                        },
                        ..n
                    })),
                    other => Err(type_mismatch("abs", &other)),
                }
            }
            "floor" | "ceil" | "ceiling" | "trunc" | "round" => {
                use numeric::RoundMode;
                let mode = match name {
                    "floor" => RoundMode::Floor,
                    "ceil" | "ceiling" => RoundMode::Ceil,
                    "trunc" => RoundMode::Trunc,
                    _ => RoundMode::HalfAwayZero,
                };
                // round(x, n) / trunc(x, n) adjust a numeric to n fractional digits
                // (round: half away from zero; trunc: toward zero).
                if (name == "round" || name == "trunc") && args.len() == 2 {
                    let Some(n) = int_arg(name, args, 1, arena, params, row, hooks)? else {
                        return Ok(Datum::Null);
                    };
                    let v = match eval_full(args[0], arena, params, row, hooks)? {
                        Datum::Null => return Ok(Datum::Null),
                        Datum::Numeric(v) => v,
                        Datum::Int4(x) => Numeric::from_i64(x as i64, arena)?,
                        Datum::Int8(x) => Numeric::from_i64(x, arena)?,
                        other => return Err(type_mismatch(name, &other)),
                    };
                    let result = if n >= 0 {
                        v.round_scale(n as usize, mode, arena)?
                    } else {
                        // A negative scale rounds to the left of the point: round
                        // v / 10^|n| to an integer, then scale back up.
                        let pow = Numeric::parse(stack_format!(24, "1e{}", -n).as_str(), arena)?;
                        let scaled = numeric::div(&v, &pow, arena)?.round_scale(0, mode, arena)?;
                        numeric::mul(&scaled, &pow, arena)?
                    };
                    return Ok(Datum::Numeric(result));
                }
                if star || args.len() != 1 {
                    return Err(arity_err(name, args.len()));
                }
                match eval_full(args[0], arena, params, row, hooks)? {
                    Datum::Null => Ok(Datum::Null),
                    // For an integer, floor/ceil/round/trunc are the identity; as in
                    // PostgreSQL the result type is double precision.
                    Datum::Int4(v) => Ok(Datum::Float8(v as f64)),
                    Datum::Int8(v) => Ok(Datum::Float8(v as f64)),
                    Datum::Float8(v) => Ok(Datum::Float8(match mode {
                        RoundMode::Floor => v.floor(),
                        RoundMode::Ceil => v.ceil(),
                        RoundMode::Trunc => v.trunc(),
                        RoundMode::HalfAwayZero => v.round_ties_even(),
                    })),
                    Datum::Numeric(v) => Ok(Datum::Numeric(v.round_scale(0, mode, arena)?)),
                    other => Err(type_mismatch(name, &other)),
                }
            }
            "sign" => {
                arity(1)?;
                match eval_full(args[0], arena, params, row, hooks)? {
                    Datum::Null => Ok(Datum::Null),
                    Datum::Int4(v) => Ok(Datum::Float8(v.signum() as f64)),
                    Datum::Int8(v) => Ok(Datum::Float8(v.signum() as f64)),
                    Datum::Float8(v) => Ok(Datum::Float8(if v > 0.0 {
                        1.0
                    } else if v < 0.0 {
                        -1.0
                    } else {
                        0.0
                    })),
                    Datum::Numeric(n) => {
                        let s = if n.is_zero() {
                            "0"
                        } else if n.sign == numeric::Sign::Neg {
                            "-1"
                        } else {
                            "1"
                        };
                        Ok(Datum::Numeric(Numeric::parse(s, arena)?))
                    }
                    other => Err(type_mismatch(name, &other)),
                }
            }
            "sqrt" | "exp" | "ln" => {
                arity(1)?;
                // A numeric argument keeps the numeric domain (arbitrary precision);
                // int/float arguments follow PostgreSQL and return double precision.
                let d = eval_full(args[0], arena, params, row, hooks)?;
                if d.is_null() {
                    return Ok(Datum::Null);
                }
                if let Datum::Numeric(n) = d {
                    if name == "sqrt" && n.sign == numeric::Sign::Neg && !n.is_zero() {
                        return Err(sql_err!(sqlstate::INVALID_ARGUMENT_FOR_POWER_FUNCTION, "cannot take square root of a negative number"));
                    }
                    if name == "ln" && (n.sign == numeric::Sign::Neg || n.is_zero()) {
                        return Err(sql_err!(sqlstate::INVALID_ARGUMENT_FOR_LOG, "cannot take logarithm of a non-positive number"));
                    }
                    return Ok(Datum::Numeric(match name {
                        "sqrt" => numeric::sqrt(&n, arena)?,
                        "exp" => numeric::exp(&n, arena)?,
                        _ => numeric::ln(&n, arena)?,
                    }));
                }
                let x = datum_f64(name, d)?;
                if name == "sqrt" && x < 0.0 {
                    return Err(sql_err!(sqlstate::INVALID_ARGUMENT_FOR_POWER_FUNCTION, "cannot take square root of a negative number"));
                }
                if name == "ln" && x <= 0.0 {
                    return Err(sql_err!(sqlstate::INVALID_ARGUMENT_FOR_LOG, "cannot take logarithm of a non-positive number"));
                }
                Ok(Datum::Float8(match name {
                    "sqrt" => x.sqrt(),
                    "exp" => x.exp(),
                    _ => x.ln(),
                }))
            }
            "log" | "log10" => {
                // log(x)/log10(x) are base-10; log(b, x) is base-b. A numeric
                // argument stays numeric (arbitrary precision); int/float go double.
                let two_arg = name == "log" && args.len() == 2;
                if !two_arg && args.len() != 1 {
                    return Err(arity_err(name, args.len()));
                }
                if two_arg {
                    let db = eval_full(args[0], arena, params, row, hooks)?;
                    let dv = eval_full(args[1], arena, params, row, hooks)?;
                    if db.is_null() || dv.is_null() {
                        return Ok(Datum::Null);
                    }
                    // PostgreSQL's two-argument log exists only for numeric:
                    // integers coerce implicitly, doubles do not, so a float
                    // argument is an undefined function rather than a looser
                    // float computation with a different result type.
                    if matches!(db, Datum::Float8(_)) || matches!(dv, Datum::Float8(_)) {
                        return Err(sql_err!(
                            sqlstate::UNDEFINED_FUNCTION,
                            "function log({}, {}) does not exist",
                            super::super::type_name_of_pub(&db),
                            super::super::type_name_of_pub(&dv)
                        ));
                    }
                    let b = datum_numeric(name, db, arena)?;
                    let v = datum_numeric(name, dv, arena)?;
                    log_domain_check(&v)?;
                    log_domain_check(&b)?;
                    return Ok(Datum::Numeric(numeric::logb(&b, &v, arena)?));
                }
                let d = eval_full(args[0], arena, params, row, hooks)?;
                if d.is_null() {
                    return Ok(Datum::Null);
                }
                if let Datum::Numeric(n) = d {
                    log_domain_check(&n)?;
                    return Ok(Datum::Numeric(numeric::log10(&n, arena)?));
                }
                Ok(Datum::Float8(datum_f64(name, d)?.log10()))
            }
            "power" | "pow" => {
                arity(2)?;
                let da = eval_full(args[0], arena, params, row, hooks)?;
                let db = eval_full(args[1], arena, params, row, hooks)?;
                if da.is_null() || db.is_null() {
                    return Ok(Datum::Null);
                }
                // A numeric argument keeps the numeric domain, but a float argument
                // wins (double precision is preferred), so both go to the f64 path.
                let any_numeric = matches!(da, Datum::Numeric(_)) || matches!(db, Datum::Numeric(_));
                let any_float = matches!(da, Datum::Float8(_)) || matches!(db, Datum::Float8(_));
                if any_numeric && !any_float {
                    let a = datum_numeric(name, da, arena)?;
                    let b = datum_numeric(name, db, arena)?;
                    return Ok(Datum::Numeric(numeric::pow(&a, &b, arena)?));
                }
                let (a, bb) = (datum_f64(name, da)?, datum_f64(name, db)?);
                // PostgreSQL rejects the cases whose real result is undefined,
                // rather than returning NaN/Inf as libm's powf would.
                if a < 0.0 && bb.fract() != 0.0 {
                    return Err(sql_err!(
                        sqlstate::INVALID_ARGUMENT_FOR_POWER_FUNCTION,
                        "a negative number raised to a non-integer power yields a complex result"
                    ));
                }
                if a == 0.0 && bb < 0.0 {
                    return Err(sql_err!(sqlstate::INVALID_ARGUMENT_FOR_POWER_FUNCTION, "zero raised to a negative power is undefined"));
                }
                Ok(Datum::Float8(a.powf(bb)))
            }
            "mod" => {
                arity(2)?;
                let a = eval_full(args[0], arena, params, row, hooks)?;
                let b = eval_full(args[1], arena, params, row, hooks)?;
                if a.is_null() || b.is_null() {
                    return Ok(Datum::Null);
                }
                // A numeric operand keeps the numeric domain (matching the `%`
                // operator); mixed integer widths pick the wider integer type.
                if matches!(a, Datum::Numeric(_)) || matches!(b, Datum::Numeric(_)) {
                    let x = datum_numeric(name, a, arena)?;
                    let y = datum_numeric(name, b, arena)?;
                    return Ok(Datum::Numeric(numeric::rem(&x, &y, arena)?));
                }
                let (x, y, wide) = match (a, b) {
                    (Datum::Int4(x), Datum::Int4(y)) => (x as i64, y as i64, false),
                    (Datum::Int4(x), Datum::Int8(y)) => (x as i64, y, true),
                    (Datum::Int8(x), Datum::Int4(y)) => (x, y as i64, true),
                    (Datum::Int8(x), Datum::Int8(y)) => (x, y, true),
                    (other, _) => return Err(type_mismatch(name, &other)),
                };
                if y == 0 {
                    return Err(sql_err!(sqlstate::DIVISION_BY_ZERO, "division by zero"));
                }
                let r = x % y;
                Ok(if wide { Datum::Int8(r) } else { Datum::Int4(r as i32) })
            }
            "gcd" | "lcm" => {
                arity(2)?;
                let a = eval_full(args[0], arena, params, row, hooks)?;
                let b = eval_full(args[1], arena, params, row, hooks)?;
                if a.is_null() || b.is_null() {
                    return Ok(Datum::Null);
                }
                let (x, y, wide) = match (a, b) {
                    (Datum::Int4(x), Datum::Int4(y)) => (x as i64, y as i64, false),
                    (Datum::Int4(x), Datum::Int8(y)) => (x as i64, y, true),
                    (Datum::Int8(x), Datum::Int4(y)) => (x, y as i64, true),
                    (Datum::Int8(x), Datum::Int8(y)) => (x, y, true),
                    (other, _) => return Err(type_mismatch(name, &other)),
                };
                let range = || sql_err!(sqlstate::NUMERIC_OUT_OF_RANGE, "{} result is out of range", name);
                let (gx, gy) = (x.unsigned_abs(), y.unsigned_abs());
                let mut g = gx;
                let mut h = gy;
                while h != 0 {
                    let t = g % h;
                    g = h;
                    h = t;
                }
                let out: i64 = if name == "gcd" {
                    i64::try_from(g).map_err(|_| range())?
                } else {
                    // lcm is 0 when the gcd is 0 (both inputs 0); otherwise |a/gcd*b|.
                    match gx.checked_div(g) {
                        None => 0,
                        Some(q) => {
                            let l = q.checked_mul(gy).ok_or_else(range)?;
                            i64::try_from(l).map_err(|_| range())?
                        }
                    }
                };
                Ok(if wide {
                    Datum::Int8(out)
                } else {
                    Datum::Int4(i32::try_from(out).map_err(|_| range())?)
                })
            }
            "width_bucket" => {
                // 2-arg form: width_bucket(operand, thresholds[]) — the bucket index
                // by binary search over an ascending array of bucket lower bounds
                // (0 below the first bound), matching PostgreSQL's width_bucket_array.
                if args.len() == 2 {
                    let operand = eval_full(args[0], arena, params, row, hooks)?;
                    let thresholds = eval_full(args[1], arena, params, row, hooks)?;
                    if operand.is_null() {
                        return Ok(Datum::Null);
                    }
                    let Datum::Array { element, raw } = thresholds else {
                        return Err(sql_err!(
                            sqlstate::DATATYPE_MISMATCH,
                            "width_bucket: thresholds argument must be an array"
                        ));
                    };
                    let (mut left, mut right) = (0usize, array::len(raw));
                    while left < right {
                        let mid = left + (right - left) / 2;
                        let bound = array::get(raw, element, mid).unwrap_or(Datum::Null);
                        if compare_datums(&operand, &bound)?.is_lt() {
                            right = mid;
                        } else {
                            left = mid + 1;
                        }
                    }
                    return Ok(Datum::Int4(left as i32));
                }
                // 4-arg form: which of `count` equal-width buckets over [low, high]
                // the operand falls in (0 below, count+1 at/above). Numeric args use
                // exact numeric arithmetic; a float argument uses double precision.
                arity(4)?;
                let operator = eval_full(args[0], arena, params, row, hooks)?;
                let lo = eval_full(args[1], arena, params, row, hooks)?;
                let hi = eval_full(args[2], arena, params, row, hooks)?;
                let Some(cnt) = int_arg(name, args, 3, arena, params, row, hooks)? else {
                    return Ok(Datum::Null);
                };
                if operator.is_null() || lo.is_null() || hi.is_null() {
                    return Ok(Datum::Null);
                }
                if cnt <= 0 {
                    return Err(sql_err!(sqlstate::INVALID_ARGUMENT_FOR_WIDTH_BUCKET, "count must be greater than zero"));
                }
                let any_float = matches!(operator, Datum::Float8(_))
                    || matches!(lo, Datum::Float8(_))
                    || matches!(hi, Datum::Float8(_));
                if any_float {
                    let (o, l, h) = (datum_f64(name, operator)?, datum_f64(name, lo)?, datum_f64(name, hi)?);
                    if l == h {
                        return Err(sql_err!(sqlstate::NULL_VALUE_NOT_ALLOWED, "lower and upper bounds cannot be equal"));
                    }
                    let b = width_bucket_f64(o, l, h, cnt);
                    return Ok(Datum::Int4(b));
                }
                let (o, l, h) = (
                    datum_numeric(name, operator, arena)?,
                    datum_numeric(name, lo, arena)?,
                    datum_numeric(name, hi, arena)?,
                );
                Ok(Datum::Int4(width_bucket_numeric(&o, &l, &h, cnt, arena)?))
            }
            "div" => {
                // Integer quotient trunc(y/x) in the numeric domain (integer args
                // are promoted to numeric, as PostgreSQL's `div(numeric,numeric)`).
                arity(2)?;
                let a = eval_full(args[0], arena, params, row, hooks)?;
                let b = eval_full(args[1], arena, params, row, hooks)?;
                if a.is_null() || b.is_null() {
                    return Ok(Datum::Null);
                }
                let (x, y) = (datum_numeric(name, a, arena)?, datum_numeric(name, b, arena)?);
                Ok(Datum::Numeric(numeric::trunc_div(&x, &y, arena)?))
            }
            "scale" => {
                arity(1)?;
                match eval_full(args[0], arena, params, row, hooks)? {
                    Datum::Null => Ok(Datum::Null),
                    Datum::Numeric(n) => Ok(Datum::Int4(n.dscale as i32)),
                    Datum::Int4(_) | Datum::Int8(_) => Ok(Datum::Int4(0)),
                    other => Err(type_mismatch(name, &other)),
                }
            }
            "min_scale" => {
                arity(1)?;
                match eval_full(args[0], arena, params, row, hooks)? {
                    Datum::Null => Ok(Datum::Null),
                    Datum::Numeric(n) => Ok(Datum::Int4(n.min_scale() as i32)),
                    Datum::Int4(_) | Datum::Int8(_) => Ok(Datum::Int4(0)),
                    other => Err(type_mismatch(name, &other)),
                }
            }
            "trim_scale" => {
                arity(1)?;
                match eval_full(args[0], arena, params, row, hooks)? {
                    Datum::Null => Ok(Datum::Null),
                    Datum::Numeric(n) => Ok(Datum::Numeric(n.round_scale(
                        n.min_scale() as usize,
                        numeric::RoundMode::Trunc,
                        arena,
                    )?)),
                    d @ (Datum::Int4(_) | Datum::Int8(_)) => Ok(d),
                    other => Err(type_mismatch(name, &other)),
                }
            }
            "cbrt" | "sin" | "cos" | "tan" | "cot" | "asin" | "acos" | "atan" | "sinh" | "cosh"
            | "tanh" | "asinh" | "acosh" | "atanh" | "degrees" | "radians" => {
                arity(1)?;
                let Some(x) = num_f64(name, args, 0, arena, params, row, hooks)? else {
                    return Ok(Datum::Null);
                };
                Ok(Datum::Float8(match name {
                    "cbrt" => x.cbrt(),
                    "sin" => x.sin(),
                    "cos" => x.cos(),
                    "tan" => x.tan(),
                    "cot" => 1.0 / x.tan(),
                    "asin" => x.asin(),
                    "acos" => x.acos(),
                    "atan" => x.atan(),
                    "sinh" => x.sinh(),
                    "cosh" => x.cosh(),
                    "tanh" => x.tanh(),
                    "asinh" => x.asinh(),
                    "acosh" => x.acosh(),
                    "atanh" => x.atanh(),
                    "degrees" => x.to_degrees(),
                    _ => x.to_radians(),
                }))
            }
            "atan2" => {
                arity(2)?;
                let (Some(a), Some(bb)) = (
                    num_f64(name, args, 0, arena, params, row, hooks)?,
                    num_f64(name, args, 1, arena, params, row, hooks)?,
                ) else {
                    return Ok(Datum::Null);
                };
                Ok(Datum::Float8(a.atan2(bb)))
            }
            "pi" => {
                arity(0)?;
                Ok(Datum::Float8(core::f64::consts::PI))
            }
            "factorial" => {
                arity(1)?;
                let Some(n) = int_arg(name, args, 0, arena, params, row, hooks)? else {
                    return Ok(Datum::Null);
                };
                if n < 0 {
                    return Err(sql_err!(sqlstate::NUMERIC_OUT_OF_RANGE, "factorial of a negative number is undefined"));
                }
                // n! as an exact numeric; a too-large product exhausts the arena and
                // errors loudly, matching PostgreSQL's numeric overflow.
                let mut acc = Numeric::from_i64(1, arena)?;
                let mut k = 2i64;
                while k <= n {
                    acc = numeric::mul(&acc, &Numeric::from_i64(k, arena)?, arena)?;
                    k += 1;
                }
                Ok(Datum::Numeric(acc))
            }
            _ => unreachable!("dispatch guard admitted an unhandled name"),
        }
    })())
}
