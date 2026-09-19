//! Miscellaneous scalar built-ins that don't belong to a type family: the
//! record constructor (`row`), the `format` string builder, `to_number`
//! numeric parsing, and `pg_size_pretty` byte-size rendering.

use core::fmt::Write;

use crate::mem::arena::{Arena, ArenaList};
use crate::sql::ast::Expr;
use crate::sql::to_char;
use crate::sql::types::{Datum, RecordField};
use crate::{sql_err, stack_format};

use super::super::{
    ColumnLookup, EvalHooks, SqlError, arena_full, arity_err, datum_to_text, eval_full,
    expression_type_identity, sqlstate, text_arg, type_mismatch,
};

fn decimal(bytes: &[u8], at: &mut usize) -> Result<Option<usize>, SqlError> {
    let start = *at;
    let mut value = 0usize;
    while bytes.get(*at).is_some_and(u8::is_ascii_digit) {
        value = value
            .checked_mul(10)
            .and_then(|value| value.checked_add(usize::from(bytes[*at] - b'0')))
            .ok_or_else(|| {
                sql_err!(
                    sqlstate::INVALID_PARAMETER_VALUE,
                    "format specifies an argument position that is too large"
                )
            })?;
        *at += 1;
    }
    Ok((*at != start).then_some(value))
}

fn format_argument<'a>(
    values: &[Datum<'a>],
    position: Option<usize>,
    next: &mut usize,
) -> Result<Datum<'a>, SqlError> {
    let index = match position {
        Some(0) => {
            return Err(sql_err!(
                sqlstate::INVALID_PARAMETER_VALUE,
                "format specifies argument 0, but arguments are numbered from 1"
            ));
        }
        Some(position) => {
            *next = (*next).max(position);
            position - 1
        }
        None => {
            let index = *next;
            *next += 1;
            index
        }
    };
    values.get(index).copied().ok_or_else(|| {
        sql_err!(
            sqlstate::INVALID_PARAMETER_VALUE,
            "too few arguments for format()"
        )
    })
}

struct ArenaText<'a> {
    bytes: ArenaList<'a, u8>,
}

impl<'a> ArenaText<'a> {
    const fn new(arena: &'a Arena) -> Self {
        Self {
            bytes: ArenaList::new(arena),
        }
    }

    fn finish(&self) -> &'a str {
        // Every append originates in UTF-8 text or `char` encoding.
        unsafe { core::str::from_utf8_unchecked(self.bytes.as_slice()) }
    }
}

impl core::fmt::Write for ArenaText<'_> {
    fn write_str(&mut self, text: &str) -> core::fmt::Result {
        for &byte in text.as_bytes() {
            self.bytes.push(byte).map_err(|_| core::fmt::Error)?;
        }
        Ok(())
    }
}

enum FormattedArgument<'a> {
    String(Option<&'a str>),
    Identifier { text: &'a str, bare: bool },
    Literal(Option<&'a str>),
}

impl FormattedArgument<'_> {
    fn characters(&self) -> usize {
        match *self {
            Self::String(Some(text)) => text.chars().count(),
            Self::String(None) => 0,
            Self::Identifier { text, bare: true } => text.chars().count(),
            Self::Identifier { text, bare: false } => {
                2 + text.chars().count() + text.chars().filter(|&c| c == '"').count()
            }
            Self::Literal(None) => 4,
            Self::Literal(Some(text)) => {
                2 + text.chars().count() + text.chars().filter(|&c| c == '\'').count()
            }
        }
    }

    fn write(&self, out: &mut impl core::fmt::Write) -> core::fmt::Result {
        match *self {
            Self::String(text) => out.write_str(text.unwrap_or("")),
            Self::Identifier { text, bare: true } => out.write_str(text),
            Self::Identifier { text, bare: false } => {
                out.write_char('"')?;
                for character in text.chars() {
                    if character == '"' {
                        out.write_char('"')?;
                    }
                    out.write_char(character)?;
                }
                out.write_char('"')
            }
            Self::Literal(None) => out.write_str("NULL"),
            Self::Literal(Some(text)) => {
                out.write_char('\'')?;
                for character in text.chars() {
                    if character == '\'' {
                        out.write_char('\'')?;
                    }
                    out.write_char(character)?;
                }
                out.write_char('\'')
            }
        }
    }
}

fn formatted_argument<'a>(
    specifier: u8,
    value: Datum<'a>,
    arena: &'a Arena,
) -> Result<FormattedArgument<'a>, SqlError> {
    if specifier == b's' {
        return Ok(FormattedArgument::String(
            (!value.is_null())
                .then(|| datum_to_text(value, arena))
                .transpose()?,
        ));
    }
    if specifier == b'I' {
        if value.is_null() {
            return Err(sql_err!(
                sqlstate::NULL_VALUE_NOT_ALLOWED,
                "null value cannot be formatted as SQL identifier"
            ));
        }
        let text = datum_to_text(value, arena)?;
        let bare = !text.is_empty()
            && text.bytes().enumerate().all(|(index, byte)| {
                byte == b'_' || byte.is_ascii_lowercase() || (index > 0 && byte.is_ascii_digit())
            });
        return Ok(FormattedArgument::Identifier { text, bare });
    }
    Ok(FormattedArgument::Literal(
        (!value.is_null())
            .then(|| datum_to_text(value, arena))
            .transpose()?,
    ))
}

fn write_spaces(out: &mut impl core::fmt::Write, count: usize) -> core::fmt::Result {
    for _ in 0..count {
        out.write_char(' ')?;
    }
    Ok(())
}

/// Handles the miscellaneous scalar family. Returns `None` if `name` is not one
/// of these functions, leaving the router to keep matching.
#[allow(clippy::too_many_arguments)]
pub(crate) fn dispatch<'a>(
    name: &str,
    args: &[&Expr<'a>],
    star: bool,
    variadic: bool,
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
                let mut fields = ArenaList::new(arena);
                for arg in args {
                    let expansion = match arg {
                        Expr::WholeRow(_) => Some(*arg),
                        Expr::Field { base, field: "*" } => Some(*base),
                        _ => None,
                    };
                    if let Some(base) = expansion {
                        let expanded =
                            super::super::record_star_expand(base, arena, params, row, hooks)?;
                        for field in expanded {
                            let name = stack_format!(12, "f{}", fields.len() + 1);
                            fields
                                .push(RecordField {
                                    name: arena
                                        .alloc_str(name.as_str())
                                        .map_err(|_| arena_full())?,
                                    type_oid: field.type_oid,
                                    value: field.value,
                                })
                                .map_err(|_| arena_full())?;
                        }
                    } else {
                        let value = eval_full(arg, arena, params, row, hooks)?;
                        let type_oid =
                            expression_type_identity(arg, row, hooks)?.record_field_oid();
                        let name = stack_format!(12, "f{}", fields.len() + 1);
                        fields
                            .push(RecordField {
                                name: arena.alloc_str(name.as_str()).map_err(|_| arena_full())?,
                                type_oid,
                                value,
                            })
                            .map_err(|_| arena_full())?;
                    }
                }
                Ok(Datum::Record(fields.as_slice()))
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
                Ok(Datum::Text(
                    arena.alloc_str(text.as_str()).map_err(|_| arena_full())?,
                ))
            }
            "format" => {
                if args.is_empty() {
                    return Err(arity_err(name, 0));
                }
                let Some(fmt) = text_arg(name, args, 0, arena, params, row, hooks)? else {
                    return Ok(Datum::Null);
                };
                let values = super::super::args::variadic_tail(
                    name, args, 1, variadic, arena, params, row, hooks,
                )?;
                let values = values.unwrap_or(&[]);
                let mut out = ArenaText::new(arena);
                let mut next = 0usize;
                let bytes = fmt.as_bytes();
                let mut i = 0usize;
                while i < bytes.len() {
                    if bytes[i] != b'%' {
                        let end = fmt[i..].find('%').map_or(bytes.len(), |offset| i + offset);
                        out.write_str(&fmt[i..end]).map_err(|_| arena_full())?;
                        i = end;
                        continue;
                    }
                    i += 1;
                    if bytes.get(i) == Some(&b'%') {
                        out.write_char('%').map_err(|_| arena_full())?;
                        i += 1;
                        continue;
                    }
                    if i == bytes.len() {
                        return Err(sql_err!(
                            sqlstate::INVALID_PARAMETER_VALUE,
                            "unterminated format specifier"
                        ));
                    }

                    let position_start = i;
                    let possible_position = decimal(bytes, &mut i)?;
                    let position = if possible_position.is_some() && bytes.get(i) == Some(&b'$') {
                        i += 1;
                        possible_position
                    } else {
                        i = position_start;
                        None
                    };
                    let mut left = false;
                    if bytes.get(i) == Some(&b'-') {
                        left = true;
                        i += 1;
                    }
                    let mut width = None;
                    if bytes.get(i) == Some(&b'*') {
                        i += 1;
                        let width_start = i;
                        let possible_width_position = decimal(bytes, &mut i)?;
                        let width_position =
                            if possible_width_position.is_some() && bytes.get(i) == Some(&b'$') {
                                i += 1;
                                possible_width_position
                            } else {
                                i = width_start;
                                None
                            };
                        width = match format_argument(values, width_position, &mut next)? {
                            Datum::Null => None,
                            Datum::Int2(value) => Some(i64::from(value)),
                            Datum::Int4(value) => Some(i64::from(value)),
                            Datum::Int8(value) => Some(value),
                            other => return Err(type_mismatch("format", &other)),
                        };
                    } else if let Some(written) = decimal(bytes, &mut i)? {
                        width = Some(i64::try_from(written).map_err(|_| {
                            sql_err!(
                                sqlstate::INVALID_PARAMETER_VALUE,
                                "format width is too large"
                            )
                        })?);
                    }
                    if width.is_some_and(|width| width < 0) {
                        left = true;
                    }
                    let width = width
                        .map(i64::unsigned_abs)
                        .and_then(|width| usize::try_from(width).ok())
                        .unwrap_or(0);
                    let Some(&spec) = bytes.get(i) else {
                        return Err(sql_err!(
                            sqlstate::INVALID_PARAMETER_VALUE,
                            "unterminated format specifier"
                        ));
                    };
                    i += 1;
                    if !matches!(spec, b's' | b'I' | b'L') {
                        return Err(sql_err!(
                            sqlstate::INVALID_PARAMETER_VALUE,
                            "unrecognized format() type specifier \"{}\"",
                            spec as char
                        ));
                    }
                    let value = format_argument(values, position, &mut next)?;
                    let rendered = formatted_argument(spec, value, arena)?;
                    let padding = width.saturating_sub(rendered.characters());
                    if !left {
                        write_spaces(&mut out, padding).map_err(|_| arena_full())?;
                    }
                    rendered.write(&mut out).map_err(|_| arena_full())?;
                    if left {
                        write_spaces(&mut out, padding).map_err(|_| arena_full())?;
                    }
                }
                Ok(Datum::Text(out.finish()))
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
