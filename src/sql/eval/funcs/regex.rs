//! Scalar regular-expression built-ins.
//!
//! Covers `regexp_replace`, `regexp_count`/`regexp_instr`/`regexp_substr`,
//! `regexp_like`, `regexp_split_to_array`, and the `SIMILAR TO` predicate
//! (`similar_to`). The set-returning `regexp_matches` and
//! `regexp_split_to_table` are expanded by the set-returning-function
//! machinery and stay in the router.

use core::fmt::Write;

use crate::sql::array;
use crate::sql::ast::Expr;
use crate::sql::regex;
use crate::sql::types::{ArrElem, Datum};
use crate::sql_err;
use crate::util::StackStr;

use super::super::{
    arena_full, arity_err, byte_to_char_1based, char_index_to_byte, expand_replacement, int_arg,
    regex_split, regexp_flags, similar_to_posix, sqlstate, text_arg, ColumnLookup, EvalHooks,
    SqlError,
};

/// Handles the scalar regex family. Returns `None` if `name` is not one of
/// these functions, leaving the router to keep matching.
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
        "regexp_replace"
            | "regexp_count"
            | "regexp_instr"
            | "regexp_substr"
            | "regexp_like"
            | "regexp_split_to_array"
            | "similar_to"
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
            "regexp_replace" => {
                // regexp_replace(source, pattern, replacement [, flags]).
                if !(3..=4).contains(&args.len()) {
                    return Err(arity_err(name, args.len()));
                }
                let (Some(src), Some(pat), Some(rep)) = (
                    text_arg(name, args, 0, arena, params, row, hooks)?,
                    text_arg(name, args, 1, arena, params, row, hooks)?,
                    text_arg(name, args, 2, arena, params, row, hooks)?,
                ) else {
                    return Ok(Datum::Null);
                };
                let mut global = false;
                let mut case_insensitive = false;
                if args.len() == 4 {
                    let Some(flags) = text_arg(name, args, 3, arena, params, row, hooks)? else {
                        return Ok(Datum::Null);
                    };
                    for f in flags.chars() {
                        match f {
                            'g' => global = true,
                            'i' => case_insensitive = true,
                            'c' => case_insensitive = false,
                            _ => {
                                return Err(sql_err!(
                                    "22023",
                                    "invalid regular expression option: \"{}\"",
                                    f
                                ))
                            }
                        }
                    }
                }
                let mut out = StackStr::<8192>::new();
                let mut pos = 0usize;
                let mut spans = [(-1i64, -1i64); regex::MAX_GROUPS];
                while let Some(((s, e), ng)) =
                    regex::find_captures(pat, src, pos, case_insensitive, &mut spans)?
                {
                    if out.write_str(&src[pos..s]).is_err() {
                        return Err(sql_err!("54000", "regexp_replace result too large"));
                    }
                    expand_replacement(&mut out, rep, src, s, e, &spans[..ng])?;
                    if e == s {
                        // Empty match: emit one source char and advance past it so
                        // the scan makes progress (PostgreSQL inserts between chars).
                        match src[e..].chars().next() {
                            Some(c) => {
                                let _ = out.write_char(c);
                                pos = e + c.len_utf8();
                            }
                            None => {
                                pos = e;
                                break;
                            }
                        }
                    } else {
                        pos = e;
                    }
                    if !global {
                        break;
                    }
                }
                if out.write_str(&src[pos..]).is_err() {
                    return Err(sql_err!("54000", "regexp_replace result too large"));
                }
                Ok(Datum::Text(arena.alloc_str(out.as_str()).map_err(|_| arena_full())?))
            }
            "regexp_count" | "regexp_instr" | "regexp_substr" => {
                // (source, pattern [, start [, flags]]). `start` is a 1-based
                // character position; `flags` may contain 'i' (case-insensitive).
                if !(2..=4).contains(&args.len()) {
                    return Err(arity_err(name, args.len()));
                }
                let (Some(src), Some(pat)) = (
                    text_arg(name, args, 0, arena, params, row, hooks)?,
                    text_arg(name, args, 1, arena, params, row, hooks)?,
                ) else {
                    return Ok(Datum::Null);
                };
                let start_char = if args.len() >= 3 {
                    match int_arg(name, args, 2, arena, params, row, hooks)? {
                        Some(v) => v.max(1),
                        None => return Ok(Datum::Null),
                    }
                } else {
                    1
                };
                let mut case_insensitive = false;
                if args.len() == 4 {
                    let Some(flags) = text_arg(name, args, 3, arena, params, row, hooks)? else {
                        return Ok(Datum::Null);
                    };
                    case_insensitive = flags.contains('i');
                }
                let begin = char_index_to_byte(src, (start_char - 1) as usize);
                if name == "regexp_count" {
                    let mut count = 0i32;
                    let mut pos = begin;
                    while let Some((s, e)) = regex::find(pat, src, pos, case_insensitive)? {
                        count += 1;
                        pos = if e == s {
                            match src[e..].chars().next() {
                                Some(c) => e + c.len_utf8(),
                                None => break,
                            }
                        } else {
                            e
                        };
                    }
                    return Ok(Datum::Int4(count));
                }
                match regex::find(pat, src, begin, case_insensitive)? {
                    None if name == "regexp_instr" => Ok(Datum::Int4(0)),
                    None => Ok(Datum::Null),
                    Some((s, _)) if name == "regexp_instr" => {
                        Ok(Datum::Int4(byte_to_char_1based(src, s)))
                    }
                    Some((s, e)) => {
                        Ok(Datum::Text(arena.alloc_str(&src[s..e]).map_err(|_| arena_full())?))
                    }
                }
            }
            // `regexp_like(source, pattern [, flags])`: whether the pattern matches.
            "regexp_like" => {
                if !(2..=3).contains(&args.len()) {
                    return Err(arity_err(name, args.len()));
                }
                let (Some(src), Some(pat)) = (
                    text_arg(name, args, 0, arena, params, row, hooks)?,
                    text_arg(name, args, 1, arena, params, row, hooks)?,
                ) else {
                    return Ok(Datum::Null);
                };
                let case_insensitive = if args.len() == 3 {
                    let Some(flags) = text_arg(name, args, 2, arena, params, row, hooks)? else {
                        return Ok(Datum::Null);
                    };
                    regexp_flags(flags)?.1
                } else {
                    false
                };
                Ok(Datum::Bool(regex::find(pat, src, 0, case_insensitive)?.is_some()))
            }
            // `regexp_split_to_array(source, pattern [, flags])`: split on matches.
            "regexp_split_to_array" => {
                if !(2..=3).contains(&args.len()) {
                    return Err(arity_err(name, args.len()));
                }
                let (Some(src), Some(pat)) = (
                    text_arg(name, args, 0, arena, params, row, hooks)?,
                    text_arg(name, args, 1, arena, params, row, hooks)?,
                ) else {
                    return Ok(Datum::Null);
                };
                let case_insensitive = if args.len() == 3 {
                    let Some(flags) = text_arg(name, args, 2, arena, params, row, hooks)? else {
                        return Ok(Datum::Null);
                    };
                    regexp_flags(flags)?.1
                } else {
                    false
                };
                let mut pieces = [Datum::Null; 1024];
                let n = regex_split(src, pat, case_insensitive, &mut pieces)?;
                Ok(Datum::Array {
                    element: ArrElem::Text,
                    raw: array::build(&pieces[..n], arena)?,
                })
            }
            "similar_to" => {
                // `x SIMILAR TO p`: the SQL regular-expression pattern is translated
                // to a POSIX regex anchored to the whole string, then matched by the
                // shared regex engine.
                arity(2)?;
                let (Some(text), Some(pattern)) = (
                    text_arg(name, args, 0, arena, params, row, hooks)?,
                    text_arg(name, args, 1, arena, params, row, hooks)?,
                ) else {
                    return Ok(Datum::Null);
                };
                let mut posix = StackStr::<256>::new();
                similar_to_posix(pattern, &mut posix)?;
                Ok(Datum::Bool(regex::regex_search(posix.as_str(), text, false)?))
            }
            _ => unreachable!("dispatch guard admitted an unhandled name"),
        }
    })())
}
