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

fn bad(kind: RangeKind, s: &str) -> SqlError {
    sql_err!("22P02", "malformed range literal for {}: \"{}\"", kind.name(), s)
}

/// Parses a range literal (`[1,5)` / `(,5]` / `empty`) into its components.
pub fn parse<'a>(input: &'a str, kind: RangeKind) -> Result<Parsed<'a>, SqlError> {
    let t = input.trim();
    if t.eq_ignore_ascii_case("empty") {
        return Ok(Parsed { empty: true, lower: None, upper: None, lower_inc: false, upper_inc: false });
    }
    let b = t.as_bytes();
    if b.len() < 2 {
        return Err(bad(kind, input));
    }
    let lower_inc = match b[0] {
        b'[' => true,
        b'(' => false,
        _ => return Err(bad(kind, input)),
    };
    let upper_inc = match b[b.len() - 1] {
        b']' => true,
        b')' => false,
        _ => return Err(bad(kind, input)),
    };
    let inner = &t[1..t.len() - 1];
    let (lower_text, upper_text) = inner.split_once(',').ok_or_else(|| bad(kind, input))?;
    let (lower_text, upper_text) = (lower_text.trim(), upper_text.trim());
    Ok(Parsed {
        empty: false,
        lower: if lower_text.is_empty() { None } else { Some(lower_text) },
        upper: if upper_text.is_empty() { None } else { Some(upper_text) },
        lower_inc,
        upper_inc,
    })
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
        let lb = if lower_text.is_some() { '[' } else { '(' };
        let text = stack_format!(128, "{}{},{})", lb, lower_text.unwrap_or(""), upper_text.unwrap_or(""));
        return alloc(arena, text.as_str());
    }
    // Continuous: empty when bounds are equal and not both inclusive.
    if let (Some(lower_text), Some(upper_text)) = (p.lower, p.upper)
        && cmp_elem(lower_text, upper_text, kind)? == Ordering::Equal
        && !(p.lower_inc && p.upper_inc)
    {
        return alloc(arena, "empty");
    }
    // An unbounded bound is always exclusive-bracketed.
    let lb = if p.lower.is_some() && p.lower_inc { '[' } else { '(' };
    let rb = if p.upper.is_some() && p.upper_inc { ']' } else { ')' };
    let text = stack_format!(128, "{}{},{}{}", lb, p.lower.unwrap_or(""), p.upper.unwrap_or(""), rb);
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
            let n: i64 = v.trim().parse().map_err(|_| bad(kind, v))?;
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
            let x: i64 = a.trim().parse().map_err(|_| bad(kind, a))?;
            let y: i64 = b.trim().parse().map_err(|_| bad(kind, b))?;
            x.cmp(&y)
        }
        RangeKind::Date => super::datetime::parse_date(a.trim())?
            .cmp(&super::datetime::parse_date(b.trim())?),
        RangeKind::Ts => super::datetime::parse_timestamp(a.trim(), false)?
            .cmp(&super::datetime::parse_timestamp(b.trim(), false)?),
        RangeKind::Tstz => super::datetime::parse_timestamp(a.trim(), true)?
            .cmp(&super::datetime::parse_timestamp(b.trim(), true)?),
        RangeKind::Num => match numeric::cmp_decimal_str(a.trim(), b.trim()) {
            Some(o) => o,
            None => return Err(bad(kind, a)),
        },
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
    let p = parse(text, kind)?;
    let raw = if lower { p.lower } else { p.upper };
    match raw {
        None => Ok(Datum::Null),
        Some(s) => elem_datum(s, kind, arena),
    }
}

fn elem_datum<'a>(s: &str, kind: RangeKind, arena: &'a Arena) -> Result<Datum<'a>, SqlError> {
    Ok(match kind {
        RangeKind::Int4 => Datum::Int4(s.trim().parse().map_err(|_| bad(kind, s))?),
        RangeKind::Int8 => Datum::Int8(s.trim().parse().map_err(|_| bad(kind, s))?),
        RangeKind::Date => Datum::Date(super::datetime::parse_date(s.trim())?),
        RangeKind::Ts => Datum::Timestamp(super::datetime::parse_timestamp(s.trim(), false)?),
        RangeKind::Tstz => Datum::Timestamptz(super::datetime::parse_timestamp(s.trim(), true)?),
        RangeKind::Num => Datum::Numeric(Numeric::parse(s.trim(), arena)?),
    })
}

pub fn is_empty(text: &str) -> bool {
    text.trim().eq_ignore_ascii_case("empty")
}

pub fn bound_inc(text: &str, kind: RangeKind, lower: bool) -> Result<bool, SqlError> {
    let p = parse(text, kind)?;
    if p.empty {
        return Ok(false);
    }
    Ok(if lower { p.lower.is_some() && p.lower_inc } else { p.upper.is_some() && p.upper_inc })
}

/// `range @> element`.
pub fn contains_elem(text: &str, kind: RangeKind, element: &str) -> Result<bool, SqlError> {
    let p = parse(text, kind)?;
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
    let (parsed_outer, parsed_inner) = (parse(outer, kind)?, parse(inner, kind)?);
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
    let (parsed_a, parsed_b) = (parse(a, kind)?, parse(b, kind)?);
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
    let (parsed_a, parsed_b) = (parse(a, kind)?, parse(b, kind)?);
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
pub fn lower_inf(text: &str, kind: RangeKind) -> Result<bool, SqlError> {
    let p = parse(text, kind)?;
    Ok(!p.empty && p.lower.is_none())
}

/// `upper_inf(r)`: the range is non-empty and has no upper bound.
pub fn upper_inf(text: &str, kind: RangeKind) -> Result<bool, SqlError> {
    let p = parse(text, kind)?;
    Ok(!p.empty && p.upper.is_none())
}

/// `a << b`: `a` lies strictly to the left of `b` (no overlap, `a` entirely
/// below `b`). Empty ranges are never strictly left of anything.
pub fn strictly_before(a: &str, b: &str, kind: RangeKind) -> Result<bool, SqlError> {
    let (parsed_a, parsed_b) = (parse(a, kind)?, parse(b, kind)?);
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
    let (parsed_a, parsed_b) = (parse(a, kind)?, parse(b, kind)?);
    if parsed_a.empty || parsed_b.empty {
        return Ok(false);
    }
    Ok(cmp_bound(parsed_a.upper, parsed_a.upper_inc, parsed_b.upper, parsed_b.upper_inc, false, kind)? != Ordering::Greater)
}

/// `a &> b`: `a` does not extend to the left of `b` (`lower(a) >= lower(b)`).
pub fn not_left_of(a: &str, b: &str, kind: RangeKind) -> Result<bool, SqlError> {
    let (parsed_a, parsed_b) = (parse(a, kind)?, parse(b, kind)?);
    if parsed_a.empty || parsed_b.empty {
        return Ok(false);
    }
    Ok(cmp_bound(parsed_a.lower, parsed_a.lower_inc, parsed_b.lower, parsed_b.lower_inc, true, kind)? != Ordering::Less)
}

/// `a -|- b`: the ranges are adjacent (disjoint with no gap between them).
pub fn adjacent(a: &str, b: &str, kind: RangeKind) -> Result<bool, SqlError> {
    let (parsed_a, parsed_b) = (parse(a, kind)?, parse(b, kind)?);
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
    let (parsed_a, parsed_b) = (parse(a, kind)?, parse(b, kind)?);
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
    let (parsed_a, parsed_b) = (parse(a, kind)?, parse(b, kind)?);
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
    let (parsed_a, parsed_b) = (parse(a, kind)?, parse(b, kind)?);
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
    let (parsed_a, parsed_b) = (parse(a, kind)?, parse(b, kind)?);
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

#[cfg(test)]
mod tests {
    use super::*;
    use RangeKind::{Int4, Num};

    #[test]
    fn cmp_ranges_orders_by_bounds() {
        // Lower bound decides first, then upper bound.
        assert_eq!(cmp_ranges("[1,5)", "[1,6)", Int4).unwrap(), Ordering::Less);
        assert_eq!(cmp_ranges("[1,5)", "[2,3)", Int4).unwrap(), Ordering::Less);
        assert_eq!(cmp_ranges("[1,10)", "[1,5)", Int4).unwrap(), Ordering::Greater);
        assert_eq!(cmp_ranges("[1,5)", "[1,5)", Int4).unwrap(), Ordering::Equal);
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
        assert!(lower_inf("(,5)", Int4).unwrap());
        assert!(upper_inf("[1,)", Int4).unwrap());
        assert!(!lower_inf("[1,5)", Int4).unwrap());
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
