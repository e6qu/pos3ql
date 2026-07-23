//! Miscellaneous scalar built-ins that don't belong to a type family: the
//! record constructor (`row`), the `format` string builder, `to_number`
//! numeric parsing, and `pg_size_pretty` byte-size rendering.

use core::fmt::Write;

use crate::sql::ast::Expr;
use crate::sql::parser;
use crate::sql::to_char;
use crate::sql::types::{Datum, RecordField};
use crate::util::StackStr;
use crate::{sql_err, stack_format};

use super::super::{
    arena_full, arity_err, eval_full, format_append_ident, format_append_literal,
    format_append_str, sqlstate, text_arg, type_mismatch, ColumnLookup, EvalHooks, SqlError,
};

/// Handles the miscellaneous scalar family. Returns `None` if `name` is not one
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
    if !matches!(name, "row" | "pg_size_pretty" | "format" | "to_number") {
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
            // Record constructor `ROW(a, b, ...)` / `row(...)`: fields are named
            // f1, f2, ... as PostgreSQL does for an anonymous record.
            "row" => {
                if args.len() > parser::MAX_LIST {
                    return Err(sql_err!(sqlstate::PROGRAM_LIMIT_EXCEEDED, "too many fields in ROW()"));
                }
                let mut fields = [RecordField {
                    name: "",
                    type_oid: 0,
                    value: Datum::Null,
                }; parser::MAX_LIST];
                for (i, arg) in args.iter().enumerate() {
                    let v = eval_full(arg, arena, params, row, hooks)?;
                    let name = stack_format!(12, "f{}", i + 1);
                    fields[i] = RecordField {
                        name: arena.alloc_str(name.as_str()).map_err(|_| arena_full())?,
                        type_oid: v.type_oid(),
                        value: v,
                    };
                }
                let out = arena.alloc_slice_copy(&fields[..args.len()]).map_err(|_| arena_full())?;
                Ok(Datum::Record(&*out))
            }
            "pg_size_pretty" => {
                // Human-readable byte size, matching PostgreSQL's pg_size_pretty:
                // "N bytes" below 10 kB, then half-rounded kB/MB/GB/TB/PB via the
                // same successive right-shifts (÷512 once, then ÷1024 per step).
                arity(1)?;
                // PostgreSQL exposes pg_size_pretty(bigint) and pg_size_pretty(numeric)
                // only; a narrower integer (int2/int4) is rejected there as ambiguous,
                // so it is not accepted here either.
                let size = match eval_full(args[0], arena, params, row, hooks)? {
                    Datum::Null => return Ok(Datum::Null),
                    Datum::Int8(v) => v,
                    Datum::Numeric(n) => n.to_i64()?,
                    other => return Err(type_mismatch(name, &other)),
                };
                const UNITS: [&str; 6] = ["bytes", "kB", "MB", "GB", "TB", "PB"];
                let limit: i64 = 10 * 1024;
                let limit2 = limit * 2 - 1;
                let half_rounded = |x: i64| (x + if x < 0 { -1 } else { 1 }) / 2;
                let text = if size.unsigned_abs() < limit as u64 {
                    stack_format!(64, "{} bytes", size)
                } else {
                    let mut scaled = size >> 9;
                    let mut index = 1usize;
                    while index < UNITS.len() - 1 {
                        if scaled.unsigned_abs() < limit2 as u64 {
                            break;
                        }
                        scaled >>= 10;
                        index += 1;
                    }
                    stack_format!(64, "{} {}", half_rounded(scaled), UNITS[index])
                };
                Ok(Datum::Text(arena.alloc_str(text.as_str()).map_err(|_| arena_full())?))
            }
            "format" => {
                if args.is_empty() {
                    return Err(arity_err(name, 0));
                }
                let Some(fmt) = text_arg(name, args, 0, arena, params, row, hooks)? else {
                    return Ok(Datum::Null);
                };
                let mut out = StackStr::<4096>::new();
                let mut argi = 1usize;
                let bytes = fmt.as_bytes();
                let mut i = 0usize;
                while i < bytes.len() {
                    if bytes[i] != b'%' {
                        let _ = out.write_char(bytes[i] as char);
                        i += 1;
                        continue;
                    }
                    i += 1;
                    let Some(&spec) = bytes.get(i) else {
                        return Err(sql_err!(sqlstate::INVALID_PARAMETER_VALUE, "unterminated format specifier"));
                    };
                    i += 1;
                    if spec == b'%' {
                        let _ = out.write_char('%');
                        continue;
                    }
                    if argi >= args.len() {
                        return Err(sql_err!(sqlstate::INVALID_PARAMETER_VALUE, "too few arguments for format()"));
                    }
                    let v = eval_full(args[argi], arena, params, row, hooks)?;
                    argi += 1;
                    match spec {
                        b's' => format_append_str(&mut out, v, arena)?,
                        b'I' => format_append_ident(&mut out, v)?,
                        b'L' => format_append_literal(&mut out, v, arena)?,
                        other => {
                            return Err(sql_err!(
                                sqlstate::INVALID_PARAMETER_VALUE,
                                "unrecognized format() type specifier \"{}\"",
                                other as char
                            ))
                        }
                    }
                }
                Ok(Datum::Text(arena.alloc_str(out.as_str()).map_err(|_| arena_full())?))
            }
            "to_number" => {
                arity(2)?;
                let (Some(s), Some(fmt)) = (
                    text_arg(name, args, 0, arena, params, row, hooks)?,
                    text_arg(name, args, 1, arena, params, row, hooks)?,
                ) else {
                    return Ok(Datum::Null);
                };
                Ok(Datum::Numeric(to_char::to_number(s, fmt, arena)?))
            }
            _ => unreachable!("dispatch guard admitted an unhandled name"),
        }
    })())
}
