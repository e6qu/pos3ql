//! Numeric / mathematical built-ins.
//!
//! Covers absolute value and sign, rounding (`floor`/`ceil`/`round`/`trunc`),
//! roots and exponentials (`sqrt`/`exp`/`ln`/`log`/`power`), integer arithmetic
//! (`mod`/`gcd`/`lcm`/`div`), numeric-scale inspection (`scale`/`min_scale`/
//! `trim_scale`), `width_bucket`, trigonometric and special functions, random
//! distributions, `pi`, and `factorial`. These share the numeric-domain
//! helpers and the arbitrary-precision `numeric` module.

use crate::sql::array;
use crate::sql::ast::Expr;
use crate::sql::numeric::{self, Numeric};
use crate::sql::types::Datum;
use crate::{sql_err, stack_format};

use super::super::{
    ColumnLookup, EvalHooks, SqlError, arena_full, arity_err, compare_datums, datum_f64,
    datum_numeric, eval_full, int_arg, log_domain_check, num_f64, overflow, sqlstate,
    type_mismatch, width_bucket_f64, width_bucket_numeric,
};

unsafe extern "C" {
    #[link_name = "erf"]
    fn libc_erf(value: f64) -> f64;
    #[link_name = "erfc"]
    fn libc_erfc(value: f64) -> f64;
    #[link_name = "tgamma"]
    fn libc_tgamma(value: f64) -> f64;
    #[link_name = "lgamma"]
    fn libc_lgamma(value: f64) -> f64;
}

fn input_out_of_range() -> SqlError {
    sql_err!(sqlstate::NUMERIC_OUT_OF_RANGE, "input is out of range")
}

fn special_f64(name: &str, value: f64) -> Result<f64, SqlError> {
    // These are C99 libm calls on every supported target, just as in
    // PostgreSQL.  They neither retain the pointer nor access Rust memory.
    let result = unsafe {
        match name {
            "erf" => libc_erf(value),
            "erfc" => libc_erfc(value),
            "gamma" => libc_tgamma(value),
            "lgamma" => libc_lgamma(value),
            _ => unreachable!("special-function router admitted {name}"),
        }
    };
    match name {
        "gamma" if value == f64::NEG_INFINITY => Err(sql_err!(
            sqlstate::NUMERIC_OUT_OF_RANGE,
            "value out of range: overflow"
        )),
        "gamma" if value.is_finite() && (result.is_infinite() || result.is_nan()) => Err(sql_err!(
            sqlstate::NUMERIC_OUT_OF_RANGE,
            "value out of range: overflow"
        )),
        "gamma" if value.is_finite() && result == 0.0 => Err(sql_err!(
            sqlstate::NUMERIC_OUT_OF_RANGE,
            "value out of range: underflow"
        )),
        "lgamma" if result.is_infinite() && value.is_finite() => Err(sql_err!(
            sqlstate::NUMERIC_OUT_OF_RANGE,
            "value out of range: overflow"
        )),
        _ => Ok(result),
    }
}

fn degree_constants() -> (f64, f64, f64, f64, f64, f64, f64) {
    let radians = core::f64::consts::PI / 180.0;
    let sin_30 = (30.0 * radians).sin();
    let one_minus_cos_60 = 1.0 - (60.0 * radians).cos();
    let asin_half = 0.5_f64.asin();
    let acos_half = 0.5_f64.acos();
    let atan_one = 1.0_f64.atan();
    let sin_45 = sind_q1(45.0, sin_30, one_minus_cos_60);
    let cos_45 = cosd_q1(45.0, sin_30, one_minus_cos_60);
    (
        sin_30,
        one_minus_cos_60,
        asin_half,
        acos_half,
        atan_one,
        sin_45 / cos_45,
        cos_45 / sin_45,
    )
}

fn sind_q1(value: f64, sin_30: f64, one_minus_cos_60: f64) -> f64 {
    if value <= 30.0 {
        (value.to_radians().sin() / sin_30) / 2.0
    } else {
        let complement = 90.0 - value;
        1.0 - ((1.0 - complement.to_radians().cos()) / one_minus_cos_60) / 2.0
    }
}

fn cosd_q1(value: f64, sin_30: f64, one_minus_cos_60: f64) -> f64 {
    if value <= 60.0 {
        1.0 - ((1.0 - value.to_radians().cos()) / one_minus_cos_60) / 2.0
    } else {
        ((90.0 - value).to_radians().sin() / sin_30) / 2.0
    }
}

fn degree_trig(name: &str, mut value: f64) -> Result<f64, SqlError> {
    if value.is_nan() {
        return Ok(value);
    }
    let (sin_30, one_minus_cos_60, asin_half, acos_half, atan_one, tan_45, cot_45) =
        degree_constants();
    let asind_q1 = |x: f64| {
        if x <= 0.5 {
            (x.asin() / asin_half) * 30.0
        } else {
            90.0 - (x.acos() / acos_half) * 60.0
        }
    };
    let acosd_q1 = |x: f64| {
        if x <= 0.5 {
            90.0 - (x.asin() / asin_half) * 30.0
        } else {
            (x.acos() / acos_half) * 60.0
        }
    };
    match name {
        "asind" | "acosd" if !(-1.0..=1.0).contains(&value) => Err(input_out_of_range()),
        "asind" => Ok(if value >= 0.0 {
            asind_q1(value)
        } else {
            -asind_q1(-value)
        }),
        "acosd" => Ok(if value >= 0.0 {
            acosd_q1(value)
        } else {
            90.0 + asind_q1(-value)
        }),
        "atand" => Ok((value.atan() / atan_one) * 45.0),
        "sind" | "cosd" | "tand" | "cotd" => {
            if value.is_infinite() {
                return Err(input_out_of_range());
            }
            value %= 360.0;
            let mut sign = 1.0;
            if value < 0.0 {
                value = -value;
                if name != "cosd" {
                    sign = -sign;
                }
            }
            if value > 180.0 {
                value = 360.0 - value;
                if name == "sind" || name == "tand" || name == "cotd" {
                    sign = -sign;
                }
            }
            if value > 90.0 {
                value = 180.0 - value;
                if name == "cosd" || name == "tand" || name == "cotd" {
                    sign = -sign;
                }
            }
            let sin = sind_q1(value, sin_30, one_minus_cos_60);
            let cos = cosd_q1(value, sin_30, one_minus_cos_60);
            let result = match name {
                "sind" => sign * sin,
                "cosd" => sign * cos,
                "tand" => sign * (sin / cos) / tan_45,
                _ => sign * (cos / sin) / cot_45,
            };
            if (name == "tand" || name == "cotd") && result == 0.0 {
                Ok(0.0)
            } else {
                Ok(result)
            }
        }
        _ => unreachable!("degree-function router admitted {name}"),
    }
}

fn random_numeric<'a>(
    min: &Numeric<'_>,
    max: &Numeric<'_>,
    arena: &'a crate::mem::arena::Arena,
) -> Result<Numeric<'a>, SqlError> {
    if min.is_nan() {
        return Err(sql_err!(
            sqlstate::INVALID_PARAMETER_VALUE,
            "lower bound cannot be NaN"
        ));
    }
    if min.is_infinite() {
        return Err(sql_err!(
            sqlstate::INVALID_PARAMETER_VALUE,
            "lower bound cannot be infinity"
        ));
    }
    if max.is_nan() {
        return Err(sql_err!(
            sqlstate::INVALID_PARAMETER_VALUE,
            "upper bound cannot be NaN"
        ));
    }
    if max.is_infinite() {
        return Err(sql_err!(
            sqlstate::INVALID_PARAMETER_VALUE,
            "upper bound cannot be infinity"
        ));
    }
    let rscale = min.dscale.max(max.dscale);
    let length = numeric::sub(max, min, arena)?;
    if length.sign == numeric::Sign::Neg {
        return Err(sql_err!(
            sqlstate::INVALID_PARAMETER_VALUE,
            "lower bound must be less than or equal to upper bound"
        ));
    }
    if length.is_zero() {
        let adjusted = Numeric {
            dscale: rscale,
            ..*min
        };
        return numeric::add(&Numeric::ZERO, &adjusted, arena);
    }

    let result_digits = i32::from(length.weight)
        + 1
        + (i32::from(rscale) + numeric::DEC_DIGITS as i32 - 1) / numeric::DEC_DIGITS as i32;
    if result_digits <= 0 || result_digits as usize > numeric::MAX_NDIGITS {
        return Err(overflow("numeric"));
    }
    let result_digits = result_digits as usize;
    let partial_places = usize::from(rscale).div_ceil(numeric::DEC_DIGITS) * numeric::DEC_DIGITS
        - usize::from(rscale);
    let partial_multiple = 10_u64.pow(partial_places as u32);

    let mut prefix = length.digit(0) as u64;
    let mut prefix_digits = 1usize;
    while prefix_digits < result_digits && prefix_digits < 4 {
        prefix *= numeric::NBASE as u64;
        if prefix_digits < length.ndigits() {
            prefix += length.digit(prefix_digits) as u64;
        }
        prefix_digits += 1;
    }

    let mut raw = [0_u8; numeric::MAX_NDIGITS * 2];
    loop {
        let mut random = if prefix_digits == result_digits && partial_multiple != 1 {
            crate::sql::guc::active_random_u64_range(0, prefix / partial_multiple)?
                * partial_multiple
        } else {
            crate::sql::guc::active_random_u64_range(0, prefix)?
        };
        for index in (0..prefix_digits).rev() {
            let digit = (random % numeric::NBASE as u64) as i16;
            raw[index * 2..index * 2 + 2].copy_from_slice(&digit.to_le_bytes());
            random /= numeric::NBASE as u64;
        }
        let whole_digits = result_digits - usize::from(partial_multiple != 1);
        let mut index = prefix_digits;
        while index + 4 <= whole_digits {
            random = crate::sql::guc::active_random_u64_range(
                0,
                numeric::NBASE as u64
                    * numeric::NBASE as u64
                    * numeric::NBASE as u64
                    * numeric::NBASE as u64
                    - 1,
            )?;
            for _ in 0..4 {
                let digit = (random % numeric::NBASE as u64) as i16;
                raw[index * 2..index * 2 + 2].copy_from_slice(&digit.to_le_bytes());
                random /= numeric::NBASE as u64;
                index += 1;
            }
        }
        while index < whole_digits {
            let digit =
                crate::sql::guc::active_random_u64_range(0, numeric::NBASE as u64 - 1)? as i16;
            raw[index * 2..index * 2 + 2].copy_from_slice(&digit.to_le_bytes());
            index += 1;
        }
        if index < result_digits {
            let digit = (crate::sql::guc::active_random_u64_range(
                0,
                numeric::NBASE as u64 / partial_multiple - 1,
            )? * partial_multiple) as i16;
            raw[index * 2..index * 2 + 2].copy_from_slice(&digit.to_le_bytes());
        }

        let mut first = 0usize;
        while first < result_digits && i16::from_le_bytes([raw[first * 2], raw[first * 2 + 1]]) == 0
        {
            first += 1;
        }
        let mut last = result_digits;
        while last > first
            && i16::from_le_bytes([raw[(last - 1) * 2], raw[(last - 1) * 2 + 1]]) == 0
        {
            last -= 1;
        }
        let digits = arena
            .alloc_slice_copy(&raw[first * 2..last * 2])
            .map_err(|_| arena_full())?;
        let candidate = Numeric {
            sign: numeric::Sign::Pos,
            weight: length.weight - first as i16,
            dscale: rscale,
            digits,
        };
        if numeric::compare(&candidate, &length) != core::cmp::Ordering::Greater {
            return numeric::add(&candidate, min, arena);
        }
    }
}

/// floor/ceil/round/trunc on an f64, shared by the float8 and (widened) real
/// arms. PostgreSQL's `round(double precision)` ties to even.
fn round_f64(v: f64, mode: numeric::RoundMode) -> f64 {
    use numeric::RoundMode;
    match mode {
        RoundMode::Floor => v.floor(),
        RoundMode::Ceil => v.ceil(),
        RoundMode::Trunc => v.trunc(),
        RoundMode::HalfAwayZero => v.round_ties_even(),
    }
}

/// `sign(double precision)`: +1/-1/0, with 0 for both signed zeroes (f32/f64
/// `signum` returns ±1 for ±0, which is wrong here).
fn float_sign(v: f64) -> f64 {
    if v > 0.0 {
        1.0
    } else if v < 0.0 {
        -1.0
    } else {
        0.0
    }
}

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
            | "asind"
            | "acos"
            | "acosd"
            | "atan"
            | "atand"
            | "sinh"
            | "cosh"
            | "tanh"
            | "asinh"
            | "acosh"
            | "atanh"
            | "degrees"
            | "radians"
            | "atan2"
            | "atan2d"
            | "sind"
            | "cosd"
            | "tand"
            | "cotd"
            | "erf"
            | "erfc"
            | "gamma"
            | "lgamma"
            | "pi"
            | "random"
            | "random_normal"
            | "setseed"
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
                    Datum::Int2(v) => v
                        .checked_abs()
                        .map(Datum::Int2)
                        .ok_or_else(|| overflow("smallint")),
                    Datum::Int4(v) => v
                        .checked_abs()
                        .map(Datum::Int4)
                        .ok_or_else(|| overflow("integer")),
                    Datum::Int8(v) => v
                        .checked_abs()
                        .map(Datum::Int8)
                        .ok_or_else(|| overflow("bigint")),
                    Datum::Float4(v) => Ok(Datum::Float4(v.abs())),
                    Datum::Float8(v) => Ok(Datum::Float8(v.abs())),
                    Datum::Numeric(n) => Ok(Datum::Numeric(Numeric {
                        sign: match n.sign {
                            numeric::Sign::Neg => numeric::Sign::Pos,
                            numeric::Sign::NegInf => numeric::Sign::PosInf,
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
                        Datum::Int2(x) => Numeric::from_i64(x as i64, arena)?,
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
                    Datum::Int2(v) => Ok(Datum::Float8(v as f64)),
                    Datum::Int4(v) => Ok(Datum::Float8(v as f64)),
                    Datum::Int8(v) => Ok(Datum::Float8(v as f64)),
                    // real has no dedicated overload; it widens to double
                    // precision, like the integer arms above.
                    Datum::Float4(v) => Ok(Datum::Float8(round_f64(f64::from(v), mode))),
                    Datum::Float8(v) => Ok(Datum::Float8(round_f64(v, mode))),
                    Datum::Numeric(v) => Ok(Datum::Numeric(v.round_scale(0, mode, arena)?)),
                    // trunc(macaddr)/trunc(macaddr8): zero the trailing bytes
                    // (the last 3 / last 5), keeping the OUI.
                    Datum::Macaddr(mut b) if name == "trunc" => {
                        b[3..].fill(0);
                        Ok(Datum::Macaddr(b))
                    }
                    Datum::Macaddr8(mut b) if name == "trunc" => {
                        b[3..].fill(0);
                        Ok(Datum::Macaddr8(b))
                    }
                    other => Err(type_mismatch(name, &other)),
                }
            }
            "sign" => {
                arity(1)?;
                match eval_full(args[0], arena, params, row, hooks)? {
                    Datum::Null => Ok(Datum::Null),
                    Datum::Int2(v) => Ok(Datum::Float8(v.signum() as f64)),
                    Datum::Int4(v) => Ok(Datum::Float8(v.signum() as f64)),
                    Datum::Int8(v) => Ok(Datum::Float8(v.signum() as f64)),
                    Datum::Float4(v) => Ok(Datum::Float8(float_sign(f64::from(v)))),
                    Datum::Float8(v) => Ok(Datum::Float8(float_sign(v))),
                    Datum::Numeric(n) => {
                        if n.is_nan() {
                            return Ok(Datum::Numeric(Numeric::NAN));
                        }
                        let s = if n.is_zero() {
                            "0"
                        } else if n.is_negative() {
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
                    if name == "sqrt" && n.is_negative() && !n.is_zero() {
                        return Err(sql_err!(
                            sqlstate::INVALID_ARGUMENT_FOR_POWER_FUNCTION,
                            "cannot take square root of a negative number"
                        ));
                    }
                    if name == "ln" && (n.is_negative() || n.is_zero()) {
                        return Err(sql_err!(
                            sqlstate::INVALID_ARGUMENT_FOR_LOG,
                            "cannot take logarithm of a non-positive number"
                        ));
                    }
                    return Ok(Datum::Numeric(match name {
                        "sqrt" => numeric::sqrt(&n, arena)?,
                        "exp" => numeric::exp(&n, arena)?,
                        _ => numeric::ln(&n, arena)?,
                    }));
                }
                let x = datum_f64(name, d)?;
                if name == "sqrt" && x < 0.0 {
                    return Err(sql_err!(
                        sqlstate::INVALID_ARGUMENT_FOR_POWER_FUNCTION,
                        "cannot take square root of a negative number"
                    ));
                }
                if name == "ln" && x <= 0.0 {
                    return Err(sql_err!(
                        sqlstate::INVALID_ARGUMENT_FOR_LOG,
                        "cannot take logarithm of a non-positive number"
                    ));
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
                let any_numeric =
                    matches!(da, Datum::Numeric(_)) || matches!(db, Datum::Numeric(_));
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
                    return Err(sql_err!(
                        sqlstate::INVALID_ARGUMENT_FOR_POWER_FUNCTION,
                        "zero raised to a negative power is undefined"
                    ));
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
                let int_of = |d: &Datum| match d {
                    Datum::Int2(x) => Some((i64::from(*x), 2u8)),
                    Datum::Int4(x) => Some((i64::from(*x), 4)),
                    Datum::Int8(x) => Some((*x, 8)),
                    _ => None,
                };
                let (Some((x, wl)), Some((y, wr))) = (int_of(&a), int_of(&b)) else {
                    return Err(type_mismatch(name, &a));
                };
                if y == 0 {
                    return Err(sql_err!(sqlstate::DIVISION_BY_ZERO, "division by zero"));
                }
                let r = x % y;
                Ok(match wl.max(wr) {
                    2 => Datum::Int2(r as i16),
                    4 => Datum::Int4(r as i32),
                    _ => Datum::Int8(r),
                })
            }
            "gcd" | "lcm" => {
                arity(2)?;
                let a = eval_full(args[0], arena, params, row, hooks)?;
                let b = eval_full(args[1], arena, params, row, hooks)?;
                if a.is_null() || b.is_null() {
                    return Ok(Datum::Null);
                }
                let (x, y, wide) = match (a, b) {
                    // PostgreSQL resolves gcd/lcm over int4 and int8 only; a
                    // smallint argument matches both promotions equally and
                    // the call is ambiguous.
                    (Datum::Int2(_), _) | (_, Datum::Int2(_)) => {
                        return Err(sql_err!(
                            sqlstate::AMBIGUOUS_FUNCTION,
                            "function {}(smallint, smallint) is not unique",
                            name
                        ));
                    }
                    (Datum::Int4(x), Datum::Int4(y)) => (x as i64, y as i64, false),
                    (Datum::Int4(x), Datum::Int8(y)) => (x as i64, y, true),
                    (Datum::Int8(x), Datum::Int4(y)) => (x, y as i64, true),
                    (Datum::Int8(x), Datum::Int8(y)) => (x, y, true),
                    (other, _) => return Err(type_mismatch(name, &other)),
                };
                let range = || {
                    sql_err!(
                        sqlstate::NUMERIC_OUT_OF_RANGE,
                        "{} result is out of range",
                        name
                    )
                };
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
                    return Err(sql_err!(
                        sqlstate::INVALID_ARGUMENT_FOR_WIDTH_BUCKET,
                        "count must be greater than zero"
                    ));
                }
                let any_float = matches!(operator, Datum::Float8(_))
                    || matches!(lo, Datum::Float8(_))
                    || matches!(hi, Datum::Float8(_));
                if any_float {
                    let (o, l, h) = (
                        datum_f64(name, operator)?,
                        datum_f64(name, lo)?,
                        datum_f64(name, hi)?,
                    );
                    if l == h {
                        return Err(sql_err!(
                            sqlstate::NULL_VALUE_NOT_ALLOWED,
                            "lower and upper bounds cannot be equal"
                        ));
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
                let (x, y) = (
                    datum_numeric(name, a, arena)?,
                    datum_numeric(name, b, arena)?,
                );
                Ok(Datum::Numeric(numeric::trunc_div(&x, &y, arena)?))
            }
            "scale" => {
                arity(1)?;
                match eval_full(args[0], arena, params, row, hooks)? {
                    Datum::Null => Ok(Datum::Null),
                    Datum::Numeric(n) if n.is_special() => Ok(Datum::Null),
                    Datum::Numeric(n) => Ok(Datum::Int4(n.dscale as i32)),
                    Datum::Int4(_) | Datum::Int8(_) => Ok(Datum::Int4(0)),
                    other => Err(type_mismatch(name, &other)),
                }
            }
            "min_scale" => {
                arity(1)?;
                match eval_full(args[0], arena, params, row, hooks)? {
                    Datum::Null => Ok(Datum::Null),
                    Datum::Numeric(n) if n.is_special() => Ok(Datum::Null),
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
            | "tanh" | "asinh" | "acosh" | "atanh" | "degrees" | "radians" | "asind" | "acosd"
            | "atand" | "sind" | "cosd" | "tand" | "cotd" | "erf" | "erfc" | "gamma" | "lgamma" => {
                arity(1)?;
                let Some(x) = num_f64(name, args, 0, arena, params, row, hooks)? else {
                    return Ok(Datum::Null);
                };
                let result = match name {
                    "cbrt" => x.cbrt(),
                    "sin" | "cos" | "tan" | "cot" if x.is_infinite() => {
                        return Err(input_out_of_range());
                    }
                    "sin" => x.sin(),
                    "cos" => x.cos(),
                    "tan" => x.tan(),
                    "cot" => 1.0 / x.tan(),
                    "asin" | "acos" if !x.is_nan() && !(-1.0..=1.0).contains(&x) => {
                        return Err(input_out_of_range());
                    }
                    "asin" => x.asin(),
                    "acos" => x.acos(),
                    "atan" => x.atan(),
                    "sinh" => x.sinh(),
                    "cosh" => x.cosh(),
                    "tanh" => x.tanh(),
                    "asinh" => x.asinh(),
                    "acosh" if !x.is_nan() && x < 1.0 => return Err(input_out_of_range()),
                    "acosh" => x.acosh(),
                    "atanh" if !x.is_nan() && !(-1.0..=1.0).contains(&x) => {
                        return Err(input_out_of_range());
                    }
                    "atanh" => x.atanh(),
                    "degrees" => x.to_degrees(),
                    "radians" => x.to_radians(),
                    "asind" | "acosd" | "atand" | "sind" | "cosd" | "tand" | "cotd" => {
                        degree_trig(name, x)?
                    }
                    "erf" | "erfc" | "gamma" | "lgamma" => special_f64(name, x)?,
                    _ => unreachable!(),
                };
                if matches!(name, "degrees" | "radians") && x.is_finite() && result.is_infinite() {
                    return Err(sql_err!(
                        sqlstate::NUMERIC_OUT_OF_RANGE,
                        "value out of range: overflow"
                    ));
                }
                Ok(Datum::Float8(result))
            }
            "atan2" | "atan2d" => {
                arity(2)?;
                let (Some(a), Some(bb)) = (
                    num_f64(name, args, 0, arena, params, row, hooks)?,
                    num_f64(name, args, 1, arena, params, row, hooks)?,
                ) else {
                    return Ok(Datum::Null);
                };
                Ok(Datum::Float8(if name == "atan2d" {
                    if a.is_nan() || bb.is_nan() {
                        f64::NAN
                    } else {
                        let atan_one = 1.0_f64.atan();
                        (a.atan2(bb) / atan_one) * 45.0
                    }
                } else {
                    a.atan2(bb)
                }))
            }
            "pi" => {
                arity(0)?;
                Ok(Datum::Float8(core::f64::consts::PI))
            }
            "random" => {
                if args.is_empty() && !star {
                    return Ok(Datum::Float8(crate::sql::guc::active_random()?));
                }
                arity(2)?;
                let lower = eval_full(args[0], arena, params, row, hooks)?;
                let upper = eval_full(args[1], arena, params, row, hooks)?;
                if lower.is_null() || upper.is_null() {
                    return Ok(Datum::Null);
                }
                let bad_bounds = || {
                    sql_err!(
                        sqlstate::INVALID_PARAMETER_VALUE,
                        "lower bound must be less than or equal to upper bound"
                    )
                };
                match (lower, upper) {
                    (Datum::Int2(a), Datum::Int2(b)) => {
                        if a > b {
                            return Err(bad_bounds());
                        }
                        Ok(Datum::Int4(crate::sql::guc::active_random_i64_range(
                            i64::from(a),
                            i64::from(b),
                        )? as i32))
                    }
                    (Datum::Int4(a), Datum::Int4(b)) => {
                        if a > b {
                            return Err(bad_bounds());
                        }
                        Ok(Datum::Int4(crate::sql::guc::active_random_i64_range(
                            i64::from(a),
                            i64::from(b),
                        )? as i32))
                    }
                    (
                        a @ (Datum::Int2(_) | Datum::Int4(_) | Datum::Int8(_)),
                        b @ (Datum::Int2(_) | Datum::Int4(_) | Datum::Int8(_)),
                    ) => {
                        let a = match a {
                            Datum::Int2(v) => i64::from(v),
                            Datum::Int4(v) => i64::from(v),
                            Datum::Int8(v) => v,
                            _ => unreachable!(),
                        };
                        let b = match b {
                            Datum::Int2(v) => i64::from(v),
                            Datum::Int4(v) => i64::from(v),
                            Datum::Int8(v) => v,
                            _ => unreachable!(),
                        };
                        if a > b {
                            return Err(bad_bounds());
                        }
                        Ok(Datum::Int8(crate::sql::guc::active_random_i64_range(a, b)?))
                    }
                    (a, b)
                        if matches!(
                            a,
                            Datum::Numeric(_) | Datum::Int2(_) | Datum::Int4(_) | Datum::Int8(_)
                        ) && matches!(
                            b,
                            Datum::Numeric(_) | Datum::Int2(_) | Datum::Int4(_) | Datum::Int8(_)
                        ) =>
                    {
                        let a = datum_numeric(name, a, arena)?;
                        let b = datum_numeric(name, b, arena)?;
                        Ok(Datum::Numeric(random_numeric(&a, &b, arena)?))
                    }
                    (other, _) => Err(type_mismatch(name, &other)),
                }
            }
            "random_normal" => {
                if args.len() > 2 || star {
                    return Err(arity_err(name, args.len()));
                }
                let mean = if args.is_empty() {
                    0.0
                } else {
                    let Some(value) = num_f64(name, args, 0, arena, params, row, hooks)? else {
                        return Ok(Datum::Null);
                    };
                    value
                };
                let stddev = if args.len() < 2 {
                    1.0
                } else {
                    let Some(value) = num_f64(name, args, 1, arena, params, row, hooks)? else {
                        return Ok(Datum::Null);
                    };
                    value
                };
                Ok(Datum::Float8(crate::sql::guc::active_random_normal(
                    mean, stddev,
                )?))
            }
            "setseed" => {
                arity(1)?;
                let Some(seed) = num_f64(name, args, 0, arena, params, row, hooks)? else {
                    return Ok(Datum::Null);
                };
                crate::sql::guc::set_active_random_seed(seed)?;
                Ok(Datum::Text(""))
            }
            "factorial" => {
                arity(1)?;
                let Some(n) = int_arg(name, args, 0, arena, params, row, hooks)? else {
                    return Ok(Datum::Null);
                };
                if n < 0 {
                    return Err(sql_err!(
                        sqlstate::NUMERIC_OUT_OF_RANGE,
                        "factorial of a negative number is undefined"
                    ));
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
