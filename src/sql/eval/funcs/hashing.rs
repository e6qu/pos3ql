//! PostgreSQL scalar hash contracts, including seeded 64-bit results.
//! https://github.com/postgres/postgres/blob/REL_18_6/src/backend/access/hash/hashfunc.c

use crate::mem::arena::Arena;
use crate::sql::ast::Expr;
use crate::sql::exec::{ColTypeResolver, infer_routine_argument_oid};
use crate::sql::types::{ColType, Datum, builtin_implicit_cast, oid};
use crate::sql_err;

use super::super::{
    ColumnLookup, EvalHooks, RuntimeColumnTypes, SqlError, cast_to, constructor_type_from_oid,
    eval_full, resolved_expression_collation, sqlstate,
};

pub(crate) fn signature(name: &str) -> Option<(ColType, bool)> {
    let name = name.strip_prefix("pg_catalog.").unwrap_or(name);
    let input = match name {
        "hashtext" | "hashtextextended" => ColType::Text,
        "hashbpchar" | "hashbpcharextended" => ColType::Bpchar,
        "hashname" | "hashnameextended" => ColType::Name,
        "hashchar" | "hashcharextended" => ColType::Char,
        "hashbool" | "hashboolextended" => ColType::Bool,
        "hashint2" | "hashint2extended" => ColType::Int2,
        "hashint4" | "hashint4extended" => ColType::Int4,
        "hashint8" | "hashint8extended" => ColType::Int8,
        "hashoid" | "hashoidextended" => ColType::Oid,
        "hashfloat4" | "hashfloat4extended" => ColType::Float4,
        "hashfloat8" | "hashfloat8extended" => ColType::Float8,
        _ => return None,
    };
    Some((input, name.ends_with("extended")))
}

pub(crate) fn describe(
    name: &str,
    args: &[&Expr<'_>],
    star: bool,
    columns: &dyn ColTypeResolver,
) -> Option<Result<(i32, i16), SqlError>> {
    let (input, extended) = signature(name)?;
    Some((|| {
        let invalid = || {
            sql_err!(
                sqlstate::UNDEFINED_FUNCTION,
                "function {}(...) does not exist",
                name
            )
        };
        if star || args.len() != 1 + usize::from(extended) {
            return Err(invalid());
        }
        for (index, argument) in args.iter().enumerate() {
            let actual = infer_routine_argument_oid(argument, columns)?;
            if actual == oid::UNKNOWN {
                continue;
            }
            let actual = constructor_type_from_oid(actual, &|oid| columns.domain_base_type(oid));
            let expected = if index == 0 { input } else { ColType::Int8 };
            if !actual.is_some_and(|actual| builtin_implicit_cast(actual, expected)) {
                return Err(invalid());
            }
        }
        let result = if extended {
            ColType::Int8
        } else {
            ColType::Int4
        };
        Ok((result.oid(), result.typlen()))
    })())
}

#[expect(clippy::too_many_arguments, reason = "explicit SQL execution context")]
pub(crate) fn dispatch<'a>(
    name: &str,
    args: &[&Expr<'a>],
    star: bool,
    arena: &'a Arena,
    params: &[Datum<'a>],
    row: &impl ColumnLookup<'a>,
    hooks: &EvalHooks<'_, 'a>,
) -> Option<Result<Datum<'a>, SqlError>> {
    let (input, extended) = signature(name)?;
    Some((|| {
        describe(
            name,
            args,
            star,
            &RuntimeColumnTypes {
                row,
                catalog: hooks.catalog,
            },
        )
        .expect("recognized hash signature")?;
        let mut values = [Datum::Null; 2];
        let mut constants = [false; 2];
        for (index, argument) in args.iter().enumerate() {
            constants[index] = argument.is_constant();
            if constants[index] {
                values[index] = eval_full(argument, arena, params, row, hooks)?;
            }
        }
        // PostgreSQL folds strict calls with a constant null before running
        // volatile arguments, but still checks constant argument errors.
        // https://github.com/postgres/postgres/blob/REL_18_6/src/backend/optimizer/util/clauses.c#L4173
        if args
            .iter()
            .enumerate()
            .any(|(index, _)| constants[index] && values[index].is_null())
        {
            return Ok(Datum::Null);
        }
        for (index, argument) in args.iter().enumerate() {
            if !constants[index] {
                values[index] = eval_full(argument, arena, params, row, hooks)?;
            }
        }
        let value = values[0];
        let seed = if extended { values[1] } else { Datum::Int8(0) };
        // Strict functions still evaluate reached arguments before deciding
        // that a null input suppresses the function call.
        if value.is_null() || seed.is_null() {
            return Ok(Datum::Null);
        }
        let value = cast_to(value, input, arena)?;
        let Datum::Int8(seed) = cast_to(seed, ColType::Int8, arena)? else {
            unreachable!("validated seed conversion")
        };
        let hash_u32 = |value| {
            if extended {
                Datum::Int8(crate::sql::identity::hash_uint32_extended(value, seed) as i64)
            } else {
                Datum::Int4(crate::sql::identity::hash_uint32(value) as i32)
            }
        };
        let hash_bytes = |bytes: &[u8]| {
            if extended {
                Datum::Int8(crate::sql::identity::hash_bytea_extended(bytes, seed))
            } else {
                Datum::Int4(crate::sql::identity::hash_bytea(bytes))
            }
        };
        Ok(match value {
            Datum::Text(value) | Datum::Bpchar(value) => {
                if matches!(input, ColType::Text | ColType::Bpchar) {
                    resolved_expression_collation(args[0], row, hooks.catalog)?;
                }
                let value = if input == ColType::Bpchar {
                    value.trim_end_matches(' ')
                } else {
                    value
                };
                hash_bytes(value.as_bytes())
            }
            Datum::Char(value) => hash_u32((value as i8 as i32) as u32),
            Datum::Bool(value) => hash_u32(u32::from(value)),
            Datum::Int2(value) => hash_u32(value as i32 as u32),
            Datum::Int4(value) => hash_u32(value as u32),
            Datum::Int8(value) => hash_u32(crate::sql::identity::hash_int64_input(value)),
            Datum::Oid(value) => hash_u32(value),
            Datum::Float4(value) => hash_float(f64::from(value), seed, extended),
            Datum::Float8(value) => hash_float(value, seed, extended),
            _ => unreachable!("validated scalar hash input conversion"),
        })
    })())
}

fn hash_float(value: f64, seed: i64, extended: bool) -> Datum<'static> {
    if value == 0.0 {
        return if extended {
            Datum::Int8(seed)
        } else {
            Datum::Int4(0)
        };
    }
    // Equal NaNs and signed zeros must hash identically across float widths.
    let value = if value.is_nan() { f64::NAN } else { value };
    let bytes = value.to_ne_bytes();
    if extended {
        Datum::Int8(crate::sql::identity::hash_bytea_extended(&bytes, seed))
    } else {
        Datum::Int4(crate::sql::identity::hash_bytea(&bytes))
    }
}
