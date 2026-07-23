//! Range types (`int4range`/`int8range`/`numrange`/`daterange`/`tsrange`/
//! `tstzrange`). Values are carried as canonical text (see `Datum::Range`);
//! this module parses a literal or constructor into that canonical form and
//! answers the range operators/functions. Discrete kinds (int/date) are
//! canonicalized to the half-open `[lower, upper)` form, as PostgreSQL does.
//!
//! Allocation-free: intermediate bound text lives in fixed stack buffers, and
//! bound comparison (including numeric) works directly on the text with no
//! post-startup heap, so the ordering/containment operators need no arena.

use core::cmp::Ordering;
use core::fmt::Write as _;

use super::eval::SqlError;
use super::numeric::{self, Numeric};
use super::types::{Datum, RangeKind};
use crate::mem::arena::Arena;
use crate::util::StackStr;
use crate::{sql_err, stack_format};

/// A parsed range: bounds as raw element text (None = unbounded), inclusivity
/// flags, and the empty marker.
pub struct Parsed<'a> {
    pub empty: bool,
    pub lower: Option<&'a str>,
    pub upper: Option<&'a str>,
    pub lower_inc: bool,
    pub upper_inc: bool,
}

/// A range literal that does not have the shape of a range — missing a bracket,
/// the wrong number of comma-separated parts. PostgreSQL names neither the range
/// type nor the element here, only that the literal is malformed.
fn bad_literal(input: &str) -> SqlError {
    sql_err!("22P02", "malformed range literal: \"{}\"", input)
}

/// A bound that is well-placed but is not a value of the element type. This is
/// the element type's own input error, not a range error — `[a,5)::int4range`
/// fails the way `'a'::integer` does — so it names the element type and value.
/// Only the integer and numeric elements reach here; the temporal elements
/// raise their own equivalent from `parse_date` / `parse_timestamp`.
fn bad_element(kind: RangeKind, value: &str) -> SqlError {
    sql_err!(
        "22P02",
        "invalid input syntax for type {}: \"{}\"",
        kind.elem_type().name(),
        value
    )
}

/// Parses a range literal (`[1,5)` / `(,5]` / `empty`) into its components.
pub fn parse<'a>(input: &'a str) -> Result<Parsed<'a>, SqlError> {
    let t = input.trim();
    if t.eq_ignore_ascii_case("empty") {
        return Ok(Parsed { empty: true, lower: None, upper: None, lower_inc: false, upper_inc: false });
    }
    let b = t.as_bytes();
    if b.len() < 2 {
        return Err(bad_literal(input));
    }
    let lower_inc = match b[0] {
        b'[' => true,
        b'(' => false,
        _ => return Err(bad_literal(input)),
    };
    let upper_inc = match b[b.len() - 1] {
        b']' => true,
        b')' => false,
        _ => return Err(bad_literal(input)),
    };
    let inner = &t[1..t.len() - 1];
    // The separator is the first comma outside quotes: a bound may be quoted to
    // carry a character that would otherwise be structural, exactly as
    // PostgreSQL writes a timestamp bound, so the split must respect the quotes.
    let (lower_text, upper_raw) = split_bounds(inner).ok_or_else(|| bad_literal(input))?;
    // A third part — another comma outside quotes in the remainder — is a
    // malformed literal, not a bad element value in the second part.
    if split_bounds(upper_raw).is_some() {
        return Err(bad_literal(input));
    }
    let lower_text = unquote_bound(lower_text.trim());
    let upper_text = unquote_bound(upper_raw.trim());
    Ok(Parsed {
        empty: false,
        lower: if lower_text.is_empty() { None } else { Some(lower_text) },
        upper: if upper_text.is_empty() { None } else { Some(upper_text) },
        lower_inc,
        upper_inc,
    })
}

/// Splits a range's inner text at the separating comma — the first one not
/// inside a quoted bound. A backslash inside quotes escapes the next character,
/// so a quoted `\"` or `\,` does not end the bound.
fn split_bounds(inner: &str) -> Option<(&str, &str)> {
    let bytes = inner.as_bytes();
    let mut quoted = false;
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'"' => quoted = !quoted,
            b'\\' if quoted => i += 1,
            b',' if !quoted => return Some((&inner[..i], &inner[i + 1..])),
            _ => {}
        }
        i += 1;
    }
    None
}

/// Strips the surrounding quotes PostgreSQL puts around a bound that needs
/// them. The builtin range element types — integers, numerics, dates,
/// timestamps — never contain a quote or backslash of their own, so a quoted
/// bound's content is the text between the quotes with no escape to undo; a
/// stray escape is left for the element parser to reject.
fn unquote_bound(s: &str) -> &str {
    if s.len() >= 2 && s.starts_with('"') && s.ends_with('"') {
        &s[1..s.len() - 1]
    } else {
        s
    }
}

/// The canonical output text of a bound: its value parsed as the element type
/// and rendered as that type prints it — so a `tsrange` bound `2020-01-01`
/// becomes `2020-01-01 00:00:00`, which is what PostgreSQL stores and shows,
/// rather than the raw text the range literal happened to carry.
fn element_text<'a>(raw: &str, kind: RangeKind, arena: &'a Arena) -> Result<&'a str, SqlError> {
    super::eval::cast_to_text(elem_datum(raw, kind, arena)?, arena)
}

/// Whether a bound's rendered text needs quoting on output. PostgreSQL quotes a
/// bound that is empty or carries a character that would otherwise be
/// structural — whitespace, a quote, a backslash, a comma, or a bracket — which
/// for the builtin element types means the timestamp bounds, whose space forces
/// the quotes, and nothing else.
fn bound_needs_quote(text: &str) -> bool {
    text.is_empty()
        || text
            .bytes()
            .any(|b| b.is_ascii_whitespace() || matches!(b, b'"' | b'\\' | b',' | b'(' | b')' | b'[' | b']'))
}

/// Renders a bound for output: its canonical element text, quoted (with any
/// quote or backslash escaped) when that text needs it.
fn bound_out<'a>(raw: &str, kind: RangeKind, arena: &'a Arena) -> Result<&'a str, SqlError> {
    let text = element_text(raw, kind, arena)?;
    if !bound_needs_quote(text) {
        return Ok(text);
    }
    let mut quoted = StackStr::<80>::new();
    let _ = quoted.write_char('"');
    for c in text.chars() {
        if c == '"' || c == '\\' {
            let _ = quoted.write_char('\\');
        }
        let _ = quoted.write_char(c);
    }
    let _ = quoted.write_char('"');
    alloc(arena, quoted.as_str())
}


/// Canonicalizes parsed bounds and renders the canonical range text into the
/// arena. Discrete kinds become half-open `[lower, upper)`; an empty or
/// reversed-after-canonicalization range renders as `empty`.
pub fn canonical<'a>(p: &Parsed, kind: RangeKind, arena: &'a Arena) -> Result<&'a str, SqlError> {
    if p.empty {
        return alloc(arena, "empty");
    }
    if let (Some(lower_text), Some(upper_text)) = (p.lower, p.upper)
        && cmp_elem(lower_text, upper_text, kind)? == Ordering::Greater
    {
        return Err(sql_err!(
            "22000",
            "range lower bound must be less than or equal to range upper bound"
        ));
    }
    if kind.is_discrete() {
        // Convert to inclusive-lower, exclusive-upper (buffers hold any
        // incremented bound text).
        let mut lo_buf = StackStr::<48>::new();
        let mut hi_buf = StackStr::<48>::new();
        let lower_text: Option<&str> = match (p.lower, p.lower_inc) {
            (None, _) => None,
            (Some(v), true) => Some(v),
            (Some(v), false) => {
                incr_into(v, kind, &mut lo_buf)?;
                Some(lo_buf.as_str())
            }
        };
        let upper_text: Option<&str> = match (p.upper, p.upper_inc) {
            (None, _) => None,
            (Some(v), false) => Some(v),
            (Some(v), true) => {
                incr_into(v, kind, &mut hi_buf)?;
                Some(hi_buf.as_str())
            }
        };
        if let (Some(l), Some(h)) = (lower_text, upper_text)
            && cmp_elem(l, h, kind)? != Ordering::Less
        {
            return alloc(arena, "empty");
        }
        // An unbounded lower bound uses `(`; a bounded one is inclusive `[`.
        // Each bound is normalized to its element text and quoted if it needs
        // it — a no-op for the discrete kinds (integers, dates), whose text is
        // already canonical and carries no character that would force quotes.
        let lb = if lower_text.is_some() { '[' } else { '(' };
        let lower_out = match lower_text {
            Some(v) => bound_out(v, kind, arena)?,
            None => "",
        };
        let upper_out = match upper_text {
            Some(v) => bound_out(v, kind, arena)?,
            None => "",
        };
        let text = stack_format!(160, "{}{},{})", lb, lower_out, upper_out);
        return alloc(arena, text.as_str());
    }
    // Continuous: empty when bounds are equal and not both inclusive.
    if let (Some(lower_text), Some(upper_text)) = (p.lower, p.upper)
        && cmp_elem(lower_text, upper_text, kind)? == Ordering::Equal
        && !(p.lower_inc && p.upper_inc)
    {
        return alloc(arena, "empty");
    }
    // An unbounded bound is always exclusive-bracketed. Each present bound is
    // rendered as its element type prints it and quoted when that text needs it
    // — which is where a timestamp bound gains both its time-of-day and its
    // surrounding quotes, matching what PostgreSQL stores and shows.
    let lb = if p.lower.is_some() && p.lower_inc { '[' } else { '(' };
    let rb = if p.upper.is_some() && p.upper_inc { ']' } else { ')' };
    let lower_out = match p.lower {
        Some(v) => bound_out(v, kind, arena)?,
        None => "",
    };
    let upper_out = match p.upper {
        Some(v) => bound_out(v, kind, arena)?,
        None => "",
    };
    let text = stack_format!(200, "{}{},{}{}", lb, lower_out, upper_out, rb);
    alloc(arena, text.as_str())
}

/// Builds a range from a constructor `int4range(lower_text, upper_text [, flags])`.
pub fn construct<'a>(
    lower: Datum,
    upper: Datum,
    flags: Option<&str>,
    kind: RangeKind,
    arena: &'a Arena,
) -> Result<&'a str, SqlError> {
    let f = flags.unwrap_or("[)");
    let fb = f.as_bytes();
    if fb.len() != 2 {
        return Err(sql_err!("22P02", "invalid range bound flags: \"{}\"", f));
    }
    let lower_inc = fb[0] == b'[';
    let upper_inc = fb[1] == b']';
    // Bound datums render to canonical text in the arena (validated).
    let lower_text = elem_text(lower, kind, arena)?;
    let upper_text = elem_text(upper, kind, arena)?;
    let p = Parsed { empty: false, lower: lower_text, upper: upper_text, lower_inc, upper_inc };
    canonical(&p, kind, arena)
}

fn elem_text<'a>(d: Datum, kind: RangeKind, arena: &'a Arena) -> Result<Option<&'a str>, SqlError> {
    if d.is_null() {
        return Ok(None);
    }
    let s = stack_format!(64, "{}", d);
    let owned = alloc(arena, s.as_str())?;
    // Validate it parses for this kind.
    cmp_elem(owned, owned, kind)?;
    Ok(Some(owned))
}

fn alloc<'a>(arena: &'a Arena, s: &str) -> Result<&'a str, SqlError> {
    arena.alloc_str(s).map_err(|_| sql_err!("53200", "out of memory"))
}

/// Increments a discrete bound value by one (int/date) into `buffer`.
fn incr_into(v: &str, kind: RangeKind, buffer: &mut StackStr<48>) -> Result<(), SqlError> {
    buffer.clear();
    match kind {
        RangeKind::Int4 | RangeKind::Int8 => {
            let n: i64 = v.trim().parse().map_err(|_| bad_element(kind, v))?;
            let _ = write!(buffer, "{}", n + 1);
        }
        RangeKind::Date => {
            let d = super::datetime::parse_date(v.trim())?;
            let _ = write!(buffer, "{}", super::datetime::format_date(d + 1).as_str());
        }
        _ => {
            let _ = write!(buffer, "{}", v);
        }
    }
    Ok(())
}

/// Compares two bound element texts under `kind`'s subtype ordering.
fn cmp_elem(a: &str, b: &str, kind: RangeKind) -> Result<Ordering, SqlError> {
    Ok(match kind {
        RangeKind::Int4 | RangeKind::Int8 => {
            let x: i64 = a.trim().parse().map_err(|_| bad_element(kind, a))?;
            let y: i64 = b.trim().parse().map_err(|_| bad_element(kind, b))?;
            x.cmp(&y)
        }
        RangeKind::Date => super::datetime::parse_date(a.trim())?
            .cmp(&super::datetime::parse_date(b.trim())?),
        RangeKind::Ts => super::datetime::parse_timestamp(a.trim(), false)?
            .cmp(&super::datetime::parse_timestamp(b.trim(), false)?),
        RangeKind::Tstz => super::datetime::parse_timestamp(a.trim(), true)?
            .cmp(&super::datetime::parse_timestamp(b.trim(), true)?),
        RangeKind::Num => {
            // cmp fails without saying which side was malformed, so each is
            // checked to name the offending one as PostgreSQL does.
            if !numeric::valid_decimal(a.trim()) {
                return Err(bad_element(kind, a));
            }
            if !numeric::valid_decimal(b.trim()) {
                return Err(bad_element(kind, b));
            }
            numeric::cmp_decimal_str(a.trim(), b.trim()).ok_or_else(|| bad_element(kind, a))?
        }
    })
}

pub fn lower_datum<'a>(text: &str, kind: RangeKind, arena: &'a Arena) -> Result<Datum<'a>, SqlError> {
    bound_datum(text, kind, true, arena)
}
pub fn upper_datum<'a>(text: &str, kind: RangeKind, arena: &'a Arena) -> Result<Datum<'a>, SqlError> {
    bound_datum(text, kind, false, arena)
}

fn bound_datum<'a>(
    text: &str,
    kind: RangeKind,
    lower: bool,
    arena: &'a Arena,
) -> Result<Datum<'a>, SqlError> {
    let p = parse(text)?;
    let raw = if lower { p.lower } else { p.upper };
    match raw {
        None => Ok(Datum::Null),
        Some(s) => elem_datum(s, kind, arena),
    }
}

fn elem_datum<'a>(s: &str, kind: RangeKind, arena: &'a Arena) -> Result<Datum<'a>, SqlError> {
    Ok(match kind {
        RangeKind::Int4 => Datum::Int4(s.trim().parse().map_err(|_| bad_element(kind, s))?),
        RangeKind::Int8 => Datum::Int8(s.trim().parse().map_err(|_| bad_element(kind, s))?),
        RangeKind::Date => Datum::Date(super::datetime::parse_date(s.trim())?),
        RangeKind::Ts => Datum::Timestamp(super::datetime::parse_timestamp(s.trim(), false)?),
        RangeKind::Tstz => Datum::Timestamptz(super::datetime::parse_timestamp(s.trim(), true)?),
        RangeKind::Num => Datum::Numeric(Numeric::parse(s.trim(), arena)?),
    })
}

pub fn is_empty(text: &str) -> bool {
    text.trim().eq_ignore_ascii_case("empty")
}

pub fn bound_inc(text: &str, lower: bool) -> Result<bool, SqlError> {
    let p = parse(text)?;
    if p.empty {
        return Ok(false);
    }
    Ok(if lower { p.lower.is_some() && p.lower_inc } else { p.upper.is_some() && p.upper_inc })
}

/// `range @> element`.
pub fn contains_elem(text: &str, kind: RangeKind, element: &str) -> Result<bool, SqlError> {
    let p = parse(text)?;
    if p.empty {
        return Ok(false);
    }
    if let Some(lower_text) = p.lower {
        match cmp_elem(element, lower_text, kind)? {
            Ordering::Less => return Ok(false),
            Ordering::Equal if !p.lower_inc => return Ok(false),
            _ => {}
        }
    }
    if let Some(upper_text) = p.upper {
        match cmp_elem(element, upper_text, kind)? {
            Ordering::Greater => return Ok(false),
            Ordering::Equal if !p.upper_inc => return Ok(false),
            _ => {}
        }
    }
    Ok(true)
}

/// `outer @> inner` (range contains range).
pub fn contains_range(outer: &str, inner: &str, kind: RangeKind) -> Result<bool, SqlError> {
    let (parsed_outer, parsed_inner) = (parse(outer)?, parse(inner)?);
    if parsed_inner.empty {
        return Ok(true);
    }
    if parsed_outer.empty {
        return Ok(false);
    }
    Ok(lower_le(&parsed_outer, &parsed_inner, kind)? && upper_ge(&parsed_outer, &parsed_inner, kind)?)
}

/// `a && b` (ranges overlap).
pub fn overlaps(a: &str, b: &str, kind: RangeKind) -> Result<bool, SqlError> {
    let (parsed_a, parsed_b) = (parse(a)?, parse(b)?);
    if parsed_a.empty || parsed_b.empty {
        return Ok(false);
    }
    Ok(!(strictly_left(&parsed_a, &parsed_b, kind)? || strictly_left(&parsed_b, &parsed_a, kind)?))
}

fn strictly_left(a: &Parsed, b: &Parsed, kind: RangeKind) -> Result<bool, SqlError> {
    let (Some(au), Some(b_lower)) = (a.upper, b.lower) else {
        return Ok(false);
    };
    Ok(match cmp_elem(au, b_lower, kind)? {
        Ordering::Less => true,
        Ordering::Equal => !(a.upper_inc && b.lower_inc),
        Ordering::Greater => false,
    })
}

fn lower_le(outer: &Parsed, inner: &Parsed, kind: RangeKind) -> Result<bool, SqlError> {
    let Some(ol) = outer.lower else { return Ok(true) };
    let Some(il) = inner.lower else { return Ok(false) };
    Ok(match cmp_elem(ol, il, kind)? {
        Ordering::Less => true,
        Ordering::Equal => outer.lower_inc || !inner.lower_inc,
        Ordering::Greater => false,
    })
}

fn upper_ge(outer: &Parsed, inner: &Parsed, kind: RangeKind) -> Result<bool, SqlError> {
    let Some(ou) = outer.upper else { return Ok(true) };
    let Some(iu) = inner.upper else { return Ok(false) };
    Ok(match cmp_elem(ou, iu, kind)? {
        Ordering::Greater => true,
        Ordering::Equal => outer.upper_inc || !inner.upper_inc,
        Ordering::Less => false,
    })
}

/// Total order over ranges, matching PostgreSQL `range_cmp`: empty sorts before
/// any non-empty range; otherwise compare lower bounds, then upper bounds, with
/// an infinite bound and bound inclusivity broken exactly as PostgreSQL does.
/// Comparison is on bound *values* (not canonical text), so `numrange(1.0,5.0)`
/// and `numrange(1.00,5.0)` compare equal.
pub fn cmp_ranges(a: &str, b: &str, kind: RangeKind) -> Result<Ordering, SqlError> {
    let (parsed_a, parsed_b) = (parse(a)?, parse(b)?);
    match (parsed_a.empty, parsed_b.empty) {
        (true, true) => return Ok(Ordering::Equal),
        (true, false) => return Ok(Ordering::Less),
        (false, true) => return Ok(Ordering::Greater),
        (false, false) => {}
    }
    let lower_text = cmp_bound(parsed_a.lower, parsed_a.lower_inc, parsed_b.lower, parsed_b.lower_inc, true, kind)?;
    if lower_text != Ordering::Equal {
        return Ok(lower_text);
    }
    cmp_bound(parsed_a.upper, parsed_a.upper_inc, parsed_b.upper, parsed_b.upper_inc, false, kind)
}

/// Compares one bound of two ranges. `None` value denotes an infinite bound;
/// `lower` selects the direction so infinities and inclusivity ties resolve the
/// way PostgreSQL's `range_cmp_bounds` does.
fn cmp_bound(
    a_value: Option<&str>,
    a_inclusive: bool,
    b_value: Option<&str>,
    b_inclusive: bool,
    lower: bool,
    kind: RangeKind,
) -> Result<Ordering, SqlError> {
    match (a_value, b_value) {
        (None, None) => Ok(Ordering::Equal),
        (None, Some(_)) => Ok(if lower { Ordering::Less } else { Ordering::Greater }),
        (Some(_), None) => Ok(if lower { Ordering::Greater } else { Ordering::Less }),
        (Some(x), Some(y)) => Ok(match cmp_elem(x, y, kind)? {
            Ordering::Equal => match (a_inclusive, b_inclusive) {
                (true, false) => {
                    if lower {
                        Ordering::Less
                    } else {
                        Ordering::Greater
                    }
                }
                (false, true) => {
                    if lower {
                        Ordering::Greater
                    } else {
                        Ordering::Less
                    }
                }
                _ => Ordering::Equal,
            },
            other => other,
        }),
    }
}

/// `lower_inf(r)`: the range is non-empty and has no lower bound.
pub fn lower_inf(text: &str) -> Result<bool, SqlError> {
    let p = parse(text)?;
    Ok(!p.empty && p.lower.is_none())
}

/// `upper_inf(r)`: the range is non-empty and has no upper bound.
pub fn upper_inf(text: &str) -> Result<bool, SqlError> {
    let p = parse(text)?;
    Ok(!p.empty && p.upper.is_none())
}

/// `a << b`: `a` lies strictly to the left of `b` (no overlap, `a` entirely
/// below `b`). Empty ranges are never strictly left of anything.
pub fn strictly_before(a: &str, b: &str, kind: RangeKind) -> Result<bool, SqlError> {
    let (parsed_a, parsed_b) = (parse(a)?, parse(b)?);
    if parsed_a.empty || parsed_b.empty {
        return Ok(false);
    }
    strictly_left(&parsed_a, &parsed_b, kind)
}

/// `a >> b`: `a` lies strictly to the right of `b`.
pub fn strictly_after(a: &str, b: &str, kind: RangeKind) -> Result<bool, SqlError> {
    strictly_before(b, a, kind)
}

/// `a &< b`: `a` does not extend to the right of `b` (`upper(a) <= upper(b)`).
pub fn not_right_of(a: &str, b: &str, kind: RangeKind) -> Result<bool, SqlError> {
    let (parsed_a, parsed_b) = (parse(a)?, parse(b)?);
    if parsed_a.empty || parsed_b.empty {
        return Ok(false);
    }
    Ok(cmp_bound(parsed_a.upper, parsed_a.upper_inc, parsed_b.upper, parsed_b.upper_inc, false, kind)? != Ordering::Greater)
}

/// `a &> b`: `a` does not extend to the left of `b` (`lower(a) >= lower(b)`).
pub fn not_left_of(a: &str, b: &str, kind: RangeKind) -> Result<bool, SqlError> {
    let (parsed_a, parsed_b) = (parse(a)?, parse(b)?);
    if parsed_a.empty || parsed_b.empty {
        return Ok(false);
    }
    Ok(cmp_bound(parsed_a.lower, parsed_a.lower_inc, parsed_b.lower, parsed_b.lower_inc, true, kind)? != Ordering::Less)
}

/// `a -|- b`: the ranges are adjacent (disjoint with no gap between them).
pub fn adjacent(a: &str, b: &str, kind: RangeKind) -> Result<bool, SqlError> {
    let (parsed_a, parsed_b) = (parse(a)?, parse(b)?);
    if parsed_a.empty || parsed_b.empty {
        return Ok(false);
    }
    Ok(bound_adjacent(parsed_a.upper, parsed_a.upper_inc, parsed_b.lower, parsed_b.lower_inc, kind)?
        || bound_adjacent(parsed_b.upper, parsed_b.upper_inc, parsed_a.lower, parsed_a.lower_inc, kind)?)
}

/// An upper bound and a lower bound touch (same value, exactly one inclusive),
/// leaving no gap and no overlap.
fn bound_adjacent(
    uval: Option<&str>,
    uinc: bool,
    lval: Option<&str>,
    linc: bool,
    kind: RangeKind,
) -> Result<bool, SqlError> {
    let (Some(u), Some(l)) = (uval, lval) else {
        return Ok(false);
    };
    Ok(cmp_elem(u, l, kind)? == Ordering::Equal && (uinc != linc))
}

/// `a * b`: the intersection of two ranges (empty when they do not overlap).
pub fn intersect<'a>(a: &str, b: &str, kind: RangeKind, arena: &'a Arena) -> Result<&'a str, SqlError> {
    let (parsed_a, parsed_b) = (parse(a)?, parse(b)?);
    if parsed_a.empty || parsed_b.empty || !overlaps(a, b, kind)? {
        return alloc(arena, "empty");
    }
    // The more restrictive bounds: the greater lower and the lesser upper.
    let (lower_text, lo_inc) = pick_lower(&parsed_a, &parsed_b, kind, true)?;
    let (upper_text, hi_inc) = pick_upper(&parsed_a, &parsed_b, kind, true)?;
    canonical(&mk(lower_text, lo_inc, upper_text, hi_inc), kind, arena)
}

/// `a + b`: the union of two ranges. PostgreSQL requires the result to be
/// contiguous (the inputs overlap or are adjacent), else it errors.
pub fn union<'a>(a: &str, b: &str, kind: RangeKind, arena: &'a Arena) -> Result<&'a str, SqlError> {
    let (parsed_a, parsed_b) = (parse(a)?, parse(b)?);
    if parsed_a.empty {
        return canonical(&parsed_b, kind, arena);
    }
    if parsed_b.empty {
        return canonical(&parsed_a, kind, arena);
    }
    if !overlaps(a, b, kind)? && !adjacent(a, b, kind)? {
        return Err(sql_err!("22000", "result of range union would not be contiguous"));
    }
    let (lower_text, lo_inc) = pick_lower(&parsed_a, &parsed_b, kind, false)?;
    let (upper_text, hi_inc) = pick_upper(&parsed_a, &parsed_b, kind, false)?;
    canonical(&mk(lower_text, lo_inc, upper_text, hi_inc), kind, arena)
}

/// `range_merge(a, b)`: the smallest range containing both, with no contiguity
/// requirement (unlike `+`).
pub fn merge<'a>(a: &str, b: &str, kind: RangeKind, arena: &'a Arena) -> Result<&'a str, SqlError> {
    let (parsed_a, parsed_b) = (parse(a)?, parse(b)?);
    if parsed_a.empty {
        return canonical(&parsed_b, kind, arena);
    }
    if parsed_b.empty {
        return canonical(&parsed_a, kind, arena);
    }
    let (lower_text, lo_inc) = pick_lower(&parsed_a, &parsed_b, kind, false)?;
    let (upper_text, hi_inc) = pick_upper(&parsed_a, &parsed_b, kind, false)?;
    canonical(&mk(lower_text, lo_inc, upper_text, hi_inc), kind, arena)
}

/// `a - b`: `a` with the portion overlapping `b` removed. Errors when the
/// result would not be contiguous (`b` strictly inside `a`).
pub fn difference<'a>(a: &str, b: &str, kind: RangeKind, arena: &'a Arena) -> Result<&'a str, SqlError> {
    let (parsed_a, parsed_b) = (parse(a)?, parse(b)?);
    if parsed_a.empty || parsed_b.empty || !overlaps(a, b, kind)? {
        return canonical(&parsed_a, kind, arena);
    }
    // Does `b` cover `a`'s left end / right end?
    let left = cmp_bound(parsed_b.lower, parsed_b.lower_inc, parsed_a.lower, parsed_a.lower_inc, true, kind)? != Ordering::Greater;
    let right = cmp_bound(parsed_b.upper, parsed_b.upper_inc, parsed_a.upper, parsed_a.upper_inc, false, kind)? != Ordering::Less;
    match (left, right) {
        (true, true) => alloc(arena, "empty"),
        // `b` trims the left: keep `[b.upper, a.upper)` (inclusivity flipped).
        (true, false) => canonical(&mk(parsed_b.upper, !parsed_b.upper_inc, parsed_a.upper, parsed_a.upper_inc), kind, arena),
        // `b` trims the right: keep `[a.lower, b.lower)` (inclusivity flipped).
        (false, true) => canonical(&mk(parsed_a.lower, parsed_a.lower_inc, parsed_b.lower, !parsed_b.lower_inc), kind, arena),
        (false, false) => {
            Err(sql_err!("22000", "result of range difference would not be contiguous"))
        }
    }
}

/// Builds a non-empty `Parsed` from chosen bounds.
fn mk<'a>(lower_text: Option<&'a str>, lo_inc: bool, upper_text: Option<&'a str>, hi_inc: bool) -> Parsed<'a> {
    Parsed { empty: false, lower: lower_text, upper: upper_text, lower_inc: lo_inc, upper_inc: hi_inc }
}

/// Chooses one range's lower bound: the greater (more restrictive) when
/// `restrictive`, else the lesser (for union/merge).
fn pick_lower<'a>(
    a: &Parsed<'a>,
    b: &Parsed<'a>,
    kind: RangeKind,
    restrictive: bool,
) -> Result<(Option<&'a str>, bool), SqlError> {
    let c = cmp_bound(a.lower, a.lower_inc, b.lower, b.lower_inc, true, kind)?;
    let take_a = if restrictive { c == Ordering::Greater } else { c != Ordering::Greater };
    Ok(if take_a { (a.lower, a.lower_inc) } else { (b.lower, b.lower_inc) })
}

/// Chooses one range's upper bound: the lesser (more restrictive) when
/// `restrictive`, else the greater (for union/merge).
fn pick_upper<'a>(
    a: &Parsed<'a>,
    b: &Parsed<'a>,
    kind: RangeKind,
    restrictive: bool,
) -> Result<(Option<&'a str>, bool), SqlError> {
    let c = cmp_bound(a.upper, a.upper_inc, b.upper, b.upper_inc, false, kind)?;
    let take_a = if restrictive { c == Ordering::Less } else { c != Ordering::Less };
    Ok(if take_a { (a.upper, a.upper_inc) } else { (b.upper, b.upper_inc) })
}

// ── Multirange support ──────────────────────────────────────────────────────

/// Upper bound on the number of component ranges a multirange may hold.
/// Exceeding it is a loud error, never silent truncation.
pub const MAX_MULTIRANGE: usize = 64;

/// Splits a canonical multirange text `{r1,r2,...}` into its component range
/// texts (no canonicalization; input is assumed already canonical). Commas
/// inside a component's brackets are not separators. Returns the count.
fn split_components<'a>(text: &'a str, out: &mut [&'a str; MAX_MULTIRANGE]) -> Result<usize, SqlError> {
    let bad = || sql_err!("22P02", "malformed multirange literal: \"{}\"", text);
    let inner = text
        .trim()
        .strip_prefix('{')
        .and_then(|s| s.strip_suffix('}'))
        .ok_or_else(bad)?
        .trim();
    if inner.is_empty() {
        return Ok(0);
    }
    let bytes = inner.as_bytes();
    let mut depth = 0i32;
    let mut start = 0usize;
    let mut n = 0usize;
    let mut push = |seg: &'a str, n: &mut usize| -> Result<(), SqlError> {
        if *n == MAX_MULTIRANGE {
            return Err(sql_err!("54000", "multirange has too many component ranges"));
        }
        out[*n] = seg.trim();
        *n += 1;
        Ok(())
    };
    for (i, &b) in bytes.iter().enumerate() {
        match b {
            b'[' | b'(' => depth += 1,
            b']' | b')' => depth -= 1,
            b',' if depth == 0 => {
                push(&inner[start..i], &mut n)?;
                start = i + 1;
            }
            _ => {}
        }
    }
    push(&inner[start..], &mut n)?;
    Ok(n)
}

/// Sorts (by lower bound) and merges overlapping/adjacent component ranges into
/// the canonical multirange text. `ranges` must already be canonical, non-empty
/// component ranges.
pub fn canonicalize_multirange<'a>(
    ranges: &mut [&'a str],
    kind: RangeKind,
    arena: &'a Arena,
) -> Result<&'a str, SqlError> {
    let n = ranges.len();
    // Allocation-free insertion sort by full-range order.
    for i in 1..n {
        let mut j = i;
        while j > 0 && cmp_ranges(ranges[j - 1], ranges[j], kind)? == Ordering::Greater {
            ranges.swap(j - 1, j);
            j -= 1;
        }
    }
    // Merge overlapping or adjacent neighbours (input is sorted).
    let mut merged: [&str; MAX_MULTIRANGE] = [""; MAX_MULTIRANGE];
    let mut k = 0usize;
    for &r in ranges.iter() {
        if k == 0 {
            merged[0] = r;
            k = 1;
            continue;
        }
        if overlaps(merged[k - 1], r, kind)? || adjacent(merged[k - 1], r, kind)? {
            merged[k - 1] = merge(merged[k - 1], r, kind, arena)?;
        } else {
            merged[k] = r;
            k += 1;
        }
    }
    render_multirange(&merged[..k], arena)
}

/// Renders component range texts as `{r1,r2,...}` (or `{}`) into the arena.
fn render_multirange<'a>(ranges: &[&str], arena: &'a Arena) -> Result<&'a str, SqlError> {
    let mut buf = StackStr::<1024>::new();
    let _ = buf.write_char('{');
    for (i, r) in ranges.iter().enumerate() {
        if i > 0 {
            let _ = buf.write_char(',');
        }
        let _ = buf.write_str(r);
    }
    let _ = buf.write_char('}');
    if buf.is_truncated() {
        return Err(sql_err!("54000", "multirange value too large"));
    }
    alloc(arena, buf.as_str())
}

/// Parses a multirange literal `{ r1, r2, ... }` (or `{}`) into canonical form:
/// each component canonicalized, empty components dropped, then sorted and
/// overlapping/adjacent components merged.
pub fn parse_multirange<'a>(
    input: &str,
    kind: RangeKind,
    arena: &'a Arena,
) -> Result<&'a str, SqlError> {
    let mut raw: [&str; MAX_MULTIRANGE] = [""; MAX_MULTIRANGE];
    let n = split_components(input, &mut raw)?;
    let mut canon: [&str; MAX_MULTIRANGE] = [""; MAX_MULTIRANGE];
    let mut m = 0usize;
    for &c in &raw[..n] {
        let p = parse(c)?;
        let cx = canonical(&p, kind, arena)?;
        if cx != "empty" {
            canon[m] = cx;
            m += 1;
        }
    }
    canonicalize_multirange(&mut canon[..m], kind, arena)
}

/// Wraps a single (canonical) range as a one-element multirange; an empty range
/// yields the empty multirange `{}`.
pub fn multirange_from_range<'a>(
    range_text: &str,
    _kind: RangeKind,
    arena: &'a Arena,
) -> Result<&'a str, SqlError> {
    if is_empty(range_text) {
        return alloc(arena, "{}");
    }
    render_multirange(&[range_text], arena)
}

/// The overall lower (`upper=false`) or upper (`upper=true`) bound element of a
/// multirange — the lower bound of its first component or the upper bound of its
/// last. An empty multirange has no bound (NULL).
pub fn multirange_bound<'a>(
    text: &str,
    kind: RangeKind,
    upper: bool,
    arena: &'a Arena,
) -> Result<Datum<'a>, SqlError> {
    let mut comps: [&str; MAX_MULTIRANGE] = [""; MAX_MULTIRANGE];
    let n = split_components(text, &mut comps)?;
    if n == 0 {
        return Ok(Datum::Null);
    }
    if upper {
        upper_datum(comps[n - 1], kind, arena)
    } else {
        lower_datum(comps[0], kind, arena)
    }
}

/// `A + B`: the union of two multiranges (all components merged).
pub fn multirange_union<'a>(a: &'a str, b: &'a str, kind: RangeKind, arena: &'a Arena) -> Result<&'a str, SqlError> {
    let mut comps: [&str; MAX_MULTIRANGE * 2] = [""; MAX_MULTIRANGE * 2];
    let mut ca: [&str; MAX_MULTIRANGE] = [""; MAX_MULTIRANGE];
    let na = split_components(a, &mut ca)?;
    let mut cb: [&str; MAX_MULTIRANGE] = [""; MAX_MULTIRANGE];
    let nb = split_components(b, &mut cb)?;
    if na + nb > MAX_MULTIRANGE * 2 {
        return Err(sql_err!("54000", "multirange has too many component ranges"));
    }
    comps[..na].copy_from_slice(&ca[..na]);
    comps[na..na + nb].copy_from_slice(&cb[..nb]);
    canonicalize_multirange(&mut comps[..na + nb], kind, arena)
}

/// `A * B`: the intersection of two multiranges (pairwise component overlaps).
pub fn multirange_intersect<'a>(a: &'a str, b: &'a str, kind: RangeKind, arena: &'a Arena) -> Result<&'a str, SqlError> {
    let mut ca: [&str; MAX_MULTIRANGE] = [""; MAX_MULTIRANGE];
    let na = split_components(a, &mut ca)?;
    let mut cb: [&str; MAX_MULTIRANGE] = [""; MAX_MULTIRANGE];
    let nb = split_components(b, &mut cb)?;
    let mut out: [&str; MAX_MULTIRANGE] = [""; MAX_MULTIRANGE];
    let mut n = 0usize;
    for &ai in &ca[..na] {
        for &bj in &cb[..nb] {
            let x = intersect(ai, bj, kind, arena)?;
            if x != "empty" {
                if n == MAX_MULTIRANGE {
                    return Err(sql_err!("54000", "multirange has too many component ranges"));
                }
                out[n] = x;
                n += 1;
            }
        }
    }
    canonicalize_multirange(&mut out[..n], kind, arena)
}

/// `range_a` minus `range_b` as up to two canonical pieces (dropped when empty).
fn range_minus<'a>(
    a: &Parsed<'a>,
    b: &Parsed<'a>,
    kind: RangeKind,
    arena: &'a Arena,
    out: &mut [&'a str; 2],
) -> Result<usize, SqlError> {
    let mut n = 0usize;
    // Left remnant: [a.lower, b.lower). Present only if b is bounded below.
    if b.lower.is_some() {
        let left = canonical(&mk(a.lower, a.lower_inc, b.lower, !b.lower_inc), kind, arena)?;
        if left != "empty" {
            out[n] = left;
            n += 1;
        }
    }
    // Right remnant: [b.upper, a.upper). Present only if b is bounded above.
    if b.upper.is_some() {
        let right = canonical(&mk(b.upper, !b.upper_inc, a.upper, a.upper_inc), kind, arena)?;
        if right != "empty" {
            out[n] = right;
            n += 1;
        }
    }
    Ok(n)
}

/// `A - B`: `A` with every point of `B` removed.
pub fn multirange_difference<'a>(a: &'a str, b: &'a str, kind: RangeKind, arena: &'a Arena) -> Result<&'a str, SqlError> {
    let mut ca: [&str; MAX_MULTIRANGE] = [""; MAX_MULTIRANGE];
    let na = split_components(a, &mut ca)?;
    let mut cb: [&str; MAX_MULTIRANGE] = [""; MAX_MULTIRANGE];
    let nb = split_components(b, &mut cb)?;
    // Working set of surviving pieces, grown as B's components split them.
    let mut pieces: [&str; MAX_MULTIRANGE * 2] = [""; MAX_MULTIRANGE * 2];
    let mut np = na;
    pieces[..na].copy_from_slice(&ca[..na]);
    for &bj in &cb[..nb] {
        let parsed_b = parse(bj)?;
        let mut next: [&str; MAX_MULTIRANGE * 2] = [""; MAX_MULTIRANGE * 2];
        let mut nn = 0usize;
        let mut push = |s: &'a str, nn: &mut usize| -> Result<(), SqlError> {
            if *nn == MAX_MULTIRANGE * 2 {
                return Err(sql_err!("54000", "multirange has too many component ranges"));
            }
            next[*nn] = s;
            *nn += 1;
            Ok(())
        };
        for &piece in &pieces[..np] {
            if overlaps(piece, bj, kind)? {
                let mut rem: [&str; 2] = [""; 2];
                let k = range_minus(&parse(piece)?, &parsed_b, kind, arena, &mut rem)?;
                for &r in &rem[..k] {
                    push(r, &mut nn)?;
                }
            } else {
                push(piece, &mut nn)?;
            }
        }
        pieces = next;
        np = nn;
    }
    canonicalize_multirange(&mut pieces[..np], kind, arena)
}

/// `A && B`: whether two multiranges share any point.
pub fn multirange_overlaps(a: &str, b: &str, kind: RangeKind) -> Result<bool, SqlError> {
    let mut ca: [&str; MAX_MULTIRANGE] = [""; MAX_MULTIRANGE];
    let na = split_components(a, &mut ca)?;
    let mut cb: [&str; MAX_MULTIRANGE] = [""; MAX_MULTIRANGE];
    let nb = split_components(b, &mut cb)?;
    for &ai in &ca[..na] {
        for &bj in &cb[..nb] {
            if overlaps(ai, bj, kind)? {
                return Ok(true);
            }
        }
    }
    Ok(false)
}

/// `A @> B` for two multiranges: every point of `B` lies in `A` (i.e. `A ∩ B`
/// equals `B`).
pub fn multirange_contains_multirange<'a>(a: &'a str, b: &'a str, kind: RangeKind, arena: &'a Arena) -> Result<bool, SqlError> {
    // Reuse the arena for the intermediate; compare canonical texts.
    let inter = multirange_intersect(a, b, kind, arena)?;
    // Canonicalize `b` for a like-for-like comparison.
    let bcanon = parse_multirange(b, kind, arena)?;
    Ok(inter == bcanon)
}

/// `A @> element`: some component range contains the element text.
pub fn multirange_contains_elem(a: &str, kind: RangeKind, element: &str) -> Result<bool, SqlError> {
    let mut ca: [&str; MAX_MULTIRANGE] = [""; MAX_MULTIRANGE];
    let na = split_components(a, &mut ca)?;
    for &ai in &ca[..na] {
        if contains_elem(ai, kind, element)? {
            return Ok(true);
        }
    }
    Ok(false)
}

/// Total order over two canonical multiranges: compare component ranges
/// pairwise; when one is a prefix of the other, the shorter sorts first.
pub fn cmp_multiranges(a: &str, b: &str, kind: RangeKind) -> Result<Ordering, SqlError> {
    let mut ca: [&str; MAX_MULTIRANGE] = [""; MAX_MULTIRANGE];
    let na = split_components(a, &mut ca)?;
    let mut cb: [&str; MAX_MULTIRANGE] = [""; MAX_MULTIRANGE];
    let nb = split_components(b, &mut cb)?;
    for i in 0..na.min(nb) {
        let o = cmp_ranges(ca[i], cb[i], kind)?;
        if o != Ordering::Equal {
            return Ok(o);
        }
    }
    Ok(na.cmp(&nb))
}

#[cfg(test)]
mod tests {
    use super::*;
    use RangeKind::{Int4, Num};

    fn error_of(literal: &str, kind: RangeKind) -> String {
        let arena = mini_arena();
        // The message a cast to this range kind would raise. `canonical` is
        // where both structural and element errors surface.
        let p = match parse(literal) {
            Err(e) => return e.message.as_str().to_string(),
            Ok(p) => p,
        };
        match canonical(&p, kind, &arena) {
            Err(e) => e.message.as_str().to_string(),
            Ok(_) => String::from("<no error>"),
        }
    }

    #[test]
    fn a_malformed_literal_names_neither_type_nor_element() {
        // Structural errors carry only "malformed range literal", as PostgreSQL
        // does — not the range type name it once wrongly included.
        assert_eq!(error_of("garbage", Int4), "malformed range literal: \"garbage\"");
        assert_eq!(error_of("1,5)", Int4), "malformed range literal: \"1,5)\"");
        assert_eq!(error_of("[1,5", Int4), "malformed range literal: \"[1,5\"");
        // A third comma-separated part is structural, not a bad second bound.
        assert_eq!(error_of("[1,2,3]", Int4), "malformed range literal: \"[1,2,3]\"");
    }

    #[test]
    fn a_bad_bound_is_the_element_types_own_input_error() {
        // A well-placed but invalid bound raises the element type's error,
        // naming the type and the offending value — the way `\'a\'::integer`
        // does — rather than a range error.
        assert_eq!(error_of("[a,5)", Int4), "invalid input syntax for type integer: \"a\"");
        assert_eq!(error_of("[5,z)", RangeKind::Int8), "invalid input syntax for type bigint: \"z\"");
        assert_eq!(error_of("[1.5,x)", Num), "invalid input syntax for type numeric: \"x\"");
        // Whichever side is bad is the one named.
        assert_eq!(error_of("[x,5)", Num), "invalid input syntax for type numeric: \"x\"");
        assert_eq!(error_of("[1,bad)", Int4), "invalid input syntax for type integer: \"bad\"");
    }

    #[test]
    fn cmp_ranges_orders_by_bounds() {
        // Lower bound decides first, then upper bound.
        assert_eq!(cmp_ranges("[1,5)", "[1,6)", Int4).unwrap(), Ordering::Less);
        assert_eq!(cmp_ranges("[1,5)", "[2,3)", Int4).unwrap(), Ordering::Less);
        assert_eq!(cmp_ranges("[1,10)", "[1,5)", Int4).unwrap(), Ordering::Greater);
        assert_eq!(cmp_ranges("[1,5)", "[1,5)", Int4).unwrap(), Ordering::Equal);
    }

    #[test]
    fn multirange_canonicalizes() {
        let arena = mini_arena();
        // Sorted, overlapping and adjacent components merged, empties dropped.
        assert_eq!(parse_multirange("{[5,7),[1,3)}", Int4, &arena).unwrap(), "{[1,3),[5,7)}");
        assert_eq!(parse_multirange("{[1,3),[2,5)}", Int4, &arena).unwrap(), "{[1,5)}");
        assert_eq!(parse_multirange("{[1,3),[3,5)}", Int4, &arena).unwrap(), "{[1,5)}");
        assert_eq!(parse_multirange("{[1,1)}", Int4, &arena).unwrap(), "{}");
        assert_eq!(parse_multirange("{}", Int4, &arena).unwrap(), "{}");
    }

    #[test]
    fn multirange_set_operations() {
        let arena = mini_arena();
        assert_eq!(multirange_union("{[1,5)}", "{[10,15)}", Int4, &arena).unwrap(), "{[1,5),[10,15)}");
        assert_eq!(multirange_intersect("{[1,10)}", "{[5,15)}", Int4, &arena).unwrap(), "{[5,10)}");
        // Removing a middle chunk splits into two components.
        assert_eq!(multirange_difference("{[1,10)}", "{[3,5)}", Int4, &arena).unwrap(), "{[1,3),[5,10)}");
        assert!(multirange_overlaps("{[1,5)}", "{[4,8)}", Int4).unwrap());
        assert!(!multirange_overlaps("{[1,5)}", "{[6,8)}", Int4).unwrap());
        assert!(multirange_contains_multirange("{[1,10)}", "{[2,4)}", Int4, &arena).unwrap());
        assert!(!multirange_contains_multirange("{[1,5)}", "{[2,8)}", Int4, &arena).unwrap());
    }

    #[test]
    fn cmp_ranges_empty_sorts_first() {
        assert_eq!(cmp_ranges("empty", "[1,2)", Int4).unwrap(), Ordering::Less);
        assert_eq!(cmp_ranges("[1,2)", "empty", Int4).unwrap(), Ordering::Greater);
        assert_eq!(cmp_ranges("empty", "empty", Int4).unwrap(), Ordering::Equal);
    }

    #[test]
    fn cmp_ranges_infinite_bounds() {
        // Unbounded lower is smallest; unbounded upper is largest.
        assert_eq!(cmp_ranges("(,5)", "[1,5)", Int4).unwrap(), Ordering::Less);
        assert_eq!(cmp_ranges("[1,)", "[1,10)", Int4).unwrap(), Ordering::Greater);
    }

    #[test]
    fn cmp_ranges_numrange_is_value_based() {
        // 1.0 and 1.00 are equal by value, so equal ranges compare equal.
        assert_eq!(cmp_ranges("[1.0,5.0)", "[1.00,5.0)", Num).unwrap(), Ordering::Equal);
        assert_eq!(cmp_ranges("[1.0,5.0)", "[1.0,5.1)", Num).unwrap(), Ordering::Less);
        assert_eq!(cmp_ranges("[-5.0,-1.0)", "[-5.0,-0.5)", Num).unwrap(), Ordering::Less);
    }

    #[test]
    fn contains_and_overlaps() {
        assert!(contains_elem("[1,10)", Int4, "5").unwrap());
        assert!(!contains_elem("[1,10)", Int4, "10").unwrap());
        assert!(contains_range("[1,10)", "[2,5)", Int4).unwrap());
        assert!(!contains_range("[1,10)", "[5,15)", Int4).unwrap());
        assert!(overlaps("[1,5)", "[4,10)", Int4).unwrap());
        assert!(!overlaps("[1,5)", "[6,10)", Int4).unwrap());
        // Every range contains the empty range; the empty range overlaps nothing.
        assert!(contains_range("[1,5)", "empty", Int4).unwrap());
        assert!(!overlaps("[1,5)", "empty", Int4).unwrap());
    }

    #[test]
    fn predicates() {
        assert!(strictly_before("[1,10)", "[20,30)", Int4).unwrap());
        assert!(!strictly_before("[1,10)", "[5,30)", Int4).unwrap());
        assert!(strictly_after("[20,30)", "[1,10)", Int4).unwrap());
        assert!(not_right_of("[1,10)", "[5,20)", Int4).unwrap());
        assert!(!not_right_of("[1,30)", "[5,20)", Int4).unwrap());
        assert!(not_left_of("[5,20)", "[1,10)", Int4).unwrap());
        assert!(adjacent("[1,10)", "[10,20)", Int4).unwrap());
        assert!(!adjacent("[1,10)", "[11,20)", Int4).unwrap());
        assert!(lower_inf("(,5)").unwrap());
        assert!(upper_inf("[1,)").unwrap());
        assert!(!lower_inf("[1,5)").unwrap());
    }

    #[test]
    fn set_operations() {
        let a = mini_arena();
        assert_eq!(intersect("[1,10)", "[5,15)", Int4, &a).unwrap(), "[5,10)");
        assert_eq!(intersect("[1,10)", "[20,30)", Int4, &a).unwrap(), "empty");
        assert_eq!(union("[1,10)", "[5,15)", Int4, &a).unwrap(), "[1,15)");
        assert_eq!(union("[1,5)", "[5,10)", Int4, &a).unwrap(), "[1,10)");
        assert!(union("[1,5)", "[10,20)", Int4, &a).is_err());
        assert_eq!(difference("[1,10)", "[5,15)", Int4, &a).unwrap(), "[1,5)");
        assert_eq!(difference("[1,10)", "[0,5)", Int4, &a).unwrap(), "[5,10)");
        assert_eq!(difference("[1,10)", "[20,30)", Int4, &a).unwrap(), "[1,10)");
        assert!(difference("[1,10)", "[3,6)", Int4, &a).is_err());
        assert_eq!(merge("[1,5)", "[10,20)", Int4, &a).unwrap(), "[1,20)");
    }

    fn mini_arena() -> Arena {
        let budget = Box::leak(Box::new(crate::mem::Budget::new(1 << 16)));
        Arena::new(budget, "range_test", 1 << 15).unwrap()
    }
}
