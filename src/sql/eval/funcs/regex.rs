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
    ColumnLookup, EvalHooks, SqlError, arena_full, arity_err, byte_to_char_1based, eval_full,
    expand_replacement, int_arg, regex_split_with_options, regexp_options, similar_to_posix,
    sqlstate, text_arg,
};

fn start_byte(source: &str, one_based: i64) -> Option<usize> {
    let wanted = usize::try_from(one_based.checked_sub(1)?).ok()?;
    if wanted == 0 {
        return Some(0);
    }
    source
        .char_indices()
        .nth(wanted)
        .map(|(byte, _)| byte)
        .or_else(|| (source.chars().count() == wanted).then_some(source.len()))
}

fn invalid_parameter(name: &str, parameter: &str, value: i64) -> SqlError {
    sql_err!(
        sqlstate::INVALID_PARAMETER_VALUE,
        "invalid value for parameter \"{}\" in {}(): {}",
        parameter,
        name,
        value
    )
}

fn reject_global(name: &str, global: bool) -> Result<(), SqlError> {
    if global {
        Err(sql_err!(
            sqlstate::INVALID_PARAMETER_VALUE,
            "{}() does not support the \"global\" option",
            name
        ))
    } else {
        Ok(())
    }
}

fn nth_match(
    pattern: &str,
    source: &str,
    start: usize,
    occurrence: usize,
    options: regex::RegexOptions,
    spans: &mut [(i64, i64); regex::MAX_GROUPS],
) -> Result<Option<regex::MatchSpan>, SqlError> {
    let mut from = start;
    for current in 1..=occurrence {
        let Some(found @ ((match_start, match_end), _)) =
            regex::find_captures_with_options(pattern, source, from, options, spans)?
        else {
            return Ok(None);
        };
        if current == occurrence {
            return Ok(Some(found));
        }
        let Some(next) = regex::next_match_from(source, match_start, match_end) else {
            return Ok(None);
        };
        from = next;
    }
    Ok(None)
}

fn selected_span(
    whole: (usize, usize),
    groups: usize,
    subexpression: usize,
    spans: &[(i64, i64); regex::MAX_GROUPS],
) -> Option<(usize, usize)> {
    if subexpression == 0 {
        Some(whole)
    } else if subexpression <= groups {
        let (start, end) = spans[subexpression - 1];
        (start >= 0).then_some((start as usize, end as usize))
    } else {
        None
    }
}

/// Handles the scalar regex family. Returns `None` if `name` is not one of
/// these functions, leaving the router to keep matching.
#[allow(clippy::too_many_arguments)]
pub(crate) fn dispatch<'a>(
    written_name: &str,
    args: &[&'a Expr<'a>],
    argument_names: &[Option<&str>],
    star: bool,
    arena: &'a crate::mem::arena::Arena,
    params: &[Datum<'a>],
    row: &impl ColumnLookup<'a>,
    hooks: &EvalHooks<'_, 'a>,
) -> Option<Result<Datum<'a>, SqlError>> {
    let name = match written_name.split_once('.') {
        Some(("pg_catalog", name)) => name,
        Some(_) => return None,
        None => written_name,
    };
    if !matches!(
        name,
        "nameregexeq"
            | "nameregexne"
            | "nameicregexeq"
            | "nameicregexne"
            | "textregexeq"
            | "textregexne"
            | "texticregexeq"
            | "texticregexne"
            | "similar_to_escape"
            | "regexp_replace"
            | "regexp_count"
            | "regexp_instr"
            | "regexp_substr"
            | "regexp_like"
            | "regexp_match"
            | "regexp_split_to_array"
            | crate::sql::parser::SIMILAR_TO
    ) {
        return None;
    }
    // `f(*)` is not one of these functions whatever its arity, and the arities
    // that vary do so over a contiguous range.
    let arity_between = |lo: usize, hi: usize| -> Result<(), SqlError> {
        if args.len() < lo || args.len() > hi || star {
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
        let mut reordered = [&Expr::Null; 7];
        let args = if argument_names.iter().any(Option::is_some) {
            let parameters: &[&str] = match (name, args.len()) {
                ("regexp_replace", 3) => &["string", "pattern", "replacement"],
                ("regexp_replace", 4)
                    if argument_names
                        .iter()
                        .flatten()
                        .any(|argument| *argument == "start") =>
                {
                    &["string", "pattern", "replacement", "start"]
                }
                ("regexp_replace", 4) => &["string", "pattern", "replacement", "flags"],
                ("regexp_replace", 5) => &["string", "pattern", "replacement", "start", "N"],
                ("regexp_replace", 6) => {
                    &["string", "pattern", "replacement", "start", "N", "flags"]
                }
                ("regexp_count", 2)
                | ("regexp_like", 2)
                | ("regexp_match", 2)
                | ("regexp_split_to_array", 2) => &["string", "pattern"],
                ("regexp_count", 3) | ("regexp_instr", 3) | ("regexp_substr", 3) => {
                    &["string", "pattern", "start"]
                }
                ("regexp_count", 4) => &["string", "pattern", "start", "flags"],
                ("regexp_instr", 2) | ("regexp_substr", 2) => &["string", "pattern"],
                ("regexp_instr", 4) | ("regexp_substr", 4) => &["string", "pattern", "start", "N"],
                ("regexp_instr", 5) => &["string", "pattern", "start", "N", "endoption"],
                ("regexp_instr", 6) => &["string", "pattern", "start", "N", "endoption", "flags"],
                ("regexp_instr", 7) => &[
                    "string",
                    "pattern",
                    "start",
                    "N",
                    "endoption",
                    "flags",
                    "subexpr",
                ],
                ("regexp_substr", 5) => &["string", "pattern", "start", "N", "flags"],
                ("regexp_substr", 6) => &["string", "pattern", "start", "N", "flags", "subexpr"],
                ("regexp_like", 3) | ("regexp_match", 3) | ("regexp_split_to_array", 3) => {
                    &["string", "pattern", "flags"]
                }
                _ => return Err(arity_err(name, args.len())),
            };
            super::super::reorder_required_arguments(
                name,
                args,
                argument_names,
                parameters,
                &mut reordered[..args.len()],
            )?;
            &reordered[..args.len()]
        } else {
            args
        };
        match name {
            "nameregexeq" | "nameregexne" | "nameicregexeq" | "nameicregexne" | "textregexeq"
            | "textregexne" | "texticregexeq" | "texticregexne" => {
                if args.len() != 2 || star {
                    return Err(arity_err(name, args.len()));
                }
                let (Some(source), Some(pattern)) = (
                    text_arg(name, args, 0, arena, params, row, hooks)?,
                    text_arg(name, args, 1, arena, params, row, hooks)?,
                ) else {
                    return Ok(Datum::Null);
                };
                let insensitive = name.contains("icregex");
                let negated = name.ends_with("regexne");
                let matched = regex::regex_search(pattern, source, insensitive)?;
                Ok(Datum::Bool(matched != negated))
            }
            "similar_to_escape" => {
                if !(1..=2).contains(&args.len()) || star {
                    return Err(arity_err(name, args.len()));
                }
                let Some(pattern) = text_arg(name, args, 0, arena, params, row, hooks)? else {
                    return Ok(Datum::Null);
                };
                let escape = if args.len() == 1 {
                    Some('\\')
                } else {
                    match eval_full(args[1], arena, params, row, hooks)? {
                        Datum::Null => return Ok(Datum::Null),
                        datum => super::super::escape_char(datum)?,
                    }
                };
                let mut translated = StackStr::<256>::new();
                similar_to_posix(pattern, &mut translated, escape)?;
                Ok(Datum::Text(
                    arena
                        .alloc_str(translated.as_str())
                        .map_err(|_| arena_full())?,
                ))
            }
            "regexp_replace" => {
                if !(3..=6).contains(&args.len()) {
                    return Err(arity_err(name, args.len()));
                }
                let (Some(src), Some(pat), Some(rep)) = (
                    text_arg(name, args, 0, arena, params, row, hooks)?,
                    text_arg(name, args, 1, arena, params, row, hooks)?,
                    text_arg(name, args, 2, arena, params, row, hooks)?,
                ) else {
                    return Ok(Datum::Null);
                };
                let fourth = if args.len() == 4 {
                    Some(eval_full(args[3], arena, params, row, hooks)?)
                } else {
                    None
                };
                if fourth.is_some_and(|value| value.is_null()) {
                    return Ok(Datum::Null);
                }
                let legacy_flags =
                    args.len() == 4 && matches!(fourth, Some(Datum::Text(_) | Datum::Bpchar(_)));
                let start = if args.len() >= 4 && !legacy_flags {
                    let value = if let Some(value) = fourth {
                        match value {
                            Datum::Int2(value) => i64::from(value),
                            Datum::Int4(value) => i64::from(value),
                            Datum::Int8(value) => value,
                            Datum::Oid(value) => i64::from(value),
                            other => return Err(super::super::type_mismatch_pub(name, &other)),
                        }
                    } else {
                        let Some(value) = int_arg(name, args, 3, arena, params, row, hooks)? else {
                            return Ok(Datum::Null);
                        };
                        value
                    };
                    if value <= 0 {
                        return Err(invalid_parameter(name, "start", value));
                    }
                    value
                } else {
                    1
                };
                let mut occurrence = 1i64;
                if args.len() >= 5 {
                    let Some(value) = int_arg(name, args, 4, arena, params, row, hooks)? else {
                        return Ok(Datum::Null);
                    };
                    occurrence = value;
                    if occurrence < 0 {
                        return Err(invalid_parameter(name, "n", occurrence));
                    }
                }
                let flags = if legacy_flags {
                    match fourth {
                        Some(Datum::Text(value) | Datum::Bpchar(value)) => value,
                        _ => unreachable!(),
                    }
                } else if args.len() == 6 {
                    let Some(value) = text_arg(name, args, 5, arena, params, row, hooks)? else {
                        return Ok(Datum::Null);
                    };
                    value
                } else {
                    ""
                };
                let parsed = regexp_options(flags)?;
                if legacy_flags || args.len() == 4 {
                    occurrence = if parsed.global { 0 } else { 1 };
                }
                let mut out = StackStr::<8192>::new();
                let mut pos = 0usize;
                let Some(mut search) = start_byte(src, start) else {
                    return Ok(Datum::Text(src));
                };
                let mut matched = 0i64;
                let mut spans = [(-1i64, -1i64); regex::MAX_GROUPS];
                while let Some(((s, e), ng)) =
                    regex::find_captures_with_options(pat, src, search, parsed.options, &mut spans)?
                {
                    matched += 1;
                    let replace = occurrence == 0 || matched == occurrence;
                    if !replace {
                        let Some(next) = regex::next_match_from(src, s, e) else {
                            break;
                        };
                        search = next;
                        continue;
                    }
                    if out.write_str(&src[pos..s]).is_err() {
                        return Err(sql_err!(
                            sqlstate::PROGRAM_LIMIT_EXCEEDED,
                            "regexp_replace result too large"
                        ));
                    }
                    expand_replacement(&mut out, rep, src, s, e, &spans[..ng])?;
                    pos = e;
                    if occurrence != 0 {
                        break;
                    }
                    let Some(next) = regex::next_match_from(src, s, e) else {
                        break;
                    };
                    search = next;
                }
                if out.write_str(&src[pos..]).is_err() {
                    return Err(sql_err!(
                        sqlstate::PROGRAM_LIMIT_EXCEEDED,
                        "regexp_replace result too large"
                    ));
                }
                Ok(Datum::Text(
                    arena.alloc_str(out.as_str()).map_err(|_| arena_full())?,
                ))
            }
            "regexp_count" | "regexp_instr" | "regexp_substr" => {
                let maximum = if name == "regexp_count" {
                    4
                } else if name == "regexp_instr" {
                    7
                } else {
                    6
                };
                if !(2..=maximum).contains(&args.len()) {
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
                        Some(v) if v > 0 => v,
                        Some(v) => return Err(invalid_parameter(name, "start", v)),
                        None => return Ok(Datum::Null),
                    }
                } else {
                    1
                };
                let occurrence_index = if name == "regexp_count" {
                    None
                } else {
                    Some(3)
                };
                let occurrence = if let Some(index) =
                    occurrence_index.filter(|index| args.len() > *index)
                {
                    let Some(value) = int_arg(name, args, index, arena, params, row, hooks)? else {
                        return Ok(Datum::Null);
                    };
                    if value <= 0 {
                        return Err(invalid_parameter(name, "n", value));
                    }
                    value
                } else {
                    1
                };
                let end_option = if name == "regexp_instr" && args.len() >= 5 {
                    let Some(value) = int_arg(name, args, 4, arena, params, row, hooks)? else {
                        return Ok(Datum::Null);
                    };
                    if !matches!(value, 0 | 1) {
                        return Err(invalid_parameter(name, "endoption", value));
                    }
                    value
                } else {
                    0
                };
                let flags_index = match name {
                    "regexp_count" => 3,
                    "regexp_instr" => 5,
                    _ => 4,
                };
                let flags = if args.len() > flags_index {
                    let Some(value) = text_arg(name, args, flags_index, arena, params, row, hooks)?
                    else {
                        return Ok(Datum::Null);
                    };
                    value
                } else {
                    ""
                };
                let parsed = regexp_options(flags)?;
                reject_global(name, parsed.global)?;
                let subexpression_index = if name == "regexp_instr" { 6 } else { 5 };
                let subexpression = if name != "regexp_count" && args.len() > subexpression_index {
                    let Some(value) =
                        int_arg(name, args, subexpression_index, arena, params, row, hooks)?
                    else {
                        return Ok(Datum::Null);
                    };
                    if value < 0 {
                        return Err(invalid_parameter(name, "subexpr", value));
                    }
                    value as usize
                } else {
                    0
                };
                let Some(begin) = start_byte(src, start_char) else {
                    return Ok(if name == "regexp_instr" || name == "regexp_count" {
                        Datum::Int4(0)
                    } else {
                        Datum::Null
                    });
                };
                if name == "regexp_count" {
                    let mut count = 0i32;
                    let mut pos = begin;
                    while let Some((s, e)) =
                        regex::find_with_options(pat, src, pos, parsed.options)?
                    {
                        count += 1;
                        let Some(next) = regex::next_match_from(src, s, e) else {
                            break;
                        };
                        pos = next;
                    }
                    return Ok(Datum::Int4(count));
                }
                let mut spans = [(-1i64, -1i64); regex::MAX_GROUPS];
                let found = nth_match(
                    pat,
                    src,
                    begin,
                    occurrence as usize,
                    parsed.options,
                    &mut spans,
                )?;
                let selected = found.and_then(|(whole, groups)| {
                    selected_span(whole, groups, subexpression, &spans)
                });
                match (name, selected) {
                    ("regexp_instr", None) => Ok(Datum::Int4(0)),
                    ("regexp_instr", Some((start, end))) => Ok(Datum::Int4(byte_to_char_1based(
                        src,
                        if end_option == 1 { end } else { start },
                    ))),
                    (_, None) => Ok(Datum::Null),
                    (_, Some((start, end))) => Ok(Datum::Text(
                        arena
                            .alloc_str(&src[start..end])
                            .map_err(|_| arena_full())?,
                    )),
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
                let parsed = if args.len() == 3 {
                    let Some(flags) = text_arg(name, args, 2, arena, params, row, hooks)? else {
                        return Ok(Datum::Null);
                    };
                    regexp_options(flags)?
                } else {
                    regexp_options("")?
                };
                reject_global(name, parsed.global)?;
                Ok(Datum::Bool(
                    regex::find_with_options(pat, src, 0, parsed.options)?.is_some(),
                ))
            }
            "regexp_match" => {
                if !(2..=3).contains(&args.len()) {
                    return Err(arity_err(name, args.len()));
                }
                let (Some(src), Some(pat)) = (
                    text_arg(name, args, 0, arena, params, row, hooks)?,
                    text_arg(name, args, 1, arena, params, row, hooks)?,
                ) else {
                    return Ok(Datum::Null);
                };
                let flags = if args.len() == 3 {
                    let Some(value) = text_arg(name, args, 2, arena, params, row, hooks)? else {
                        return Ok(Datum::Null);
                    };
                    value
                } else {
                    ""
                };
                let parsed = regexp_options(flags)?;
                reject_global(name, parsed.global)?;
                let mut spans = [(-1i64, -1i64); regex::MAX_GROUPS];
                let Some(((start, end), groups)) =
                    regex::find_captures_with_options(pat, src, 0, parsed.options, &mut spans)?
                else {
                    return Ok(Datum::Null);
                };
                let mut elements = [Datum::Null; regex::MAX_GROUPS];
                let count = if groups == 0 {
                    elements[0] = Datum::Text(&src[start..end]);
                    1
                } else {
                    for (index, (group_start, group_end)) in spans[..groups].iter().enumerate() {
                        if *group_start >= 0 {
                            elements[index] =
                                Datum::Text(&src[*group_start as usize..*group_end as usize]);
                        }
                    }
                    groups
                };
                Ok(Datum::Array {
                    element: ArrElem::Text,
                    raw: array::build(&elements[..count], arena)?,
                })
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
                let parsed = if args.len() == 3 {
                    let Some(flags) = text_arg(name, args, 2, arena, params, row, hooks)? else {
                        return Ok(Datum::Null);
                    };
                    regexp_options(flags)?
                } else {
                    regexp_options("")?
                };
                reject_global(name, parsed.global)?;
                let mut pieces = [Datum::Null; 1024];
                let n = regex_split_with_options(src, pat, parsed.options, &mut pieces)?;
                Ok(Datum::Array {
                    element: ArrElem::Text,
                    raw: array::build(&pieces[..n], arena)?,
                })
            }
            crate::sql::parser::SIMILAR_TO => {
                // `x SIMILAR TO p`: the SQL regular-expression pattern is translated
                // to a POSIX regex anchored to the whole string, then matched by the
                // shared regex engine.
                arity_between(2, 3)?;
                // The matched string and pattern keep bpchar padding: SIMILAR TO
                // is pattern matching, which PostgreSQL runs on the raw value.
                let raw_text = |i: usize| -> Result<Option<&'a str>, SqlError> {
                    match eval_full(args[i], arena, params, row, hooks)? {
                        Datum::Null => Ok(None),
                        Datum::Text(s) | Datum::Bpchar(s) => Ok(Some(s)),
                        other => Err(super::super::type_mismatch_pub(name, &other)),
                    }
                };
                let (Some(text), Some(pattern)) = (raw_text(0)?, raw_text(1)?) else {
                    return Ok(Datum::Null);
                };
                // `ESCAPE c` arrives as a third argument; without it the escape
                // character is a backslash, as PostgreSQL's default is.
                let escape = match args.get(2) {
                    Some(e) => match eval_full(e, arena, params, row, hooks)? {
                        Datum::Null => return Ok(Datum::Null),
                        d => super::super::escape_char(d)?,
                    },
                    None => Some('\\'),
                };
                let mut posix = StackStr::<256>::new();
                similar_to_posix(pattern, &mut posix, escape)?;
                Ok(Datum::Bool(regex::regex_search(
                    posix.as_str(),
                    text,
                    false,
                )?))
            }
            _ => unreachable!("dispatch guard admitted an unhandled name"),
        }
    })())
}
