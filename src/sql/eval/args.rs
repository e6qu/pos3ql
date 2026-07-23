//! Reading a scalar function's arguments, and building its text result.
//!
//! Every scalar function starts by asserting its arity and reading each
//! argument as the type it needs, and PostgreSQL's error for a wrong one names
//! the function, so those two steps live here rather than being spelled out per
//! function. The text-building side is here for the same reason: `format`,
//! `quote_ident` and `quote_literal` all decide the same quoting question, and
//! deciding it once is what keeps them agreeing.

use core::fmt::Write as _;

use crate::mem::arena::Arena;
use crate::sql::numeric::Numeric;
use crate::sql::types::Datum;
use crate::sql_err;
use crate::stack_format;
use crate::util::StackStr;

use crate::sql::ast::Expr;

use super::{
    arena_full, eval_full, parse_bytea, sqlstate, type_mismatch, ColumnLookup, EvalHooks, SqlError,
};

pub(crate) fn arity_err(name: &str, got: usize) -> SqlError {
    sql_err!(
        sqlstate::UNDEFINED_FUNCTION,
        "function {}(...) with {} arguments does not exist",
        name,
        got
    )
}

/// Evaluates `args[i]` and requires text (None = SQL NULL).
#[allow(clippy::too_many_arguments)]
pub(crate) fn text_arg<'a>(
    name: &str,
    args: &[&Expr<'a>],
    i: usize,
    arena: &'a Arena,
    params: &[Datum<'a>],
    row: &impl ColumnLookup<'a>,
    hooks: &EvalHooks<'_, 'a>,
) -> Result<Option<&'a str>, SqlError> {
    match eval_full(args[i], arena, params, row, hooks)? {
        Datum::Null => Ok(None),
        Datum::Text(s) => Ok(Some(s)),
        other => Err(type_mismatch(name, &other)),
    }
}

/// Evaluates `args[i]` and requires a bytea (None = SQL NULL). An unknown text
/// literal is parsed as a bytea, as PostgreSQL's coercion does.
#[allow(clippy::too_many_arguments)]
pub(crate) fn bytea_arg<'a>(
    name: &str,
    args: &[&Expr<'a>],
    i: usize,
    arena: &'a Arena,
    params: &[Datum<'a>],
    row: &impl ColumnLookup<'a>,
    hooks: &EvalHooks<'_, 'a>,
) -> Result<Option<&'a [u8]>, SqlError> {
    match eval_full(args[i], arena, params, row, hooks)? {
        Datum::Null => Ok(None),
        Datum::Bytea(b) => Ok(Some(b)),
        Datum::Text(s) => Ok(Some(parse_bytea(s, arena)?)),
        other => Err(type_mismatch(name, &other)),
    }
}

/// Evaluates `args[i]` and requires an integer (None = SQL NULL).
#[allow(clippy::too_many_arguments)]
pub(crate) fn int_arg<'a>(
    name: &str,
    args: &[&Expr<'a>],
    i: usize,
    arena: &'a Arena,
    params: &[Datum<'a>],
    row: &impl ColumnLookup<'a>,
    hooks: &EvalHooks<'_, 'a>,
) -> Result<Option<i64>, SqlError> {
    match eval_full(args[i], arena, params, row, hooks)? {
        Datum::Null => Ok(None),
        Datum::Int4(v) => Ok(Some(v as i64)),
        Datum::Int8(v) => Ok(Some(v)),
        other => Err(type_mismatch(name, &other)),
    }
}

/// Evaluates `args[i]` and converts a numeric value to f64 (None = SQL NULL).
#[allow(clippy::too_many_arguments)]
pub(crate) fn num_f64<'a>(
    name: &str,
    args: &[&Expr<'a>],
    i: usize,
    arena: &'a Arena,
    params: &[Datum<'a>],
    row: &impl ColumnLookup<'a>,
    hooks: &EvalHooks<'_, 'a>,
) -> Result<Option<f64>, SqlError> {
    match eval_full(args[i], arena, params, row, hooks)? {
        Datum::Null => Ok(None),
        Datum::Int4(v) => Ok(Some(v as f64)),
        Datum::Int8(v) => Ok(Some(v as f64)),
        Datum::Float8(v) => Ok(Some(v)),
        Datum::Numeric(n) => Ok(Some(n.to_f64())),
        other => Err(type_mismatch(name, &other)),
    }
}

/// f64 view of an already-evaluated numeric-category datum.
pub(crate) fn datum_f64(name: &str, d: Datum<'_>) -> Result<f64, SqlError> {
    match d {
        Datum::Int4(v) => Ok(v as f64),
        Datum::Int8(v) => Ok(v as f64),
        Datum::Float8(v) => Ok(v),
        Datum::Numeric(n) => Ok(n.to_f64()),
        other => Err(type_mismatch(name, &other)),
    }
}

/// `width_bucket` for double-precision bounds; `count` buckets over [low,high]
/// (or reversed when high < low), 0 below and count+1 at/above the range.
pub(crate) fn width_bucket_f64(operator: f64, lo: f64, hi: f64, count: i64) -> i32 {
    let c = count as f64;
    let bucket = if lo < hi {
        if operator < lo {
            0
        } else if operator >= hi {
            count + 1
        } else {
            ((operator - lo) / (hi - lo) * c).floor() as i64 + 1
        }
    } else if operator > lo {
        0
    } else if operator <= hi {
        count + 1
    } else {
        ((lo - operator) / (lo - hi) * c).floor() as i64 + 1
    };
    bucket as i32
}

/// `width_bucket` with exact numeric arithmetic (matching PostgreSQL's numeric
/// form), using an integer quotient so bucket boundaries land exactly.
pub(crate) fn width_bucket_numeric(
    operator: &Numeric,
    lo: &Numeric,
    hi: &Numeric,
    count: i64,
    arena: &Arena,
) -> Result<i32, SqlError> {
    use crate::sql::numeric::{compare, mul, sub, trunc_div};
    use core::cmp::Ordering;
    if compare(lo, hi) == Ordering::Equal {
        return Err(sql_err!(sqlstate::NULL_VALUE_NOT_ALLOWED, "lower and upper bounds cannot be equal"));
    }
    let cnt = Numeric::from_i64(count, arena)?;
    let ascending = compare(lo, hi) == Ordering::Less;
    let (below, at_or_above) = if ascending {
        (compare(operator, lo) == Ordering::Less, compare(operator, hi) != Ordering::Less)
    } else {
        (compare(operator, lo) == Ordering::Greater, compare(operator, hi) != Ordering::Greater)
    };
    if below {
        return Ok(0);
    }
    if at_or_above {
        return Ok((count + 1) as i32);
    }
    // floor((|operator-lo| * count) / |hi-lo|) + 1
    let (num_a, den) = if ascending {
        (sub(operator, lo, arena)?, sub(hi, lo, arena)?)
    } else {
        (sub(lo, operator, arena)?, sub(lo, hi, arena)?)
    };
    let q = trunc_div(&mul(&num_a, &cnt, arena)?, &den, arena)?;
    Ok((q.to_i64()? + 1) as i32)
}

/// `format()` `%s`: the argument's text (NULL renders as empty).
pub(crate) fn format_append_str<'a>(
    out: &mut StackStr<4096>,
    v: Datum<'a>,
    arena: &'a Arena,
) -> Result<(), SqlError> {
    if !v.is_null() {
        let _ = out.write_str(datum_to_text(v, arena)?);
    }
    Ok(())
}

/// `format()` `%I`: a SQL identifier, double-quoted only when it is not a bare
/// lowercase identifier.
pub(crate) fn format_append_ident(out: &mut StackStr<4096>, v: Datum<'_>) -> Result<(), SqlError> {
    if v.is_null() {
        return Err(sql_err!(sqlstate::NULL_VALUE_NOT_ALLOWED, "null value cannot be formatted as SQL identifier"));
    }
    let s = match v {
        Datum::Text(s) => s,
        other => return Err(type_mismatch("format", &other)),
    };
    let bare = !s.is_empty()
        && s.bytes().enumerate().all(|(i, c)| {
            c == b'_' || c.is_ascii_lowercase() || (i > 0 && c.is_ascii_digit())
        });
    if bare {
        let _ = out.write_str(s);
    } else {
        let _ = out.write_char('"');
        for c in s.chars() {
            if c == '"' {
                let _ = out.write_char('"');
            }
            let _ = out.write_char(c);
        }
        let _ = out.write_char('"');
    }
    Ok(())
}

/// `format()` `%L`: a SQL literal — `NULL` for null, otherwise single-quoted
/// with embedded quotes doubled.
pub(crate) fn format_append_literal<'a>(
    out: &mut StackStr<4096>,
    v: Datum<'a>,
    arena: &'a Arena,
) -> Result<(), SqlError> {
    if v.is_null() {
        let _ = out.write_str("NULL");
        return Ok(());
    }
    let s = datum_to_text(v, arena)?;
    let _ = out.write_char('\'');
    for c in s.chars() {
        if c == '\'' {
            let _ = out.write_char('\'');
        }
        let _ = out.write_char(c);
    }
    let _ = out.write_char('\'');
    Ok(())
}

/// Byte offset of the 0-based character index `n` in `s` (clamped to the end).
pub(crate) fn char_index_to_byte(s: &str, n: usize) -> usize {
    s.char_indices().nth(n).map_or(s.len(), |(b, _)| b)
}

/// 1-based character position of byte offset `b` in `s`.
pub(crate) fn byte_to_char_1based(s: &str, b: usize) -> i32 {
    s[..b].chars().count() as i32 + 1
}

/// Expands a `regexp_replace` replacement string into `out`: `\&` is the whole
/// match, `\\` a literal backslash, `\` + other the literal character.
/// Capture-group backreferences (`\1`..`\9`) are rejected loudly — this engine
/// does not track capture positions.
pub(crate) fn expand_replacement(
    out: &mut StackStr<8192>,
    rep: &str,
    src: &str,
    match_start: usize,
    match_end: usize,
    spans: &[(i64, i64)],
) -> Result<(), SqlError> {
    let bytes = rep.as_bytes();
    let mut i = 0usize;
    while i < bytes.len() {
        if bytes[i] != b'\\' {
            // Copy a whole UTF-8 char.
            let c = rep[i..].chars().next().unwrap();
            let _ = out.write_char(c);
            i += c.len_utf8();
            continue;
        }
        match bytes.get(i + 1) {
            Some(b'&') => {
                let _ = out.write_str(&src[match_start..match_end]);
                i += 2;
            }
            Some(b'\\') => {
                let _ = out.write_char('\\');
                i += 2;
            }
            // `\1`..`\9`: the n-th capturing group's text. A group that did not
            // participate in the match — or a number beyond the pattern's group
            // count — contributes nothing (verified against PostgreSQL 18.4).
            Some(d) if d.is_ascii_digit() => {
                let n = (d - b'0') as usize;
                if n == 0 {
                    // `\0` is not a backreference: PostgreSQL keeps it literally.
                    let _ = out.write_str("\\0");
                } else if n <= spans.len() {
                    let (gs, ge) = spans[n - 1];
                    if gs >= 0 {
                        let _ = out.write_str(&src[gs as usize..ge as usize]);
                    }
                }
                i += 2;
            }
            Some(&c) => {
                let _ = out.write_char(c as char);
                i += 2;
            }
            None => {
                let _ = out.write_char('\\');
                i += 1;
            }
        }
    }
    Ok(())
}

/// Rejects a non-positive logarithm argument the way PostgreSQL does.
pub(crate) fn log_domain_check(n: &Numeric) -> Result<(), SqlError> {
    if n.is_zero() {
        return Err(sql_err!(sqlstate::INVALID_ARGUMENT_FOR_LOG, "cannot take logarithm of zero"));
    }
    if n.sign == crate::sql::numeric::Sign::Neg {
        return Err(sql_err!(sqlstate::INVALID_ARGUMENT_FOR_LOG, "cannot take logarithm of a negative number"));
    }
    Ok(())
}

/// Numeric view of an already-evaluated integer/numeric datum.
pub(crate) fn datum_numeric<'a>(name: &str, d: Datum<'a>, arena: &'a Arena) -> Result<Numeric<'a>, SqlError> {
    match d {
        Datum::Numeric(n) => Ok(n),
        Datum::Int4(v) => Numeric::from_i64(v as i64, arena),
        Datum::Int8(v) => Numeric::from_i64(v, arena),
        Datum::Float8(v) => Numeric::parse(stack_format!(64, "{}", v).as_str(), arena),
        other => Err(type_mismatch(name, &other)),
    }
}

/// Text form of a datum for concat-family functions.
/// A date/time value as microseconds for OVERLAPS comparison, or None for NULL
/// or a non-temporal value.
pub(crate) fn overlaps_micros(d: &Datum) -> Option<i64> {
    match d {
        Datum::Date(days) => Some(*days as i64 * 86_400_000_000),
        Datum::Timestamp(v) | Datum::Timestamptz(v) | Datum::Time(v) => Some(*v),
        _ => None,
    }
}

/// The end microseconds of an OVERLAPS pair whose start is `start`: either the
/// end value directly, or `start` advanced by an interval end.
pub(crate) fn overlaps_end_micros(start: &Datum, end: &Datum) -> Option<i64> {
    if let Datum::Interval(iv) = end {
        return overlaps_micros(start).map(|s| crate::sql::datetime::add_interval(s, *iv));
    }
    overlaps_micros(end)
}

/// Whether an identifier must be double-quoted to round-trip: it is not a bare
/// `[a-z_][a-z0-9_]*` token, or it is a keyword that would otherwise be
/// reinterpreted. "Keyword" here is PostgreSQL's own test, every category but
/// plain `unreserved` — `insert` is a keyword yet needs no quotes, while
/// `between` and `all` do.
pub(crate) fn ident_needs_quotes(s: &str) -> bool {
    let mut chars = s.chars();
    let valid = match chars.next() {
        Some(c) if c == '_' || c.is_ascii_lowercase() => {
            chars.all(|c| c == '_' || c.is_ascii_lowercase() || c.is_ascii_digit())
        }
        _ => false,
    };
    !valid || crate::sql::parser::keyword_needs_quotes(s)
}

/// Double-quotes an identifier into the arena, doubling embedded quotes. The
/// buffer lives here rather than on the huge `call()` frame.
pub(crate) fn quote_ident_str<'a>(s: &str, arena: &'a Arena) -> Result<&'a str, SqlError> {
    use core::fmt::Write as _;
    let mut out = crate::util::StackStr::<8192>::new();
    let _ = out.write_char('"');
    for c in s.chars() {
        if c == '"' {
            let _ = out.write_str("\"\"");
        } else {
            let _ = out.write_char(c);
        }
    }
    let _ = out.write_char('"');
    if out.is_truncated() {
        return Err(sql_err!(sqlstate::PROGRAM_LIMIT_EXCEEDED, "identifier too long to quote"));
    }
    arena.alloc_str(out.as_str()).map_err(|_| arena_full())
}

/// Wraps `text` as a SQL string literal into the arena, doubling single quotes
/// and backslashes (and prefixing `E` when a backslash is present).
pub(crate) fn quote_literal_str<'a>(text: &str, arena: &'a Arena) -> Result<&'a str, SqlError> {
    use core::fmt::Write as _;
    let mut out = crate::util::StackStr::<16384>::new();
    if text.contains('\\') {
        let _ = out.write_char('E');
    }
    let _ = out.write_char('\'');
    for c in text.chars() {
        match c {
            '\'' => {
                let _ = out.write_str("''");
            }
            '\\' => {
                let _ = out.write_str("\\\\");
            }
            _ => {
                let _ = out.write_char(c);
            }
        }
    }
    let _ = out.write_char('\'');
    if out.is_truncated() {
        return Err(sql_err!(sqlstate::PROGRAM_LIMIT_EXCEEDED, "literal too long to quote"));
    }
    arena.alloc_str(out.as_str()).map_err(|_| arena_full())
}

/// Splits a possibly-qualified identifier (`schema.table`, `"Weird Name".col`)
/// into its parts, folding unquoted parts to lowercase and unescaping quoted
/// ones. Returns the part count.
pub(crate) fn parse_qualified_ident<'a>(
    input: &str,
    out: &mut [Datum<'a>],
    arena: &'a Arena,
) -> Result<usize, SqlError> {
    let bad = || sql_err!(sqlstate::SYNTAX_ERROR, "string is not a valid identifier: \"{}\"", input);
    let bytes = input.as_bytes();
    let mut i = 0usize;
    let mut n = 0usize;
    loop {
        while i < bytes.len() && bytes[i].is_ascii_whitespace() {
            i += 1;
        }
        if i >= bytes.len() {
            return Err(bad());
        }
        if n == out.len() {
            return Err(sql_err!(sqlstate::PROGRAM_LIMIT_EXCEEDED, "identifier has too many parts"));
        }
        let part = if bytes[i] == b'"' {
            // Quoted part: read to the closing quote, collapsing `""` to `"`.
            i += 1;
            let mut buffer = crate::util::StackStr::<1024>::new();
            use core::fmt::Write as _;
            loop {
                match bytes.get(i) {
                    None => return Err(bad()),
                    Some(b'"') if bytes.get(i + 1) == Some(&b'"') => {
                        let _ = buffer.write_char('"');
                        i += 2;
                    }
                    Some(b'"') => {
                        i += 1;
                        break;
                    }
                    Some(&c) => {
                        let _ = buffer.write_char(c as char);
                        i += 1;
                    }
                }
            }
            arena.alloc_str(buffer.as_str()).map_err(|_| arena_full())?
        } else {
            // Unquoted part: letters/digits/underscore, folded to lowercase.
            let start = i;
            while i < bytes.len()
                && (bytes[i] == b'_' || bytes[i].is_ascii_alphanumeric() || bytes[i] >= 0x80)
            {
                i += 1;
            }
            if i == start {
                return Err(bad());
            }
            let raw = &input[start..i];
            let lower = arena.alloc_slice_with(raw.len(), |k| raw.as_bytes()[k].to_ascii_lowercase())
                .map_err(|_| arena_full())?;
            unsafe { core::str::from_utf8_unchecked(lower) }
        };
        out[n] = Datum::Text(part);
        n += 1;
        while i < bytes.len() && bytes[i].is_ascii_whitespace() {
            i += 1;
        }
        match bytes.get(i) {
            Some(b'.') => i += 1,
            None => return Ok(n),
            _ => return Err(bad()),
        }
    }
}

pub(crate) fn datum_to_text<'a>(v: Datum<'a>, arena: &'a Arena) -> Result<&'a str, SqlError> {
    match v {
        Datum::Text(s) => Ok(s),
        other => arena.alloc_str_display(other).map_err(|_| arena_full()),
    }
}

/// Concatenates text pieces into a fresh arena string of total length `total`.
pub(crate) fn alloc_text<'a>(arena: &'a Arena, parts: &[&str], total: usize) -> Result<Datum<'a>, SqlError> {
    let out = arena.alloc_slice_with(total, |_| 0u8).map_err(|_| arena_full())?;
    let mut at = 0;
    for p in parts {
        out[at..at + p.len()].copy_from_slice(p.as_bytes());
        at += p.len();
    }
    Ok(Datum::Text(unsafe { core::str::from_utf8_unchecked(out) }))
}

/// The pieces `string_to_array` and `string_to_table` split a string into.
/// PostgreSQL gives both the same rule, so it is written once: a NULL delimiter
/// splits into individual characters, an empty delimiter does not split at all,
/// an empty input yields nothing, and the caller decides separately what a
/// piece equal to its `null_string` becomes. Returns how many pieces landed in
/// `out`, which borrow from `s`.
pub(crate) fn split_pieces<'a>(
    s: &'a str,
    delimiter: Option<&str>,
    out: &mut [&'a str],
) -> Result<usize, SqlError> {
    let mut n = 0usize;
    let mut push = |piece: &'a str, n: &mut usize| -> Result<(), SqlError> {
        if *n >= out.len() {
            return Err(sql_err!(sqlstate::PROGRAM_LIMIT_EXCEEDED, "too many pieces in a split string"));
        }
        out[*n] = piece;
        *n += 1;
        Ok(())
    };
    match delimiter {
        Some("") => push(s, &mut n)?,
        Some(d) if !s.is_empty() => {
            for piece in s.split(d) {
                push(piece, &mut n)?;
            }
        }
        Some(_) => {} // empty input yields nothing
        None => {
            for (i, c) in s.char_indices() {
                push(&s[i..i + c.len_utf8()], &mut n)?;
            }
        }
    }
    Ok(n)
}
