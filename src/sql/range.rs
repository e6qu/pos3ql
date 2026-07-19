//! Range types (`int4range`/`int8range`/`numrange`/`daterange`/`tsrange`/
//! `tstzrange`). Values are carried as canonical text (see `Datum::Range`);
//! this module parses a literal or constructor into that canonical form and
//! answers the range operators/functions. Discrete kinds (int/date) are
//! canonicalized to the half-open `[lower, upper)` form, as PostgreSQL does.
//!
//! Allocation-free: intermediate bound text lives in fixed stack buffers, and
//! numeric comparisons borrow the caller's arena (no post-startup heap).

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

/// Parses a range literal `[lo,hi)` / `(,hi]` / `empty` into its components.
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
    let (lo, hi) = inner.split_once(',').ok_or_else(|| bad(kind, input))?;
    let (lo, hi) = (lo.trim(), hi.trim());
    Ok(Parsed {
        empty: false,
        lower: if lo.is_empty() { None } else { Some(lo) },
        upper: if hi.is_empty() { None } else { Some(hi) },
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
    if let (Some(lo), Some(hi)) = (p.lower, p.upper)
        && cmp_elem(lo, hi, kind, arena)? == Ordering::Greater
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
        let lo: Option<&str> = match (p.lower, p.lower_inc) {
            (None, _) => None,
            (Some(v), true) => Some(v),
            (Some(v), false) => {
                incr_into(v, kind, &mut lo_buf)?;
                Some(lo_buf.as_str())
            }
        };
        let hi: Option<&str> = match (p.upper, p.upper_inc) {
            (None, _) => None,
            (Some(v), false) => Some(v),
            (Some(v), true) => {
                incr_into(v, kind, &mut hi_buf)?;
                Some(hi_buf.as_str())
            }
        };
        if let (Some(l), Some(h)) = (lo, hi)
            && cmp_elem(l, h, kind, arena)? != Ordering::Less
        {
            return alloc(arena, "empty");
        }
        // An unbounded lower bound uses `(`; a bounded one is inclusive `[`.
        let lb = if lo.is_some() { '[' } else { '(' };
        let text = stack_format!(128, "{}{},{})", lb, lo.unwrap_or(""), hi.unwrap_or(""));
        return alloc(arena, text.as_str());
    }
    // Continuous: empty when bounds are equal and not both inclusive.
    if let (Some(lo), Some(hi)) = (p.lower, p.upper)
        && cmp_elem(lo, hi, kind, arena)? == Ordering::Equal
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

/// Builds a range from a constructor `int4range(lo, hi [, flags])`.
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
    let lo = elem_text(lower, kind, arena)?;
    let hi = elem_text(upper, kind, arena)?;
    let p = Parsed { empty: false, lower: lo, upper: hi, lower_inc, upper_inc };
    canonical(&p, kind, arena)
}

fn elem_text<'a>(d: Datum, kind: RangeKind, arena: &'a Arena) -> Result<Option<&'a str>, SqlError> {
    if d.is_null() {
        return Ok(None);
    }
    let s = stack_format!(64, "{}", d);
    let owned = alloc(arena, s.as_str())?;
    // Validate it parses for this kind.
    cmp_elem(owned, owned, kind, arena)?;
    Ok(Some(owned))
}

fn alloc<'a>(arena: &'a Arena, s: &str) -> Result<&'a str, SqlError> {
    arena.alloc_str(s).map_err(|_| sql_err!("53200", "out of memory"))
}

/// Increments a discrete bound value by one (int/date) into `buf`.
fn incr_into(v: &str, kind: RangeKind, buf: &mut StackStr<48>) -> Result<(), SqlError> {
    buf.clear();
    match kind {
        RangeKind::Int4 | RangeKind::Int8 => {
            let n: i64 = v.trim().parse().map_err(|_| bad(kind, v))?;
            let _ = write!(buf, "{}", n + 1);
        }
        RangeKind::Date => {
            let d = super::datetime::parse_date(v.trim())?;
            let _ = write!(buf, "{}", super::datetime::format_date(d + 1).as_str());
        }
        _ => {
            let _ = write!(buf, "{}", v);
        }
    }
    Ok(())
}

/// Compares two bound element texts under `kind`'s subtype ordering.
fn cmp_elem(a: &str, b: &str, kind: RangeKind, arena: &Arena) -> Result<Ordering, SqlError> {
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
        RangeKind::Num => {
            let x = Numeric::parse(a.trim(), arena)?;
            let y = Numeric::parse(b.trim(), arena)?;
            numeric::compare(&x, &y)
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
pub fn contains_elem(
    text: &str,
    kind: RangeKind,
    elem: &str,
    arena: &Arena,
) -> Result<bool, SqlError> {
    let p = parse(text, kind)?;
    if p.empty {
        return Ok(false);
    }
    if let Some(lo) = p.lower {
        match cmp_elem(elem, lo, kind, arena)? {
            Ordering::Less => return Ok(false),
            Ordering::Equal if !p.lower_inc => return Ok(false),
            _ => {}
        }
    }
    if let Some(hi) = p.upper {
        match cmp_elem(elem, hi, kind, arena)? {
            Ordering::Greater => return Ok(false),
            Ordering::Equal if !p.upper_inc => return Ok(false),
            _ => {}
        }
    }
    Ok(true)
}

/// `outer @> inner` (range contains range).
pub fn contains_range(
    outer: &str,
    inner: &str,
    kind: RangeKind,
    arena: &Arena,
) -> Result<bool, SqlError> {
    let (po, pi) = (parse(outer, kind)?, parse(inner, kind)?);
    if pi.empty {
        return Ok(true);
    }
    if po.empty {
        return Ok(false);
    }
    Ok(lower_le(&po, &pi, kind, arena)? && upper_ge(&po, &pi, kind, arena)?)
}

/// `a && b` (ranges overlap).
pub fn overlaps(a: &str, b: &str, kind: RangeKind, arena: &Arena) -> Result<bool, SqlError> {
    let (pa, pb) = (parse(a, kind)?, parse(b, kind)?);
    if pa.empty || pb.empty {
        return Ok(false);
    }
    Ok(!(strictly_left(&pa, &pb, kind, arena)? || strictly_left(&pb, &pa, kind, arena)?))
}

fn strictly_left(a: &Parsed, b: &Parsed, kind: RangeKind, arena: &Arena) -> Result<bool, SqlError> {
    let (Some(au), Some(bl)) = (a.upper, b.lower) else {
        return Ok(false);
    };
    Ok(match cmp_elem(au, bl, kind, arena)? {
        Ordering::Less => true,
        Ordering::Equal => !(a.upper_inc && b.lower_inc),
        Ordering::Greater => false,
    })
}

fn lower_le(outer: &Parsed, inner: &Parsed, kind: RangeKind, arena: &Arena) -> Result<bool, SqlError> {
    let Some(ol) = outer.lower else { return Ok(true) };
    let Some(il) = inner.lower else { return Ok(false) };
    Ok(match cmp_elem(ol, il, kind, arena)? {
        Ordering::Less => true,
        Ordering::Equal => outer.lower_inc || !inner.lower_inc,
        Ordering::Greater => false,
    })
}

fn upper_ge(outer: &Parsed, inner: &Parsed, kind: RangeKind, arena: &Arena) -> Result<bool, SqlError> {
    let Some(ou) = outer.upper else { return Ok(true) };
    let Some(iu) = inner.upper else { return Ok(false) };
    Ok(match cmp_elem(ou, iu, kind, arena)? {
        Ordering::Greater => true,
        Ordering::Equal => outer.upper_inc || !inner.upper_inc,
        Ordering::Less => false,
    })
}
