//! Text / string built-ins (the non-regex, non-set-returning scalar family).
//!
//! Covers length and case (`length`/`upper`/`lower`), trimming and padding
//! (`trim`/`ltrim`/`rtrim`/`btrim`, `lpad`/`rpad`), substring/position/replace
//! (`overlay`, `substr`/`substring`, `replace`, `strpos`, `split_part`,
//! `translate`), assembly (`concat`/`concat_ws`, `repeat`, `reverse`,
//! `left`/`right`, `initcap`), character codecs (`ascii`, `chr`,
//! `octet_length`, `bit_length`), `starts_with`, and identifier/literal quoting
//! (`quote_ident`, `quote_literal`/`quote_nullable`, `parse_ident`). The regex
//! forms of `substring` reuse the shared `regex_substring` helpers.

use crate::sql::array;
use crate::sql::ast::{Collation, Expr};
use crate::sql::range;
use crate::sql::types::{ArrElem, Datum};
use crate::sql::unicode::{self, NormalizationForm};
use crate::sql_err;

use super::super::{
    ColumnLookup, EvalHooks, SqlError, alloc_text, arena_full, arity_err, datum_to_text, eval_full,
    ident_needs_quotes, int_arg, overflow, parse_qualified_ident, quote_ident_str,
    quote_literal_str, regex_substring, resolved_expression_collation, sql_regex_substring,
    sqlstate, text_arg, text_view, type_mismatch,
};

fn is_cased(character: char) -> bool {
    unicode::cased(character)
}

fn is_final_sigma(input: &str, byte_offset: usize) -> bool {
    if !input[byte_offset..].starts_with('Σ') {
        return false;
    }
    let preceded = input[..byte_offset]
        .chars()
        .rev()
        .find(|character| !unicode::case_ignorable(*character))
        .is_some_and(is_cased);
    let followed = input[byte_offset + 'Σ'.len_utf8()..]
        .chars()
        .find(|character| !unicode::case_ignorable(*character))
        .is_some_and(is_cased);
    preceded && !followed
}

fn unicode_case<'a>(
    input: &str,
    uppercase: bool,
    arena: &'a crate::mem::arena::Arena,
) -> Result<&'a str, SqlError> {
    let kind = if uppercase {
        unicode::CaseKind::Upper
    } else {
        unicode::CaseKind::Lower
    };
    let output_len = input
        .char_indices()
        .map(|(offset, character)| {
            let (mapping, count) =
                unicode::case_mapping(character, kind, !uppercase && is_final_sigma(input, offset));
            mapping[..usize::from(count)]
                .iter()
                .map(|codepoint| {
                    char::from_u32(*codepoint)
                        .expect("valid case map")
                        .len_utf8()
                })
                .sum::<usize>()
        })
        .sum();
    let output = arena
        .alloc_slice_with(output_len, |_| 0u8)
        .map_err(|_| arena_full())?;
    let mut at = 0;
    for (offset, character) in input.char_indices() {
        let (mapping, count) =
            unicode::case_mapping(character, kind, !uppercase && is_final_sigma(input, offset));
        for codepoint in &mapping[..usize::from(count)] {
            let mapped = char::from_u32(*codepoint).expect("valid case map");
            at += mapped.encode_utf8(&mut output[at..]).len();
        }
    }
    Ok(unsafe { core::str::from_utf8_unchecked(output) })
}

fn ascii_case<'a>(
    input: &str,
    uppercase: bool,
    arena: &'a crate::mem::arena::Arena,
) -> Result<&'a str, SqlError> {
    let output = arena
        .alloc_slice_with(input.len(), |index| {
            if uppercase {
                input.as_bytes()[index].to_ascii_uppercase()
            } else {
                input.as_bytes()[index].to_ascii_lowercase()
            }
        })
        .map_err(|_| arena_full())?;
    Ok(unsafe { core::str::from_utf8_unchecked(output) })
}

fn uses_full_unicode_case_mapping(
    collation: Collation,
    hooks: &EvalHooks<'_, '_>,
) -> Result<bool, SqlError> {
    match hooks.catalog {
        Some(catalog) => catalog.uses_full_unicode_case_mapping(collation),
        None if collation == Collation::PgUnicodeFast => Ok(true),
        None if matches!(collation, Collation::Catalog(_)) => Err(sql_err!(
            sqlstate::FEATURE_NOT_SUPPORTED,
            "catalog collation casing is unavailable"
        )),
        None => Ok(false),
    }
}

fn to_ascii<'a>(
    input: &str,
    encoding: crate::storage::PgEncoding,
    arena: &'a crate::mem::arena::Arena,
) -> Result<&'a str, SqlError> {
    const LATIN1: &[u8; 96] = b"  cL Y  \"Ca  -R     'u .,      ?AAAAAAACEEEEIIII NOOOOOxOUUUUYTBaaaaaaaceeeeiiii nooooo/ouuuuyty";
    const LATIN2: &[u8; 96] = b" A L LS \"SSTZ-ZZ a,l'ls ,sstz\"zzRAAAALCCCEEEEIIDDNNOOOOxRUUUUYTBraaaalccceeeeiiddnnoooo/ruuuuyt.";
    const LATIN9: &[u8; 96] = b"  cL YS sCa  -R     Zu .z   EeY?AAAAAAACEEEEIIII NOOOOOxOUUUUYTBaaaaaaaceeeeiiii nooooo/ouuuuyty";
    const WIN1250: &[u8; 128] = b"  ' \"    %S<STZZ `'\"\".--  s>stzz   L A  \"CS  -RZ  ,l'u .,as L\"lzRAAAALCCCEEEEIIDDNNOOOOxRUUUUYTBraaaalccceeeeiiddnnoooo/ruuuuyt ";
    let (mapping, first) = match encoding.code() {
        8 => (&LATIN1[..], 160u8),
        9 => (&LATIN2[..], 160u8),
        16 => (&LATIN9[..], 160u8),
        29 => (&WIN1250[..], 128u8),
        _ => {
            return Err(sql_err!(
                sqlstate::FEATURE_NOT_SUPPORTED,
                "encoding conversion from {} to ASCII not supported",
                encoding.name()
            ));
        }
    };
    let output = arena
        .alloc_slice_with(input.len(), |index| {
            let byte = input.as_bytes()[index];
            if byte < 128 {
                byte
            } else if byte < first {
                b' '
            } else {
                mapping[usize::from(byte - first)]
            }
        })
        .map_err(|_| arena_full())?;
    Ok(unsafe { core::str::from_utf8_unchecked(output) })
}

/// Handles the scalar text family. Returns `None` if `name` is not one of these
/// functions, leaving the router to keep matching.
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
    if !matches!(
        name,
        "length"
            | "char_length"
            | "character_length"
            | "upper"
            | "lower"
            | "trim"
            | "btrim"
            | "ltrim"
            | "rtrim"
            | "overlay"
            | "substr"
            | "substring"
            | "replace"
            | "repeat"
            | "reverse"
            | "left"
            | "right"
            | "strpos"
            | "position"
            | "concat"
            | "concat_ws"
            | "initcap"
            | "ascii"
            | "chr"
            | "octet_length"
            | "lpad"
            | "rpad"
            | "split_part"
            | "translate"
            | "bit_length"
            | "starts_with"
            | "quote_ident"
            | "quote_literal"
            | "quote_nullable"
            | "parse_ident"
            | "normalize"
            | "is_normalized"
            | "unicode_version"
            | "unicode_assigned"
            | "unistr"
            | "casefold"
            | "to_ascii"
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
            "normalize" | "is_normalized" => {
                arity(2)?;
                let (Some(input), Some(form)) = (
                    text_arg(name, args, 0, arena, params, row, hooks)?,
                    text_arg(name, args, 1, arena, params, row, hooks)?,
                ) else {
                    return Ok(Datum::Null);
                };
                let form = NormalizationForm::parse(form)?;
                if name == "normalize" {
                    unicode::normalize(input, form, arena).map(Datum::Text)
                } else {
                    unicode::is_normalized(input, form, arena).map(Datum::Bool)
                }
            }
            "unicode_version" => {
                arity(0)?;
                Ok(Datum::Text("16.0"))
            }
            "unicode_assigned" => {
                arity(1)?;
                let Some(input) = text_arg(name, args, 0, arena, params, row, hooks)? else {
                    return Ok(Datum::Null);
                };
                Ok(Datum::Bool(input.chars().all(unicode::assigned)))
            }
            "unistr" => {
                arity(1)?;
                let Some(input) = text_arg(name, args, 0, arena, params, row, hooks)? else {
                    return Ok(Datum::Null);
                };
                unicode::unistr(input, arena).map(Datum::Text)
            }
            "casefold" => {
                arity(1)?;
                let Some(input) = text_arg(name, args, 0, arena, params, row, hooks)? else {
                    return Ok(Datum::Null);
                };
                let collation = resolved_expression_collation(args[0], row, hooks.catalog)?;
                if uses_full_unicode_case_mapping(collation, hooks)? {
                    unicode::casefold(input, arena).map(Datum::Text)
                } else {
                    let output = arena
                        .alloc_slice_with(input.len(), |index| {
                            input.as_bytes()[index].to_ascii_lowercase()
                        })
                        .map_err(|_| arena_full())?;
                    Ok(Datum::Text(unsafe {
                        core::str::from_utf8_unchecked(output)
                    }))
                }
            }
            "to_ascii" => {
                if star || !(1..=2).contains(&args.len()) {
                    return Err(arity_err(name, args.len()));
                }
                let Some(input) = text_arg(name, args, 0, arena, params, row, hooks)? else {
                    return Ok(Datum::Null);
                };
                let encoding = if args.len() == 1 {
                    crate::storage::PgEncoding::UTF8
                } else {
                    match text_view(eval_full(args[1], arena, params, row, hooks)?) {
                        Datum::Null => return Ok(Datum::Null),
                        Datum::Text(value) => {
                            crate::storage::PgEncoding::parse(value).ok_or_else(|| {
                                sql_err!(
                                    sqlstate::UNDEFINED_OBJECT,
                                    "{} is not a valid encoding name",
                                    value
                                )
                            })?
                        }
                        Datum::Int4(value) => u8::try_from(value)
                            .ok()
                            .and_then(crate::storage::PgEncoding::from_code)
                            .ok_or_else(|| {
                                sql_err!(
                                    sqlstate::UNDEFINED_OBJECT,
                                    "{} is not a valid encoding code",
                                    value
                                )
                            })?,
                        other => return Err(type_mismatch(name, &other)),
                    }
                };
                to_ascii(input, encoding, arena).map(Datum::Text)
            }
            "length" | "char_length" | "character_length" => {
                arity(1)?;
                match eval_full(args[0], arena, params, row, hooks)? {
                    Datum::Null => Ok(Datum::Null),
                    Datum::Text(s) => Ok(Datum::Int4(s.chars().count() as i32)),
                    // char(n) length ignores the blank padding.
                    Datum::Bpchar(s) => {
                        Ok(Datum::Int4(s.trim_end_matches(' ').chars().count() as i32))
                    }
                    // length of a bit string is its number of bits.
                    Datum::Bit { bits, .. } => Ok(Datum::Int4(bits.len() as i32)),
                    // length of a bytea is its number of bytes.
                    Datum::Bytea(b) => Ok(Datum::Int4(b.len() as i32)),
                    Datum::TsVector(vector) if name == "length" => {
                        crate::sql::full_text::vector_length(vector.as_str(), arena)
                            .map(Datum::Int4)
                    }
                    Datum::Geometry { kind, text }
                        if name == "length"
                            && matches!(
                                kind,
                                crate::sql::types::GeometryKind::Lseg
                                    | crate::sql::types::GeometryKind::Path
                            ) =>
                    {
                        let mut values = [0.0; 256];
                        let (count, closed) =
                            crate::sql::geometry::components(kind, text, &mut values)?;
                        let mut total = 0.0;
                        for index in (2..count).step_by(2) {
                            total += (values[index] - values[index - 2])
                                .hypot(values[index + 1] - values[index - 1]);
                        }
                        if closed && count >= 4 {
                            total += (values[0] - values[count - 2])
                                .hypot(values[1] - values[count - 1]);
                        }
                        Ok(Datum::Float8(total))
                    }
                    other => Err(sql_err!(
                        sqlstate::UNDEFINED_FUNCTION,
                        "function {}({}) does not exist",
                        name,
                        other.type_oid()
                    )),
                }
            }
            "upper" | "lower" => {
                arity(1)?;
                match text_view(eval_full(args[0], arena, params, row, hooks)?) {
                    Datum::Null => Ok(Datum::Null),
                    // On a range, lower/upper return the corresponding bound.
                    Datum::Range { text, kind } => {
                        if name == "upper" {
                            range::upper_datum(text, kind, arena)
                        } else {
                            range::lower_datum(text, kind, arena)
                        }
                    }
                    // On a multirange, the overall lower/upper bound.
                    Datum::Multirange { text, kind } => {
                        range::multirange_bound(text, kind, name == "upper", arena)
                    }
                    Datum::Text(s) => {
                        let upper = name == "upper";
                        let collation = resolved_expression_collation(args[0], row, hooks.catalog)?;
                        if uses_full_unicode_case_mapping(collation, hooks)? {
                            unicode_case(s, upper, arena).map(Datum::Text)
                        } else {
                            ascii_case(s, upper, arena).map(Datum::Text)
                        }
                    }
                    other => Err(type_mismatch(name, &other)),
                }
            }
            "trim" | "btrim" | "ltrim" | "rtrim" => {
                if star || !(1..=2).contains(&args.len()) {
                    return Err(arity_err(name, args.len()));
                }
                let Some(s) = text_arg(name, args, 0, arena, params, row, hooks)? else {
                    return Ok(Datum::Null);
                };
                let chars = if args.len() == 2 {
                    match text_arg(name, args, 1, arena, params, row, hooks)? {
                        Some(c) => c,
                        None => return Ok(Datum::Null),
                    }
                } else {
                    " "
                };
                let mut out = s;
                if name != "rtrim" {
                    out = out.trim_start_matches(|c| chars.contains(c));
                }
                if name != "ltrim" {
                    out = out.trim_end_matches(|c| chars.contains(c));
                }
                Ok(Datum::Text(out))
            }
            "overlay" => {
                // overlay(s placing r from n [for l]): replace l characters of s
                // starting at 1-based position n with r (l defaults to length(r)).
                if !(3..=4).contains(&args.len()) {
                    return Err(arity_err(name, args.len()));
                }
                let (Some(s), Some(r)) = (
                    text_arg(name, args, 0, arena, params, row, hooks)?,
                    text_arg(name, args, 1, arena, params, row, hooks)?,
                ) else {
                    return Ok(Datum::Null);
                };
                let Some(n) = int_arg(name, args, 2, arena, params, row, hooks)? else {
                    return Ok(Datum::Null);
                };
                let l = if args.len() == 4 {
                    match int_arg(name, args, 3, arena, params, row, hooks)? {
                        Some(v) => v,
                        None => return Ok(Datum::Null),
                    }
                } else {
                    r.chars().count() as i64
                };
                // Prefix = first (n-1) chars of s; suffix = s from char (n-1+l).
                let prefix_chars = (n - 1).max(0) as usize;
                let skip_to = (n - 1 + l).max(0) as usize;
                let prefix_end = s
                    .char_indices()
                    .nth(prefix_chars)
                    .map_or(s.len(), |(b, _)| b);
                let suffix_start = s.char_indices().nth(skip_to).map_or(s.len(), |(b, _)| b);
                let suffix_start = suffix_start.max(prefix_end);
                let total = prefix_end + r.len() + (s.len() - suffix_start);
                alloc_text(arena, &[&s[..prefix_end], r, &s[suffix_start..]], total)
            }
            "substr" | "substring" => {
                if star || !(2..=3).contains(&args.len()) {
                    return Err(arity_err(name, args.len()));
                }
                let Some(s) = text_arg(name, args, 0, arena, params, row, hooks)? else {
                    return Ok(Datum::Null);
                };
                // A text second argument selects the regular-expression forms:
                // `substring(str FROM posix_pattern)` and, with a third text arg,
                // `substring(str FROM sql_pattern FOR escape)`.
                let from_val = text_view(eval_full(args[1], arena, params, row, hooks)?);
                if let Datum::Text(pattern) = from_val {
                    if args.len() == 2 {
                        return regex_substring(s, pattern);
                    }
                    let escape = match text_view(eval_full(args[2], arena, params, row, hooks)?) {
                        Datum::Text(e) => e,
                        Datum::Null => return Ok(Datum::Null),
                        other => {
                            return Err(type_mismatch("substring escape must be text", &other));
                        }
                    };
                    return sql_regex_substring(s, pattern, escape, arena);
                }
                let from = match from_val {
                    Datum::Int2(v) => v as i64,
                    Datum::Int4(v) => v as i64,
                    Datum::Int8(v) => v,
                    Datum::Null => return Ok(Datum::Null),
                    other => return Err(type_mismatch(name, &other)),
                };
                let count = if args.len() == 3 {
                    match int_arg(name, args, 2, arena, params, row, hooks)? {
                        Some(c) => {
                            if c < 0 {
                                return Err(sql_err!(
                                    sqlstate::SUBSTRING_ERROR,
                                    "negative substring length not allowed"
                                ));
                            }
                            Some(c)
                        }
                        None => return Ok(Datum::Null),
                    }
                } else {
                    None
                };
                // 1-based window of character indices [max(from,1), from+count).
                let lo = from.max(1);
                let hi = count.map(|c| from.saturating_add(c));
                let mut start: Option<usize> = None;
                let mut end = s.len();
                for (k, (byte, _ch)) in (1_i64..).zip(s.char_indices()) {
                    if start.is_none() && k >= lo {
                        start = Some(byte);
                    }
                    if hi == Some(k) || hi.is_some_and(|h| k > h) {
                        end = byte;
                        break;
                    }
                }
                let start = start.unwrap_or(s.len());
                let end = end.max(start);
                Ok(Datum::Text(&s[start..end]))
            }
            "replace" => {
                arity(3)?;
                let (Some(s), Some(from), Some(to)) = (
                    text_arg(name, args, 0, arena, params, row, hooks)?,
                    text_arg(name, args, 1, arena, params, row, hooks)?,
                    text_arg(name, args, 2, arena, params, row, hooks)?,
                ) else {
                    return Ok(Datum::Null);
                };
                if from.is_empty() {
                    return Ok(Datum::Text(s));
                }
                let n = s.matches(from).count();
                let out_len = s.len() + n * to.len().saturating_sub(from.len())
                    - n * from.len().saturating_sub(to.len());
                let out = arena
                    .alloc_slice_with(out_len, |_| 0u8)
                    .map_err(|_| arena_full())?;
                let mut at = 0;
                let mut rest = s;
                while let Some(pos) = rest.find(from) {
                    out[at..at + pos].copy_from_slice(&rest.as_bytes()[..pos]);
                    at += pos;
                    out[at..at + to.len()].copy_from_slice(to.as_bytes());
                    at += to.len();
                    rest = &rest[pos + from.len()..];
                }
                out[at..at + rest.len()].copy_from_slice(rest.as_bytes());
                Ok(Datum::Text(unsafe { core::str::from_utf8_unchecked(out) }))
            }
            "repeat" => {
                arity(2)?;
                let Some(s) = text_arg(name, args, 0, arena, params, row, hooks)? else {
                    return Ok(Datum::Null);
                };
                let Some(n) = int_arg(name, args, 1, arena, params, row, hooks)? else {
                    return Ok(Datum::Null);
                };
                let n = n.max(0) as usize;
                let out_len = s.len().checked_mul(n).ok_or_else(|| overflow("text"))?;
                let out = arena
                    .alloc_slice_with(out_len, |_| 0u8)
                    .map_err(|_| arena_full())?;
                for i in 0..n {
                    out[i * s.len()..(i + 1) * s.len()].copy_from_slice(s.as_bytes());
                }
                Ok(Datum::Text(unsafe { core::str::from_utf8_unchecked(out) }))
            }
            "reverse" => {
                arity(1)?;
                let Some(s) = text_arg(name, args, 0, arena, params, row, hooks)? else {
                    return Ok(Datum::Null);
                };
                let out = arena
                    .alloc_slice_with(s.len(), |_| 0u8)
                    .map_err(|_| arena_full())?;
                let mut at = s.len();
                for c in s.chars() {
                    at -= c.len_utf8();
                    c.encode_utf8(&mut out[at..]);
                }
                Ok(Datum::Text(unsafe { core::str::from_utf8_unchecked(out) }))
            }
            "left" | "right" => {
                arity(2)?;
                let Some(s) = text_arg(name, args, 0, arena, params, row, hooks)? else {
                    return Ok(Datum::Null);
                };
                let Some(n) = int_arg(name, args, 1, arena, params, row, hooks)? else {
                    return Ok(Datum::Null);
                };
                let total = s.chars().count() as i64;
                // Negative n means "all but the last/first |n| characters".
                let take = if name == "left" {
                    if n < 0 {
                        (total + n).max(0)
                    } else {
                        n.min(total)
                    }
                } else if n < 0 {
                    (total + n).max(0)
                } else {
                    n.min(total)
                };
                let out = if name == "left" {
                    let end: usize = s
                        .char_indices()
                        .nth(take as usize)
                        .map(|(b, _)| b)
                        .unwrap_or(s.len());
                    &s[..end]
                } else {
                    let start: usize = s
                        .char_indices()
                        .nth((total - take) as usize)
                        .map(|(b, _)| b)
                        .unwrap_or(s.len());
                    &s[start..]
                };
                Ok(Datum::Text(out))
            }
            "strpos" | "position" => {
                arity(2)?;
                let (Some(s), Some(sub)) = (
                    text_arg(name, args, 0, arena, params, row, hooks)?,
                    text_arg(name, args, 1, arena, params, row, hooks)?,
                ) else {
                    return Ok(Datum::Null);
                };
                let pos = match s.find(sub) {
                    Some(byte) => s[..byte].chars().count() as i32 + 1,
                    None => 0,
                };
                Ok(Datum::Int4(pos))
            }
            "concat" => {
                // Concatenates every argument's text form, skipping NULLs.
                let mut total = 0usize;
                let mut values = [Datum::Null; array::MAX_ELEMENTS];
                let Some(values) = super::super::args::variadic_tail(
                    name,
                    args,
                    0,
                    variadic,
                    arena,
                    params,
                    row,
                    hooks,
                    &mut values,
                )?
                else {
                    return Ok(Datum::Null);
                };
                let mut parts: [&str; array::MAX_ELEMENTS] = [""; array::MAX_ELEMENTS];
                if star {
                    return Err(arity_err(name, args.len()));
                }
                let mut np = 0;
                for &v in values {
                    if v.is_null() {
                        continue;
                    }
                    let t = datum_to_text(v, arena)?;
                    parts[np] = t;
                    total += t.len();
                    np += 1;
                }
                alloc_text(arena, &parts[..np], total)
            }
            "concat_ws" => {
                if star || args.is_empty() {
                    return Err(arity_err(name, args.len()));
                }
                let sep = match text_arg(name, args, 0, arena, params, row, hooks)? {
                    Some(s) => s,
                    None => return Ok(Datum::Null),
                };
                let mut values = [Datum::Null; array::MAX_ELEMENTS];
                let Some(values) = super::super::args::variadic_tail(
                    name,
                    args,
                    1,
                    variadic,
                    arena,
                    params,
                    row,
                    hooks,
                    &mut values,
                )?
                else {
                    return Ok(Datum::Null);
                };
                let mut parts: [&str; array::MAX_ELEMENTS] = [""; array::MAX_ELEMENTS];
                let mut np = 0;
                let mut total = 0usize;
                for &v in values {
                    if v.is_null() {
                        continue;
                    }
                    if np > 0 {
                        total += sep.len();
                    }
                    let t = datum_to_text(v, arena)?;
                    parts[np] = t;
                    total += t.len();
                    np += 1;
                }
                let output = arena
                    .alloc_slice_with(total, |_| 0u8)
                    .map_err(|_| arena_full())?;
                let mut at = 0usize;
                for (index, part) in parts[..np].iter().enumerate() {
                    if index > 0 {
                        output[at..at + sep.len()].copy_from_slice(sep.as_bytes());
                        at += sep.len();
                    }
                    output[at..at + part.len()].copy_from_slice(part.as_bytes());
                    at += part.len();
                }
                Ok(Datum::Text(unsafe {
                    core::str::from_utf8_unchecked(output)
                }))
            }
            "initcap" => {
                arity(1)?;
                let Some(s) = text_arg(name, args, 0, arena, params, row, hooks)? else {
                    return Ok(Datum::Null);
                };
                let collation = resolved_expression_collation(args[0], row, hooks.catalog)?;
                let unicode_full = uses_full_unicode_case_mapping(collation, hooks)?;
                // Upper-case the first letter of each word (runs of
                // alphanumerics), lower-casing the rest.
                let out_len = if unicode_full {
                    let mut previous_alphanumeric = false;
                    s.char_indices()
                        .map(|(offset, character)| {
                            let alphanumeric = unicode::alphanumeric(character);
                            let kind = if alphanumeric && !previous_alphanumeric {
                                unicode::CaseKind::Title
                            } else {
                                unicode::CaseKind::Lower
                            };
                            let (mapping, count) =
                                unicode::case_mapping(character, kind, is_final_sigma(s, offset));
                            previous_alphanumeric = alphanumeric;
                            mapping[..usize::from(count)]
                                .iter()
                                .map(|codepoint| {
                                    char::from_u32(*codepoint)
                                        .expect("valid case map")
                                        .len_utf8()
                                })
                                .sum::<usize>()
                        })
                        .sum()
                } else {
                    s.len()
                };
                let out = arena
                    .alloc_slice_with(out_len, |_| 0u8)
                    .map_err(|_| arena_full())?;
                let mut at = 0;
                let mut prev_alnum = false;
                for (offset, c) in s.char_indices() {
                    let alphanumeric = if unicode_full {
                        unicode::alphanumeric(c)
                    } else {
                        c.is_ascii_alphanumeric()
                    };
                    if unicode_full {
                        let kind = if alphanumeric && !prev_alnum {
                            unicode::CaseKind::Title
                        } else {
                            unicode::CaseKind::Lower
                        };
                        let (mapping, count) =
                            unicode::case_mapping(c, kind, is_final_sigma(s, offset));
                        for codepoint in &mapping[..usize::from(count)] {
                            let mapped = char::from_u32(*codepoint).expect("valid case map");
                            at += mapped.encode_utf8(&mut out[at..]).len();
                        }
                    } else {
                        out[at..at + c.len_utf8()]
                            .copy_from_slice(c.encode_utf8(&mut [0; 4]).as_bytes());
                        if c.is_ascii() {
                            out[at] = if alphanumeric && !prev_alnum {
                                out[at].to_ascii_uppercase()
                            } else {
                                out[at].to_ascii_lowercase()
                            };
                        }
                        at += c.len_utf8();
                    }
                    prev_alnum = alphanumeric;
                }
                Ok(Datum::Text(unsafe {
                    core::str::from_utf8_unchecked(&out[..at])
                }))
            }
            "ascii" => {
                arity(1)?;
                let Some(s) = text_arg(name, args, 0, arena, params, row, hooks)? else {
                    return Ok(Datum::Null);
                };
                Ok(match s.chars().next() {
                    Some(c) => Datum::Int4(c as i32),
                    None => Datum::Int4(0),
                })
            }
            "chr" => {
                arity(1)?;
                let Some(n) = int_arg(name, args, 0, arena, params, row, hooks)? else {
                    return Ok(Datum::Null);
                };
                if n == 0 {
                    return Err(sql_err!(
                        sqlstate::PROGRAM_LIMIT_EXCEEDED,
                        "null character not permitted"
                    ));
                }
                let c = u32::try_from(n)
                    .ok()
                    .and_then(char::from_u32)
                    .ok_or_else(|| {
                        sql_err!(
                            sqlstate::INVALID_PARAMETER_VALUE,
                            "requested character not valid for encoding"
                        )
                    })?;
                let out = arena
                    .alloc_slice_with(c.len_utf8(), |_| 0u8)
                    .map_err(|_| arena_full())?;
                c.encode_utf8(out);
                Ok(Datum::Text(unsafe { core::str::from_utf8_unchecked(out) }))
            }
            "octet_length" => {
                arity(1)?;
                match eval_full(args[0], arena, params, row, hooks)? {
                    Datum::Null => Ok(Datum::Null),
                    Datum::Text(s) => Ok(Datum::Int4(s.len() as i32)),
                    // octet_length(bpchar) counts the padding — one of the few
                    // functions PostgreSQL gives the raw padded value.
                    Datum::Bpchar(s) => Ok(Datum::Int4(s.len() as i32)),
                    Datum::Bytea(b) => Ok(Datum::Int4(b.len() as i32)),
                    // octets needed to hold the bits.
                    Datum::Bit { bits, .. } => Ok(Datum::Int4(bits.len().div_ceil(8) as i32)),
                    other => Err(type_mismatch(name, &other)),
                }
            }
            "lpad" | "rpad" => {
                if star || !(2..=3).contains(&args.len()) {
                    return Err(arity_err(name, args.len()));
                }
                let Some(s) = text_arg(name, args, 0, arena, params, row, hooks)? else {
                    return Ok(Datum::Null);
                };
                let Some(len) = int_arg(name, args, 1, arena, params, row, hooks)? else {
                    return Ok(Datum::Null);
                };
                let fill = if args.len() == 3 {
                    match text_arg(name, args, 2, arena, params, row, hooks)? {
                        Some(f) => f,
                        None => return Ok(Datum::Null),
                    }
                } else {
                    " "
                };
                let len = len.max(0) as usize;
                let s_len = s.chars().count();
                // Longer than the target: truncate to the first `len` characters.
                if s_len >= len {
                    let end = s.char_indices().nth(len).map(|(b, _)| b).unwrap_or(s.len());
                    return Ok(Datum::Text(&s[..end]));
                }
                if fill.is_empty() {
                    return Ok(Datum::Text(s));
                }
                let pad_count = len - s_len;
                // Padding is `fill` repeated, cut to `pad_count` characters.
                let pad_len: usize = fill
                    .chars()
                    .cycle()
                    .take(pad_count)
                    .map(char::len_utf8)
                    .sum();
                let total = pad_len + s.len();
                let buffer = arena
                    .alloc_slice_with(total, |_| 0u8)
                    .map_err(|_| arena_full())?;
                let mut at = 0;
                let write_pad = |buffer: &mut [u8], at: &mut usize| {
                    for c in fill.chars().cycle().take(pad_count) {
                        *at += c.encode_utf8(&mut buffer[*at..]).len();
                    }
                };
                if name == "lpad" {
                    write_pad(buffer, &mut at);
                    buffer[at..at + s.len()].copy_from_slice(s.as_bytes());
                } else {
                    buffer[at..at + s.len()].copy_from_slice(s.as_bytes());
                    at += s.len();
                    write_pad(buffer, &mut at);
                }
                Ok(Datum::Text(unsafe {
                    core::str::from_utf8_unchecked(buffer)
                }))
            }
            "split_part" => {
                arity(3)?;
                let (Some(s), Some(delim)) = (
                    text_arg(name, args, 0, arena, params, row, hooks)?,
                    text_arg(name, args, 1, arena, params, row, hooks)?,
                ) else {
                    return Ok(Datum::Null);
                };
                let Some(n) = int_arg(name, args, 2, arena, params, row, hooks)? else {
                    return Ok(Datum::Null);
                };
                if n == 0 {
                    return Err(sql_err!(
                        sqlstate::INVALID_PARAMETER_VALUE,
                        "field position must not be zero"
                    ));
                }
                if delim.is_empty() {
                    return Ok(Datum::Text(if n == 1 || n == -1 { s } else { "" }));
                }
                let part = if n > 0 {
                    s.split(delim).nth((n - 1) as usize).unwrap_or("")
                } else {
                    let total = s.split(delim).count() as i64;
                    let index = total + n; // n is negative
                    if index < 0 {
                        ""
                    } else {
                        s.split(delim).nth(index as usize).unwrap_or("")
                    }
                };
                Ok(Datum::Text(part))
            }
            "translate" => {
                arity(3)?;
                let (Some(s), Some(from), Some(to)) = (
                    text_arg(name, args, 0, arena, params, row, hooks)?,
                    text_arg(name, args, 1, arena, params, row, hooks)?,
                    text_arg(name, args, 2, arena, params, row, hooks)?,
                ) else {
                    return Ok(Datum::Null);
                };
                // Each character of `s` that appears in `from` is replaced by the
                // char at the same index in `to`, or removed if `to` is shorter.
                let out_cap: usize = s.chars().map(|c| c.len_utf8()).sum();
                let buffer = arena
                    .alloc_slice_with(out_cap.max(1), |_| 0u8)
                    .map_err(|_| arena_full())?;
                let mut at = 0;
                for c in s.chars() {
                    match from.chars().position(|f| f == c) {
                        Some(i) => {
                            if let Some(r) = to.chars().nth(i) {
                                at += r.encode_utf8(&mut buffer[at..]).len();
                            }
                            // else: removed.
                        }
                        None => {
                            at += c.encode_utf8(&mut buffer[at..]).len();
                        }
                    }
                }
                Ok(Datum::Text(unsafe {
                    core::str::from_utf8_unchecked(&buffer[..at])
                }))
            }
            "bit_length" => {
                arity(1)?;
                match eval_full(args[0], arena, params, row, hooks)? {
                    Datum::Null => Ok(Datum::Null),
                    // A bit string's bit_length is its bit count.
                    Datum::Bit { bits, .. } => Ok(Datum::Int4(bits.len() as i32)),
                    Datum::Text(s) => Ok(Datum::Int4((s.len() as i64 * 8) as i32)),
                    Datum::Bpchar(s) => Ok(Datum::Int4(
                        (s.trim_end_matches(' ').len() as i64 * 8) as i32,
                    )),
                    Datum::Bytea(b) => Ok(Datum::Int4((b.len() as i64 * 8) as i32)),
                    other => Err(type_mismatch("bit_length", &other)),
                }
            }
            "starts_with" => {
                arity(2)?;
                let (Some(s), Some(p)) = (
                    text_arg(name, args, 0, arena, params, row, hooks)?,
                    text_arg(name, args, 1, arena, params, row, hooks)?,
                ) else {
                    return Ok(Datum::Null);
                };
                Ok(Datum::Bool(s.starts_with(p)))
            }
            // `quote_ident`: double-quote an identifier when it is not a bare
            // lowercase identifier (or is a keyword), doubling embedded quotes.
            "quote_ident" => {
                arity(1)?;
                let Some(s) = text_arg(name, args, 0, arena, params, row, hooks)? else {
                    return Ok(Datum::Null);
                };
                if ident_needs_quotes(s) {
                    Ok(Datum::Text(quote_ident_str(s, arena)?))
                } else {
                    Ok(Datum::Text(s))
                }
            }
            // `quote_literal` / `quote_nullable`: the value as a SQL literal string.
            // NULL → SQL NULL for quote_literal, the text `NULL` for quote_nullable.
            "quote_literal" | "quote_nullable" => {
                arity(1)?;
                let v = eval_full(args[0], arena, params, row, hooks)?;
                if v.is_null() {
                    return Ok(if name == "quote_nullable" {
                        Datum::Text("NULL")
                    } else {
                        Datum::Null
                    });
                }
                Ok(Datum::Text(quote_literal_str(
                    datum_to_text(text_view(v), arena)?,
                    arena,
                )?))
            }
            // `parse_ident(text)`: split a qualified name into its parts as text[].
            "parse_ident" => {
                if !(1..=2).contains(&args.len()) || star {
                    return Err(super::super::arity_err(name, args.len()));
                }
                let Some(s) = text_arg(name, args, 0, arena, params, row, hooks)? else {
                    return Ok(Datum::Null);
                };
                let strict = if args.len() == 2 {
                    match eval_full(args[1], arena, params, row, hooks)? {
                        Datum::Bool(value) => value,
                        Datum::Null => return Ok(Datum::Null),
                        other => return Err(type_mismatch(name, &other)),
                    }
                } else {
                    true
                };
                let mut parts = [Datum::Null; 64];
                let n = parse_qualified_ident(s, &mut parts, arena, strict)?;
                Ok(Datum::Array {
                    element: ArrElem::Text,
                    raw: array::build(&parts[..n], arena)?,
                })
            }
            _ => unreachable!("dispatch guard admitted an unhandled name"),
        }
    })())
}
