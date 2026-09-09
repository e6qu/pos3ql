//! Scalar support routines for PostgreSQL's `money` type.

use crate::sql::ast::Expr;
use crate::sql::types::{ColType, Datum};
use crate::sql_err;

use super::super::{ColumnLookup, EvalHooks, SqlError, eval_full, sqlstate};

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
        "cash_in"
            | "cash_out"
            | "cash_send"
            | "cash_cmp"
            | "cash_eq"
            | "cash_ne"
            | "cash_lt"
            | "cash_le"
            | "cash_gt"
            | "cash_ge"
            | "cash_pl"
            | "cash_mi"
            | "cash_mul_int2"
            | "cash_mul_int4"
            | "cash_mul_int8"
            | "int2_mul_cash"
            | "int4_mul_cash"
            | "int8_mul_cash"
            | "cash_div_int2"
            | "cash_div_int4"
            | "cash_div_int8"
            | "cash_mul_flt4"
            | "cash_mul_flt8"
            | "flt4_mul_cash"
            | "flt8_mul_cash"
            | "cash_div_flt4"
            | "cash_div_flt8"
            | "cash_div_cash"
            | "cashlarger"
            | "cashsmaller"
            | "cash_words"
            | "money"
    ) {
        return None;
    }
    Some((|| {
        if star
            || (name == "cash_send"
                || name == "cash_out"
                || name == "cash_words"
                || name == "money"
                || name == "cash_in")
                && args.len() != 1
            || !matches!(
                name,
                "cash_send" | "cash_out" | "cash_words" | "money" | "cash_in"
            ) && args.len() != 2
        {
            return Err(sql_err!(
                sqlstate::UNDEFINED_FUNCTION,
                "function {}(...) with {} arguments does not exist",
                name,
                if star { 1 } else { args.len() }
            ));
        }
        let left = eval_full(args[0], arena, params, row, hooks)?;
        let left = coerce_argument(
            args[0],
            left,
            match name {
                "cash_in" | "money" => None,
                "int2_mul_cash" => Some(ColType::Int2),
                "int4_mul_cash" => Some(ColType::Int4),
                "int8_mul_cash" => Some(ColType::Int8),
                "flt4_mul_cash" => Some(ColType::Float4),
                "flt8_mul_cash" => Some(ColType::Float8),
                _ => Some(ColType::Money),
            },
            arena,
            name,
        )?;
        if left.is_null() {
            return Ok(Datum::Null);
        }
        if name == "money" {
            return super::super::cast_to(left, ColType::Money, arena);
        }
        if name == "cash_in" {
            return match left {
                Datum::Text(text) => crate::sql::money::parse(text).map(Datum::Money),
                _ => Err(bad_types(name)),
            };
        }
        let commuted = matches!(
            name,
            "int2_mul_cash" | "int4_mul_cash" | "int8_mul_cash" | "flt4_mul_cash" | "flt8_mul_cash"
        );
        if commuted {
            let right = eval_full(args[1], arena, params, row, hooks)?;
            let right = coerce_argument(args[1], right, Some(ColType::Money), arena, name)?;
            if right.is_null() {
                return Ok(Datum::Null);
            }
            let Datum::Money(money) = right else {
                return Err(bad_types(name));
            };
            return Ok(Datum::Money(if name.starts_with("flt") {
                crate::sql::money::scale_float(
                    money,
                    float(left).ok_or_else(|| bad_types(name))?,
                    false,
                )?
            } else {
                crate::sql::money::mul_integer(
                    money,
                    integer(left).ok_or_else(|| bad_types(name))?,
                )?
            }));
        }
        let money = match left {
            Datum::Money(value) => value,
            _ => return Err(bad_types(name)),
        };
        match name {
            "cash_out" => arena
                .alloc_str_display(crate::sql::money::Display(money))
                .map(Datum::Text)
                .map_err(|_| super::super::arena_full()),
            "cash_send" => arena
                .alloc_slice_copy(&money.to_be_bytes())
                .map(|bytes| Datum::Bytea(&*bytes))
                .map_err(|_| super::super::arena_full()),
            "cash_words" => arena
                .alloc_str_display(crate::sql::money::Words(money))
                .map(Datum::Text)
                .map_err(|_| super::super::arena_full()),
            _ => {
                let right = eval_full(args[1], arena, params, row, hooks)?;
                let right = coerce_argument(
                    args[1],
                    right,
                    Some(match name {
                        "cash_mul_int2" | "cash_div_int2" => ColType::Int2,
                        "cash_mul_int4" | "cash_div_int4" => ColType::Int4,
                        "cash_mul_int8" | "cash_div_int8" => ColType::Int8,
                        "cash_mul_flt4" | "cash_div_flt4" => ColType::Float4,
                        "cash_mul_flt8" | "cash_div_flt8" => ColType::Float8,
                        _ => ColType::Money,
                    }),
                    arena,
                    name,
                )?;
                if right.is_null() {
                    return Ok(Datum::Null);
                }
                binary(name, money, right)
            }
        }
    })())
}

fn coerce_argument<'a>(
    expression: &Expr<'a>,
    value: Datum<'a>,
    target: Option<ColType>,
    arena: &'a crate::mem::arena::Arena,
    name: &str,
) -> Result<Datum<'a>, SqlError> {
    let Some(target) = target else {
        return Ok(value);
    };
    if value.is_null() {
        return Ok(value);
    }
    let implicit = matches!(
        (value, target),
        (Datum::Money(_), ColType::Money)
            | (
                Datum::Int2(_),
                ColType::Int2 | ColType::Int4 | ColType::Int8
            )
            | (Datum::Int4(_), ColType::Int4 | ColType::Int8)
            | (Datum::Int8(_), ColType::Int8)
            | (
                Datum::Int2(_) | Datum::Int4(_) | Datum::Int8(_) | Datum::Numeric(_),
                ColType::Float4
            )
            | (
                Datum::Int2(_)
                    | Datum::Int4(_)
                    | Datum::Int8(_)
                    | Datum::Numeric(_)
                    | Datum::Float4(_),
                ColType::Float8,
            )
            | (Datum::Float4(_), ColType::Float4)
            | (Datum::Float8(_), ColType::Float8)
    );
    if implicit || super::super::is_unknown_literal(expression) {
        return super::super::cast_to(value, target, arena);
    }
    Err(bad_types(name))
}

fn binary<'a>(name: &str, left: i64, right: Datum<'a>) -> Result<Datum<'a>, SqlError> {
    let cash = match right {
        Datum::Money(value) => Some(value),
        _ => None,
    };
    Ok(match name {
        "cash_cmp" => Datum::Int4(match left.cmp(&cash.ok_or_else(|| bad_types(name))?) {
            core::cmp::Ordering::Less => -1,
            core::cmp::Ordering::Equal => 0,
            core::cmp::Ordering::Greater => 1,
        }),
        "cash_eq" => Datum::Bool(left == cash.ok_or_else(|| bad_types(name))?),
        "cash_ne" => Datum::Bool(left != cash.ok_or_else(|| bad_types(name))?),
        "cash_lt" => Datum::Bool(left < cash.ok_or_else(|| bad_types(name))?),
        "cash_le" => Datum::Bool(left <= cash.ok_or_else(|| bad_types(name))?),
        "cash_gt" => Datum::Bool(left > cash.ok_or_else(|| bad_types(name))?),
        "cash_ge" => Datum::Bool(left >= cash.ok_or_else(|| bad_types(name))?),
        "cash_pl" => Datum::Money(crate::sql::money::add(
            left,
            cash.ok_or_else(|| bad_types(name))?,
        )?),
        "cash_mi" => Datum::Money(crate::sql::money::sub(
            left,
            cash.ok_or_else(|| bad_types(name))?,
        )?),
        "cash_div_cash" => Datum::Float8(crate::sql::money::ratio(
            left,
            cash.ok_or_else(|| bad_types(name))?,
        )?),
        "cashlarger" => Datum::Money(left.max(cash.ok_or_else(|| bad_types(name))?)),
        "cashsmaller" => Datum::Money(left.min(cash.ok_or_else(|| bad_types(name))?)),
        "cash_mul_int2" | "cash_mul_int4" | "cash_mul_int8" => Datum::Money(
            crate::sql::money::mul_integer(left, integer(right).ok_or_else(|| bad_types(name))?)?,
        ),
        "cash_div_int2" | "cash_div_int4" | "cash_div_int8" => Datum::Money(
            crate::sql::money::div_integer(left, integer(right).ok_or_else(|| bad_types(name))?)?,
        ),
        "cash_mul_flt4" | "cash_mul_flt8" => Datum::Money(crate::sql::money::scale_float(
            left,
            float(right).ok_or_else(|| bad_types(name))?,
            false,
        )?),
        "cash_div_flt4" | "cash_div_flt8" => Datum::Money(crate::sql::money::scale_float(
            left,
            float(right).ok_or_else(|| bad_types(name))?,
            true,
        )?),
        "int2_mul_cash" | "int4_mul_cash" | "int8_mul_cash" | "flt4_mul_cash" | "flt8_mul_cash" => {
            unreachable!("commuted support functions handled before left-money dispatch")
        }
        _ => return Err(bad_types(name)),
    })
}

fn integer(value: Datum<'_>) -> Option<i64> {
    match value {
        Datum::Int2(value) => Some(value.into()),
        Datum::Int4(value) => Some(value.into()),
        Datum::Int8(value) => Some(value),
        _ => None,
    }
}

fn float(value: Datum<'_>) -> Option<f64> {
    match value {
        Datum::Float4(value) => Some(value.into()),
        Datum::Float8(value) => Some(value),
        _ => None,
    }
}

fn bad_types(name: &str) -> SqlError {
    sql_err!(
        sqlstate::UNDEFINED_FUNCTION,
        "function {}(...) does not exist",
        name
    )
}
