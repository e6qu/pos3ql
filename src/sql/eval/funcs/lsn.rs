//! Scalar support routines for PostgreSQL's `pg_lsn` type.

use crate::sql::ast::Expr;
use crate::sql::types::{ColType, Datum};
use crate::sql_err;

use super::super::{ColumnLookup, EvalHooks, SqlError, eval_full, sqlstate, type_mismatch};

fn is_unknown(expression: &Expr<'_>) -> bool {
    matches!(expression, Expr::Str(_) | Expr::Null | Expr::Param(_))
}

fn lsn_argument<'a>(
    name: &str,
    expression: &Expr<'_>,
    value: Datum<'a>,
) -> Result<Datum<'a>, SqlError> {
    match value {
        Datum::Text(value) if is_unknown(expression) => {
            crate::sql::lsn::parse(value).map(Datum::PgLsn)
        }
        Datum::PgLsn(_) | Datum::Null => Ok(value),
        other => Err(type_mismatch(name, &other)),
    }
}

fn numeric_argument<'a>(
    name: &str,
    expression: &Expr<'_>,
    value: Datum<'a>,
    arena: &'a crate::mem::arena::Arena,
) -> Result<Datum<'a>, SqlError> {
    let numeric = match value {
        Datum::Numeric(_) | Datum::Null => return Ok(value),
        Datum::Int2(value) => crate::sql::numeric::Numeric::from_i64(i64::from(value), arena),
        Datum::Int4(value) => crate::sql::numeric::Numeric::from_i64(i64::from(value), arena),
        Datum::Oid(value) => crate::sql::numeric::Numeric::from_i128(i128::from(value), arena),
        Datum::Int8(value) => crate::sql::numeric::Numeric::from_i64(value, arena),
        Datum::Text(value) if is_unknown(expression) => {
            crate::sql::numeric::Numeric::parse(value, arena)
        }
        other => return Err(type_mismatch(name, &other)),
    }?;
    Ok(Datum::Numeric(numeric))
}

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
        "pg_lsn_in"
            | "pg_lsn_out"
            | "pg_lsn_send"
            | "pg_lsn"
            | "pg_lsn_lt"
            | "pg_lsn_le"
            | "pg_lsn_eq"
            | "pg_lsn_ge"
            | "pg_lsn_gt"
            | "pg_lsn_ne"
            | "pg_lsn_mi"
            | "pg_wal_lsn_diff"
            | "pg_lsn_pli"
            | "numeric_pl_pg_lsn"
            | "pg_lsn_mii"
            | "pg_lsn_cmp"
            | "pg_lsn_hash"
            | "pg_lsn_hash_extended"
            | "pg_lsn_larger"
            | "pg_lsn_smaller"
    ) {
        return None;
    }
    Some((|| {
        let unary = matches!(
            name,
            "pg_lsn_in" | "pg_lsn_out" | "pg_lsn_send" | "pg_lsn" | "pg_lsn_hash"
        );
        let expected = if unary { 1 } else { 2 };
        if star || args.len() != expected {
            return Err(sql_err!(
                sqlstate::UNDEFINED_FUNCTION,
                "function {}(...) with {} arguments does not exist",
                name,
                if star { 1 } else { args.len() }
            ));
        }
        let left = eval_full(args[0], arena, params, row, hooks)?;
        let left = match name {
            "pg_lsn_in" => left,
            "pg_lsn" | "numeric_pl_pg_lsn" => numeric_argument(name, args[0], left, arena)?,
            _ => lsn_argument(name, args[0], left)?,
        };
        if left.is_null() {
            return Ok(Datum::Null);
        }
        match name {
            "pg_lsn_in" => match left {
                Datum::Text(value) => crate::sql::lsn::parse(value).map(Datum::PgLsn),
                other => Err(type_mismatch(name, &other)),
            },
            "pg_lsn_out" => match left {
                Datum::PgLsn(value) => arena
                    .alloc_str_display(Datum::PgLsn(value))
                    .map(Datum::Text)
                    .map_err(|_| super::super::arena_full()),
                other => Err(type_mismatch(name, &other)),
            },
            "pg_lsn_send" => match left {
                Datum::PgLsn(value) => arena
                    .alloc_slice_copy(&value.to_be_bytes())
                    .map(|bytes| Datum::Bytea(&*bytes))
                    .map_err(|_| super::super::arena_full()),
                other => Err(type_mismatch(name, &other)),
            },
            "pg_lsn" => match left {
                Datum::Numeric(value) => crate::sql::lsn::from_numeric(&value).map(Datum::PgLsn),
                other => Err(type_mismatch(name, &other)),
            },
            "pg_lsn_hash" => match left {
                Datum::PgLsn(value) => Ok(Datum::Int4(crate::sql::lsn::hash(value))),
                other => Err(type_mismatch(name, &other)),
            },
            _ => {
                let right = eval_full(args[1], arena, params, row, hooks)?;
                let right = match name {
                    "pg_lsn_hash_extended" => match right {
                        Datum::Int2(value) => Datum::Int8(i64::from(value)),
                        Datum::Int4(value) => Datum::Int8(i64::from(value)),
                        Datum::Oid(value) => Datum::Int8(i64::from(value)),
                        Datum::Int8(_) | Datum::Null => right,
                        other => return Err(type_mismatch(name, &other)),
                    },
                    "pg_lsn_pli" | "pg_lsn_mii" => numeric_argument(name, args[1], right, arena)?,
                    "numeric_pl_pg_lsn" => lsn_argument(name, args[1], right)?,
                    _ => lsn_argument(name, args[1], right)?,
                };
                if right.is_null() {
                    return Ok(Datum::Null);
                }
                binary(name, left, right, arena)
            }
        }
    })())
}

fn binary<'a>(
    name: &str,
    left: Datum<'a>,
    right: Datum<'a>,
    arena: &'a crate::mem::arena::Arena,
) -> Result<Datum<'a>, SqlError> {
    match name {
        "pg_lsn_hash_extended" => match (left, right) {
            (Datum::PgLsn(value), Datum::Int8(seed)) => {
                Ok(Datum::Int8(crate::sql::lsn::hash_extended(value, seed)))
            }
            (left, _) => Err(type_mismatch(name, &left)),
        },
        "pg_lsn_mi" | "pg_wal_lsn_diff" => match (left, right) {
            (Datum::PgLsn(left), Datum::PgLsn(right)) => {
                crate::sql::lsn::difference(left, right, arena).map(Datum::Numeric)
            }
            (left, _) => Err(type_mismatch(name, &left)),
        },
        "pg_lsn_pli" | "pg_lsn_mii" => match (left, right) {
            (Datum::PgLsn(value), Datum::Numeric(offset)) => {
                crate::sql::lsn::shift(value, &offset, name == "pg_lsn_mii", arena)
                    .map(Datum::PgLsn)
            }
            (left, _) => Err(type_mismatch(name, &left)),
        },
        "numeric_pl_pg_lsn" => match (left, right) {
            (Datum::Numeric(offset), Datum::PgLsn(value)) => {
                crate::sql::lsn::shift(value, &offset, false, arena).map(Datum::PgLsn)
            }
            (left, _) => Err(type_mismatch(name, &left)),
        },
        _ => {
            let (left, right) = match (left, right) {
                (Datum::PgLsn(left), Datum::PgLsn(right)) => (left, right),
                (left, _) => return Err(type_mismatch(name, &left)),
            };
            Ok(match name {
                "pg_lsn_lt" => Datum::Bool(left < right),
                "pg_lsn_le" => Datum::Bool(left <= right),
                "pg_lsn_eq" => Datum::Bool(left == right),
                "pg_lsn_ge" => Datum::Bool(left >= right),
                "pg_lsn_gt" => Datum::Bool(left > right),
                "pg_lsn_ne" => Datum::Bool(left != right),
                "pg_lsn_cmp" => Datum::Int4(match left.cmp(&right) {
                    core::cmp::Ordering::Less => -1,
                    core::cmp::Ordering::Equal => 0,
                    core::cmp::Ordering::Greater => 1,
                }),
                "pg_lsn_larger" => Datum::PgLsn(left.max(right)),
                "pg_lsn_smaller" => Datum::PgLsn(left.min(right)),
                _ => unreachable!("guard admitted an unhandled pg_lsn function"),
            })
        }
    }
}

pub(crate) fn result_type(name: &str) -> Option<ColType> {
    match name {
        "pg_lsn_in" | "pg_lsn" | "pg_lsn_pli" | "numeric_pl_pg_lsn" | "pg_lsn_mii"
        | "pg_lsn_larger" | "pg_lsn_smaller" => Some(ColType::PgLsn),
        "pg_lsn_out" => Some(ColType::Text),
        "pg_lsn_send" => Some(ColType::Bytea),
        "pg_lsn_lt" | "pg_lsn_le" | "pg_lsn_eq" | "pg_lsn_ge" | "pg_lsn_gt" | "pg_lsn_ne" => {
            Some(ColType::Bool)
        }
        "pg_lsn_mi" | "pg_wal_lsn_diff" => Some(ColType::Numeric),
        "pg_lsn_cmp" | "pg_lsn_hash" => Some(ColType::Int4),
        "pg_lsn_hash_extended" => Some(ColType::Int8),
        _ => None,
    }
}
