//! Scalar support routines for PostgreSQL `tid` and `cid` identities.

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
        "tidin"
            | "tidout"
            | "tidsend"
            | "tideq"
            | "tidne"
            | "tidlt"
            | "tidle"
            | "tidgt"
            | "tidge"
            | "bttidcmp"
            | "tidlarger"
            | "tidsmaller"
            | "hashtid"
            | "hashtidextended"
            | "cidin"
            | "cidout"
            | "cidsend"
            | "cideq"
            | "hashcid"
            | "hashcidextended"
    ) {
        return None;
    }
    Some((|| {
        let binary = matches!(
            name,
            "tideq"
                | "tidne"
                | "tidlt"
                | "tidle"
                | "tidgt"
                | "tidge"
                | "bttidcmp"
                | "tidlarger"
                | "tidsmaller"
                | "hashtidextended"
                | "cideq"
                | "hashcidextended"
        );
        let expected = if binary { 2 } else { 1 };
        if star || args.len() != expected {
            return Err(sql_err!(
                sqlstate::UNDEFINED_FUNCTION,
                "function {}(...) with {} arguments does not exist",
                name,
                if star { 1 } else { args.len() }
            ));
        }
        let target = if name.starts_with("cid") || name.starts_with("hashcid") {
            ColType::Cid
        } else {
            ColType::Tid
        };
        let left = argument(args[0], target, arena, params, row, hooks, name)?;
        if left.is_null() {
            return Ok(Datum::Null);
        }
        match (name, left) {
            ("tidin", Datum::Tid(value)) => Ok(Datum::Tid(value)),
            ("tidout", Datum::Tid(value)) => arena
                .alloc_str_display(value)
                .map(Datum::Text)
                .map_err(|_| super::super::arena_full()),
            ("tidsend", Datum::Tid(value)) => {
                let bytes = arena
                    .alloc_slice_with(6, |_| 0u8)
                    .map_err(|_| super::super::arena_full())?;
                bytes[..4].copy_from_slice(&value.block.to_be_bytes());
                bytes[4..].copy_from_slice(&value.offset.to_be_bytes());
                Ok(Datum::Bytea(bytes))
            }
            ("hashtid", Datum::Tid(value)) => {
                Ok(Datum::Int4(crate::sql::identity::hash_tid(value)))
            }
            ("cidin", Datum::Cid(value)) => Ok(Datum::Cid(value)),
            ("cidout", Datum::Cid(value)) => arena
                .alloc_str_display(value)
                .map(Datum::Text)
                .map_err(|_| super::super::arena_full()),
            ("cidsend", Datum::Cid(value)) => arena
                .alloc_slice_copy(&value.to_be_bytes())
                .map(|bytes| Datum::Bytea(&*bytes))
                .map_err(|_| super::super::arena_full()),
            ("hashcid", Datum::Cid(value)) => {
                Ok(Datum::Int4(crate::sql::identity::hash_cid(value)))
            }
            (_, Datum::Tid(left)) => {
                let seed = name == "hashtidextended";
                let right = argument(
                    args[1],
                    if seed { ColType::Int8 } else { ColType::Tid },
                    arena,
                    params,
                    row,
                    hooks,
                    name,
                )?;
                if right.is_null() {
                    return Ok(Datum::Null);
                }
                if let Datum::Int8(seed) = right {
                    return Ok(Datum::Int8(crate::sql::identity::hash_tid_extended(
                        left, seed,
                    )));
                }
                let Datum::Tid(right) = right else {
                    return Err(bad_types(name));
                };
                Ok(match name {
                    "tideq" => Datum::Bool(left == right),
                    "tidne" => Datum::Bool(left != right),
                    "tidlt" => Datum::Bool(left < right),
                    "tidle" => Datum::Bool(left <= right),
                    "tidgt" => Datum::Bool(left > right),
                    "tidge" => Datum::Bool(left >= right),
                    "bttidcmp" => Datum::Int4(match left.cmp(&right) {
                        core::cmp::Ordering::Less => -1,
                        core::cmp::Ordering::Equal => 0,
                        core::cmp::Ordering::Greater => 1,
                    }),
                    "tidlarger" => Datum::Tid(left.max(right)),
                    "tidsmaller" => Datum::Tid(left.min(right)),
                    _ => return Err(bad_types(name)),
                })
            }
            (_, Datum::Cid(left)) => {
                let seed = name == "hashcidextended";
                let right = argument(
                    args[1],
                    if seed { ColType::Int8 } else { ColType::Cid },
                    arena,
                    params,
                    row,
                    hooks,
                    name,
                )?;
                if right.is_null() {
                    return Ok(Datum::Null);
                }
                if let Datum::Int8(seed) = right {
                    return Ok(Datum::Int8(crate::sql::identity::hash_cid_extended(
                        left, seed,
                    )));
                }
                match right {
                    Datum::Cid(right) if name == "cideq" => Ok(Datum::Bool(left == right)),
                    _ => Err(bad_types(name)),
                }
            }
            _ => Err(bad_types(name)),
        }
    })())
}

#[allow(clippy::too_many_arguments)]
fn argument<'a>(
    expression: &Expr<'a>,
    target: ColType,
    arena: &'a crate::mem::arena::Arena,
    params: &[Datum<'a>],
    row: &impl ColumnLookup<'a>,
    hooks: &EvalHooks<'_, 'a>,
    name: &str,
) -> Result<Datum<'a>, SqlError> {
    let value = eval_full(expression, arena, params, row, hooks)?;
    if value.is_null() {
        return Ok(value);
    }
    let already_typed = matches!(
        (value, target),
        (Datum::Tid(_), ColType::Tid)
            | (Datum::Cid(_), ColType::Cid)
            | (
                Datum::Int2(_) | Datum::Int4(_) | Datum::Oid(_) | Datum::Int8(_),
                ColType::Int8
            )
    );
    if already_typed || super::super::is_unknown_literal(expression) {
        return super::super::cast_to(value, target, arena);
    }
    Err(bad_types(name))
}

fn bad_types(name: &str) -> SqlError {
    sql_err!(
        sqlstate::UNDEFINED_FUNCTION,
        "function {}(...) does not exist",
        name
    )
}
