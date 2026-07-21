//! Binary-operator evaluation split out of the expression evaluator: the array
//! set operators (`@> <@ &&`) and the range predicate and set operators. These
//! take already-evaluated `Datum`s (they do not walk the AST), so they depend
//! only on the value modules, not on `eval_full`.

use crate::mem::arena::Arena;
use crate::sql::ast::BinaryOp;
use crate::sql::numeric::{self, Numeric};
use crate::sql::types::{Datum, Interval, RangeKind};
use crate::sql::{array, datetime, range};
use crate::sql_err;

use super::{
    arena_full, as_f64, as_i64, bad_text, division_by_zero, interval_cmp_value, num_factor,
    overflow, parse_bool, parse_uuid, sqlstate, to_numeric, type_mismatch, type_name_of,
    validate_bits, SqlError,
};

pub(crate) fn array_set_op<'a>(
    operator: BinaryOp,
    l: Datum<'a>,
    r: Datum<'a>,
) -> Result<Datum<'a>, SqlError> {
    use BinaryOp::{ContainedBy, Contains, Overlaps};
    if l.is_null() || r.is_null() {
        return Ok(Datum::Null);
    }
    let (Datum::Array { element: le, raw: lr }, Datum::Array { element: re, raw: rr }) = (l, r)
    else {
        return Err(sql_err!(
            sqlstate::UNDEFINED_FUNCTION,
            "operator requires two arrays"
        ));
    };
    let member = |needle: &Datum<'a>, elem, raw: &'a [u8]| -> Result<bool, SqlError> {
        if needle.is_null() {
            return Ok(false);
        }
        for i in 0..array::len(raw) {
            let v = array::get(raw, elem, i).unwrap_or(Datum::Null);
            if !v.is_null() && compare_datums(needle, &v)?.is_eq() {
                return Ok(true);
            }
        }
        Ok(false)
    };
    // Every element of `sub` is a member of `sup`.
    let subset = |sub_elem, sub_raw: &'a [u8], sup_elem, sup_raw: &'a [u8]| -> Result<bool, SqlError> {
        for i in 0..array::len(sub_raw) {
            let v = array::get(sub_raw, sub_elem, i).unwrap_or(Datum::Null);
            if !v.is_null() && !member(&v, sup_elem, sup_raw)? {
                return Ok(false);
            }
        }
        Ok(true)
    };
    let result = match operator {
        Contains => subset(re, rr, le, lr)?,
        ContainedBy => subset(le, lr, re, rr)?,
        Overlaps => {
            let mut any = false;
            for i in 0..array::len(lr) {
                let v = array::get(lr, le, i).unwrap_or(Datum::Null);
                if member(&v, re, rr)? {
                    any = true;
                    break;
                }
            }
            any
        }
        _ => unreachable!("array_set_op only handles @>, <@, &&"),
    };
    Ok(Datum::Bool(result))
}

pub(crate) fn range_op<'a>(
    operator: BinaryOp,
    l: Datum<'a>,
    r: Datum<'a>,
    arena: &'a Arena,
) -> Result<Datum<'a>, SqlError> {
    use BinaryOp::{Adjacent, ContainedBy, Contains, NotLeftOf, NotRightOf, Overlaps, Shl, Shr};
    if l.is_null() || r.is_null() {
        return Ok(Datum::Null);
    }
    match operator {
        Contains => range_contains(l, r, arena),
        ContainedBy => range_contains(r, l, arena),
        _ => {
            let (lt, lk) = as_range(&l)?;
            let (rt, rk) = as_range(&r)?;
            if lk != rk {
                return Err(range_mismatch());
            }
            Ok(Datum::Bool(match operator {
                Overlaps => range::overlaps(lt, rt, lk)?,
                Shl => range::strictly_before(lt, rt, lk)?,
                Shr => range::strictly_after(lt, rt, lk)?,
                NotRightOf => range::not_right_of(lt, rt, lk)?,
                NotLeftOf => range::not_left_of(lt, rt, lk)?,
                Adjacent => range::adjacent(lt, rt, lk)?,
                _ => unreachable!("range_op only handles range predicates"),
            }))
        }
    }
}

/// Range set operators returning a range: `+` (union), `-` (difference), `*`
/// (intersection). Both operands must be ranges of the same kind.
pub(crate) fn range_setop<'a>(
    operator: BinaryOp,
    l: Datum<'a>,
    r: Datum<'a>,
    arena: &'a Arena,
) -> Result<Datum<'a>, SqlError> {
    if l.is_null() || r.is_null() {
        return Ok(Datum::Null);
    }
    let (lt, lk) = as_range(&l)?;
    let (rt, rk) = as_range(&r)?;
    if lk != rk {
        return Err(range_mismatch());
    }
    let text = match operator {
        BinaryOp::Add => range::union(lt, rt, lk, arena)?,
        BinaryOp::Sub => range::difference(lt, rt, lk, arena)?,
        BinaryOp::Mul => range::intersect(lt, rt, lk, arena)?,
        _ => return Err(type_mismatch("range operator", &l)),
    };
    Ok(Datum::Range { text, kind: lk })
}

pub(crate) fn range_mismatch() -> SqlError {
    sql_err!(sqlstate::UNDEFINED_FUNCTION, "operator requires matching range types")
}

/// Whether `container` (a range) contains `contained` (a range of the same kind
/// or a bare element).
fn range_contains<'a>(
    container: Datum<'a>,
    contained: Datum<'a>,
    arena: &'a Arena,
) -> Result<Datum<'a>, SqlError> {
    let (container_text, container_kind) = as_range(&container)?;
    match contained {
        Datum::Range { text, kind } => {
            if kind != container_kind {
                return Err(range_mismatch());
            }
            Ok(Datum::Bool(range::contains_range(container_text, text, container_kind)?))
        }
        element => {
            let element_text = arena.alloc_str_display(element).map_err(|_| arena_full())?;
            Ok(Datum::Bool(range::contains_elem(
                container_text,
                container_kind,
                element_text,
            )?))
        }
    }
}

fn as_range<'a>(d: &Datum<'a>) -> Result<(&'a str, RangeKind), SqlError> {
    match d {
        Datum::Range { text, kind } => Ok((text, *kind)),
        other => Err(type_mismatch("range operator", other)),
    }
}

/// Extracts the `'0'`/`'1'` characters of a bit-string operand, coercing an
/// unknown text literal (`'101'`) but rejecting any other type.
fn bit_operand<'a>(d: &Datum<'a>) -> Result<&'a str, SqlError> {
    match d {
        Datum::Bit { bits, .. } => Ok(bits),
        Datum::Text(s) => validate_bits(s),
        other => Err(sql_err!(
            sqlstate::UNDEFINED_FUNCTION,
            "operator does not exist: bit vs {}",
            type_name_of(other)
        )),
    }
}

/// True when a bit operand reports as `varbit`; a concatenation or bitwise
/// combination is `varbit` if either input is.
fn bit_is_varying(d: &Datum) -> bool {
    matches!(d, Datum::Bit { varying: true, .. })
}

/// `bit || bit`: concatenation, always `varbit`.
pub(crate) fn bit_concat<'a>(l: Datum<'a>, r: Datum<'a>, arena: &'a Arena) -> Result<Datum<'a>, SqlError> {
    if l.is_null() || r.is_null() {
        return Ok(Datum::Null);
    }
    let (a, b) = (bit_operand(&l)?, bit_operand(&r)?);
    let out = arena
        .alloc_slice_with(a.len() + b.len(), |i| {
            if i < a.len() { a.as_bytes()[i] } else { b.as_bytes()[i - a.len()] }
        })
        .map_err(|_| arena_full())?;
    Ok(Datum::Bit { bits: unsafe { core::str::from_utf8_unchecked(out) }, varying: true })
}

/// `bit & bit`, `bit | bit`, `bit # bit`: per-position boolean combination.
/// Both operands must have equal length (PostgreSQL rejects a size mismatch).
pub(crate) fn bit_bitwise<'a>(
    operator: BinaryOp,
    l: Datum<'a>,
    r: Datum<'a>,
    arena: &'a Arena,
) -> Result<Datum<'a>, SqlError> {
    if l.is_null() || r.is_null() {
        return Ok(Datum::Null);
    }
    let (a, b) = (bit_operand(&l)?, bit_operand(&r)?);
    if a.len() != b.len() {
        let verb = match operator {
            BinaryOp::BitAnd => "AND",
            BinaryOp::BitOr => "OR",
            _ => "XOR",
        };
        return Err(sql_err!("22026", "cannot {} bit strings of different sizes", verb));
    }
    let varying = bit_is_varying(&l) || bit_is_varying(&r);
    let out = arena
        .alloc_slice_with(a.len(), |i| {
            let (x, y) = (a.as_bytes()[i] == b'1', b.as_bytes()[i] == b'1');
            let bit = match operator {
                BinaryOp::BitAnd => x && y,
                BinaryOp::BitOr => x || y,
                _ => x ^ y,
            };
            if bit { b'1' } else { b'0' }
        })
        .map_err(|_| arena_full())?;
    Ok(Datum::Bit { bits: unsafe { core::str::from_utf8_unchecked(out) }, varying })
}

/// `bit << n` / `bit >> n`: length-preserving shift, zero-filled. A negative
/// count shifts the other way, matching PostgreSQL.
pub(crate) fn bit_shift<'a>(
    operator: BinaryOp,
    l: Datum<'a>,
    r: Datum<'a>,
    arena: &'a Arena,
) -> Result<Datum<'a>, SqlError> {
    if l.is_null() || r.is_null() {
        return Ok(Datum::Null);
    }
    let Datum::Bit { bits, varying } = l else {
        return Err(type_mismatch("bit-string shift", &l));
    };
    let count = as_i64(&r).ok_or_else(|| type_mismatch("bit-string shift amount", &r))?;
    // `<<` moves bits toward the most-significant (left) end.
    let left = if matches!(operator, BinaryOp::Shl) { count } else { -count };
    let len = bits.len() as i64;
    let src = bits.as_bytes();
    let out = arena
        .alloc_slice_with(bits.len(), |i| {
            let from = i as i64 + left;
            if (0..len).contains(&from) { src[from as usize] } else { b'0' }
        })
        .map_err(|_| arena_full())?;
    Ok(Datum::Bit { bits: unsafe { core::str::from_utf8_unchecked(out) }, varying })
}

/// Comparison of two bit strings: PostgreSQL compares bit-by-bit, and when one
/// is a prefix of the other the shorter sorts first — exactly the lexicographic
/// order of the `'0'`/`'1'` strings.
pub(crate) fn compare_bits<'a>(operator: BinaryOp, l: Datum<'a>, r: Datum<'a>) -> Result<Datum<'a>, SqlError> {
    if l.is_null() || r.is_null() {
        return Ok(Datum::Null);
    }
    let (a, b) = (bit_operand(&l)?, bit_operand(&r)?);
    let ordering = a.cmp(b);
    Ok(Datum::Bool(match operator {
        BinaryOp::Eq => ordering.is_eq(),
        BinaryOp::NotEq => ordering.is_ne(),
        BinaryOp::Lt => ordering.is_lt(),
        BinaryOp::LtEq => ordering.is_le(),
        BinaryOp::Gt => ordering.is_gt(),
        _ => ordering.is_ge(),
    }))
}

/// Total order used by comparisons and ORDER BY. NULL handling differs
/// between the two, so NULL never reaches here.
/// Exact comparison between a Numeric and an integer, allocation-free.
fn compare_numeric_int(l: &Datum, r: &Datum) -> Result<core::cmp::Ordering, SqlError> {
    let mut buffer = [0u8; 20];
    match (l, r) {
        (Datum::Numeric(n), other) => {
            let interval = as_i64(other).expect("integer side");
            let t = Numeric::from_i64_stack(interval, &mut buffer);
            Ok(numeric::compare(n, &t))
        }
        (other, Datum::Numeric(n)) => {
            let interval = as_i64(other).expect("integer side");
            let t = Numeric::from_i64_stack(interval, &mut buffer);
            Ok(numeric::compare(&t, n))
        }
        _ => unreachable!("compare_numeric_int only for numeric/int pairs"),
    }
}

pub fn compare_datums(l: &Datum, r: &Datum) -> Result<core::cmp::Ordering, SqlError> {
    use core::cmp::Ordering;
    let ord = match (l, r) {
        (Datum::Bool(a), Datum::Bool(b)) => a.cmp(b),
        (Datum::Text(a), Datum::Text(b)) => a.cmp(b),
        (Datum::Date(a), Datum::Date(b)) => a.cmp(b),
        (Datum::Timestamp(a), Datum::Timestamp(b))
        | (Datum::Timestamptz(a), Datum::Timestamptz(b))
        | (Datum::Timestamp(a), Datum::Timestamptz(b))
        | (Datum::Timestamptz(a), Datum::Timestamp(b)) => a.cmp(b),
        (Datum::Time(a), Datum::Time(b)) => a.cmp(b),
        (Datum::Json { text: a, .. }, Datum::Json { text: b, .. }) => a.cmp(b),
        (Datum::Record(a), Datum::Record(b)) => {
            // Field-wise, with a NULL field comparing greater (PostgreSQL
            // record ordering); shorter record sorts first on a common prefix.
            for i in 0..a.len().min(b.len()) {
                let (x, y) = (&a[i].value, &b[i].value);
                let c = match (x.is_null(), y.is_null()) {
                    (true, true) => Ordering::Equal,
                    (true, false) => Ordering::Greater,
                    (false, true) => Ordering::Less,
                    (false, false) => compare_datums(x, y)?,
                };
                if !c.is_eq() {
                    return Ok(c);
                }
            }
            a.len().cmp(&b.len())
        }
        (Datum::Array { element, raw: ra }, Datum::Array { raw: rb, .. }) => {
            // Element-wise, then by length (PostgreSQL array ordering).
            let (length_a, length_b) = (array::len(ra), array::len(rb));
            for i in 0..length_a.min(length_b) {
                let x = array::get(ra, *element, i).unwrap_or(Datum::Null);
                let y = array::get(rb, *element, i).unwrap_or(Datum::Null);
                let c = compare_datums(&x, &y)?;
                if !c.is_eq() {
                    return Ok(c);
                }
            }
            length_a.cmp(&length_b)
        }
        (Datum::Date(a), Datum::Timestamp(b) | Datum::Timestamptz(b)) => {
            (i64::from(*a) * 86_400_000_000).cmp(b)
        }
        (Datum::Timestamp(a) | Datum::Timestamptz(a), Datum::Date(b)) => {
            a.cmp(&(i64::from(*b) * 86_400_000_000))
        }
        (Datum::Uuid(a), Datum::Uuid(b)) => a.cmp(b),
        (Datum::Bytea(a), Datum::Bytea(b)) => a.cmp(b),
        // Intervals compare by canonical microsecond value (PostgreSQL's
        // interval_cmp_value: months count as 30 days, days as 24 hours), so
        // `1 month` = `30 days` = `720 hours`.
        (Datum::Interval(a), Datum::Interval(b)) => {
            interval_cmp_value(*a).cmp(&interval_cmp_value(*b))
        }
        // Bit strings compare bit-by-bit, shorter-is-prefix sorting first —
        // exactly lexicographic order of the '0'/'1' characters.
        (Datum::Bit { bits: a, .. }, Datum::Bit { bits: b, .. }) => a.cmp(b),
        (Datum::Numeric(a), Datum::Numeric(b)) => numeric::compare(a, b),
        (Datum::Range { text: a, kind: ka }, Datum::Range { text: b, kind: kb }) => {
            if ka != kb {
                return Err(sql_err!(
                    sqlstate::UNDEFINED_FUNCTION,
                    "operator does not exist: {} = {}",
                    ka.name(),
                    kb.name()
                ));
            }
            range::cmp_ranges(a, b, *ka)?
        }
        (Datum::Multirange { text: a, kind: ka }, Datum::Multirange { text: b, kind: kb }) => {
            if ka != kb {
                return Err(sql_err!(
                    sqlstate::UNDEFINED_FUNCTION,
                    "operator does not exist: {} = {}",
                    ka.multirange_name(),
                    kb.multirange_name()
                ));
            }
            range::cmp_multiranges(a, b, *ka)?
        }
        // Numeric vs integer: compare exactly via numeric.
        (Datum::Numeric(_), Datum::Int4(_) | Datum::Int8(_))
        | (Datum::Int4(_) | Datum::Int8(_), Datum::Numeric(_)) => {
            // Fall through to the float comparison below only if exactness is
            // not required; integers convert to numeric exactly.
            return compare_numeric_int(l, r);
        }
        _ => {
            if let (Some(a), Some(b)) = (as_i64(l), as_i64(r)) {
                a.cmp(&b)
            } else if let (Some(a), Some(b)) = (as_f64(l), as_f64(r)) {
                // PostgreSQL float comparison treats NaN as largest.
                return Ok(a.partial_cmp(&b).unwrap_or_else(|| {
                    match (a.is_nan(), b.is_nan()) {
                        (true, false) => Ordering::Greater,
                        (false, true) => Ordering::Less,
                        _ => Ordering::Equal,
                    }
                }));
            } else {
                // PostgreSQL reports incompatible comparisons as
                // "operator does not exist" (42883), not a datatype mismatch.
                return Err(sql_err!(
                    sqlstate::UNDEFINED_FUNCTION,
                    "operator does not exist: {} = {}",
                    type_name_of(l),
                    type_name_of(r)
                ));
            }
        }
    };
    Ok(ord)
}

/// PostgreSQL's unknown-literal rule, approximated: a text value meeting a
/// typed value in a comparison or arithmetic context converts to the typed
/// side (text parameters and quoted literals are "unknown", not text).
pub(crate) fn coerce_unknown<'a>(v: Datum<'a>, other: &Datum) -> Result<Datum<'a>, SqlError> {
    let Datum::Text(s) = v else {
        return Ok(v);
    };
    Ok(match other {
        Datum::Int4(_) => Datum::Int4(
            s.trim()
                .parse()
                .map_err(|_| bad_text(s, "integer"))?,
        ),
        Datum::Int8(_) => Datum::Int8(
            s.trim()
                .parse()
                .map_err(|_| bad_text(s, "bigint"))?,
        ),
        Datum::Float8(_) => Datum::Float8(
            s.trim()
                .parse()
                .map_err(|_| bad_text(s, "double precision"))?,
        ),
        Datum::Bool(_) => Datum::Bool(parse_bool(s)?),
        Datum::Date(_) => Datum::Date(datetime::parse_date(s)?),
        Datum::Timestamp(_) => Datum::Timestamp(datetime::parse_timestamp(s, false)?),
        Datum::Timestamptz(_) => {
            Datum::Timestamptz(datetime::parse_timestamp(s, true)?)
        }
        Datum::Uuid(_) => Datum::Uuid(parse_uuid(s)?),
        _ => v,
    })
}

/// Range comparison operators (`=`, `<>`, `<`, `<=`, `>`, `>=`). Both operands
/// must be ranges of the same kind; ordering follows PostgreSQL `range_cmp`.
pub(crate) fn compare_ranges<'a>(operator: BinaryOp, l: Datum<'a>, r: Datum<'a>) -> Result<Datum<'a>, SqlError> {
    if l.is_null() || r.is_null() {
        return Ok(Datum::Null);
    }
    let sym = match operator {
        BinaryOp::Eq => "=",
        BinaryOp::NotEq => "<>",
        BinaryOp::Lt => "<",
        BinaryOp::LtEq => "<=",
        BinaryOp::Gt => ">",
        BinaryOp::GtEq => ">=",
        _ => unreachable!(),
    };
    let (Datum::Range { text: lt, kind: lk }, Datum::Range { text: rt, kind: rk }) = (l, r) else {
        return Err(sql_err!(
            sqlstate::UNDEFINED_FUNCTION,
            "operator does not exist: {} {} {}",
            type_name_of(&l),
            sym,
            type_name_of(&r)
        ));
    };
    if lk != rk {
        return Err(sql_err!(
            sqlstate::UNDEFINED_FUNCTION,
            "operator does not exist: {} {} {}",
            lk.name(),
            sym,
            rk.name()
        ));
    }
    let ord = range::cmp_ranges(lt, rt, lk)?;
    let out = match operator {
        BinaryOp::Eq => ord.is_eq(),
        BinaryOp::NotEq => ord.is_ne(),
        BinaryOp::Lt => ord.is_lt(),
        BinaryOp::LtEq => ord.is_le(),
        BinaryOp::Gt => ord.is_gt(),
        BinaryOp::GtEq => ord.is_ge(),
        _ => unreachable!(),
    };
    Ok(Datum::Bool(out))
}

pub(crate) fn compare<'a>(
    operator: BinaryOp,
    l: Datum<'a>,
    r: Datum<'a>,
    l_unknown: bool,
    r_unknown: bool,
) -> Result<Datum<'a>, SqlError> {
    if l.is_null() || r.is_null() {
        return Ok(Datum::Null);
    }
    let l = if l_unknown { coerce_unknown(l, &r)? } else { l };
    let r = if r_unknown { coerce_unknown(r, &l)? } else { r };
    let ord = compare_datums(&l, &r)?;
    let out = match operator {
        BinaryOp::Eq => ord.is_eq(),
        BinaryOp::NotEq => ord.is_ne(),
        BinaryOp::Lt => ord.is_lt(),
        BinaryOp::LtEq => ord.is_le(),
        BinaryOp::Gt => ord.is_gt(),
        BinaryOp::GtEq => ord.is_ge(),
        _ => unreachable!(),
    };
    Ok(Datum::Bool(out))
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn arithmetic<'a>(
    operator: BinaryOp,
    l: Datum<'a>,
    r: Datum<'a>,
    l_unknown: bool,
    r_unknown: bool,
    arena: &'a Arena,
) -> Result<Datum<'a>, SqlError> {
    if l.is_null() || r.is_null() {
        return Ok(Datum::Null);
    }
    let l = if l_unknown { coerce_unknown(l, &r)? } else { l };
    let r = if r_unknown { coerce_unknown(r, &l)? } else { r };
    // Date arithmetic (PostgreSQL): `date + int` / `date - int` -> date;
    // `date - date` -> int (days). Handled before the generic integer path,
    // which would otherwise coerce a date to a bare day count.
    // Interval arithmetic: date/timestamp ± interval -> timestamp; interval
    // ± interval -> interval. Months add calendar months (day clamped).
    match (operator, l, r) {
        (BinaryOp::Add | BinaryOp::Sub, Datum::Interval(a), Datum::Interval(b)) => {
            let s: i32 = if operator == BinaryOp::Sub { -1 } else { 1 };
            return Ok(Datum::Interval(Interval {
                months: a.months + s * b.months,
                days: a.days + s * b.days,
                micros: a.micros + s as i64 * b.micros,
            }));
        }
        // `interval * number` / `number * interval` / `interval / number`.
        (BinaryOp::Mul, Datum::Interval(interval), _) if num_factor(&r).is_some() => {
            return Ok(Datum::Interval(datetime::interval_scale(interval, num_factor(&r).expect("checked"), false)));
        }
        (BinaryOp::Mul, _, Datum::Interval(interval)) if num_factor(&l).is_some() => {
            return Ok(Datum::Interval(datetime::interval_scale(interval, num_factor(&l).expect("checked"), false)));
        }
        (BinaryOp::Div, Datum::Interval(interval), _) if num_factor(&r).is_some() => {
            return Ok(Datum::Interval(datetime::interval_scale(interval, num_factor(&r).expect("checked"), true)));
        }
        (BinaryOp::Add | BinaryOp::Sub, dt @ (Datum::Timestamp(_) | Datum::Timestamptz(_) | Datum::Date(_)), Datum::Interval(interval))
        | (BinaryOp::Add, Datum::Interval(interval), dt @ (Datum::Timestamp(_) | Datum::Timestamptz(_) | Datum::Date(_))) => {
            let base = match dt {
                Datum::Timestamp(t) | Datum::Timestamptz(t) => t,
                Datum::Date(d) => d as i64 * 86_400_000_000,
                _ => unreachable!(),
            };
            let signed = if operator == BinaryOp::Sub {
                Interval { months: -interval.months, days: -interval.days, micros: -interval.micros }
            } else {
                interval
            };
            let out = datetime::add_interval(base, signed);
            // date ± interval yields timestamp in PostgreSQL; timestamptz stays timezone.
            return Ok(match dt {
                Datum::Timestamptz(_) => Datum::Timestamptz(out),
                _ => Datum::Timestamp(out),
            });
        }
        _ => {}
    }
    match (operator, l, r) {
        (BinaryOp::Sub, Datum::Date(a), Datum::Date(b)) => {
            return Ok(Datum::Int4(a - b));
        }
        // timestamp - timestamp -> interval (days + time, no month folding).
        (BinaryOp::Sub, Datum::Timestamp(a), Datum::Timestamp(b))
        | (BinaryOp::Sub, Datum::Timestamptz(a), Datum::Timestamptz(b)) => {
            let diff = a - b;
            return Ok(Datum::Interval(Interval {
                months: 0,
                days: (diff / 86_400_000_000) as i32,
                micros: diff % 86_400_000_000,
            }));
        }
        (BinaryOp::Add | BinaryOp::Sub, Datum::Date(a), _) if as_i64(&r).is_some() => {
            let days = as_i64(&r).expect("checked");
            return date_shift(a, days, operator == BinaryOp::Sub);
        }
        // `int + date` is commutative with `date + int`; `int - date` is not
        // defined in PostgreSQL, so only Add is accepted here.
        (BinaryOp::Add, _, Datum::Date(b)) if as_i64(&l).is_some() => {
            let days = as_i64(&l).expect("checked");
            return date_shift(b, days, false);
        }
        _ => {}
    }
    // PostgreSQL numeric-promotion: int operator int -> int; if either side is
    // numeric (and neither is float8) -> numeric; if either is float8 ->
    // float8.
    let either_numeric = matches!(l, Datum::Numeric(_)) || matches!(r, Datum::Numeric(_));
    let either_float = matches!(l, Datum::Float8(_)) || matches!(r, Datum::Float8(_));
    // Integer operator integer stays integral.
    if let (Some(a), Some(b)) = (as_i64(&l), as_i64(&r)) {
        let out = match operator {
            BinaryOp::Add => a.checked_add(b),
            BinaryOp::Sub => a.checked_sub(b),
            BinaryOp::Mul => a.checked_mul(b),
            BinaryOp::Div => {
                if b == 0 {
                    return Err(division_by_zero());
                }
                a.checked_div(b)
            }
            BinaryOp::Mod => {
                if b == 0 {
                    return Err(division_by_zero());
                }
                a.checked_rem(b)
            }
            _ => unreachable!(),
        };
        let v = out.ok_or_else(|| overflow("bigint"))?;
        return narrow_int(v, &l, &r);
    }
    if either_numeric && !either_float {
        let a = to_numeric(&l, arena)?;
        let b = to_numeric(&r, arena)?;
        let out = match operator {
            BinaryOp::Add => numeric::add(&a, &b, arena)?,
            BinaryOp::Sub => numeric::sub(&a, &b, arena)?,
            BinaryOp::Mul => numeric::mul(&a, &b, arena)?,
            BinaryOp::Div => numeric::div(&a, &b, arena)?,
            BinaryOp::Mod => numeric::rem(&a, &b, arena)?,
            _ => unreachable!(),
        };
        return Ok(Datum::Numeric(out));
    }
    // PostgreSQL defines no modulo operator for double precision, so `%` with
    // a float8 operand is undefined even though `+`/`-`/`*`/`/` are not.
    if operator == BinaryOp::Mod && either_float {
        return Err(sql_err!(
            sqlstate::UNDEFINED_FUNCTION,
            "operator does not exist: {} % {}",
            type_name_of(&l),
            type_name_of(&r)
        ));
    }
    if let (Some(a), Some(b)) = (as_f64(&l), as_f64(&r)) {
        let out = match operator {
            BinaryOp::Add => a + b,
            BinaryOp::Sub => a - b,
            BinaryOp::Mul => a * b,
            BinaryOp::Div => {
                if b == 0.0 {
                    return Err(division_by_zero());
                }
                a / b
            }
            BinaryOp::Mod => {
                if b == 0.0 {
                    return Err(division_by_zero());
                }
                a % b
            }
            _ => unreachable!(),
        };
        return Ok(Datum::Float8(out));
    }
    // No arithmetic operator is defined for this operand pair (e.g. int - date,
    // text + int). PostgreSQL reports this as "operator does not exist" (42883).
    let sym = match operator {
        BinaryOp::Add => "+",
        BinaryOp::Sub => "-",
        BinaryOp::Mul => "*",
        BinaryOp::Div => "/",
        BinaryOp::Mod => "%",
        _ => "?",
    };
    Err(sql_err!(
        sqlstate::UNDEFINED_FUNCTION,
        "operator does not exist: {} {} {}",
        type_name_of(&l),
        sym,
        type_name_of(&r)
    ))
}

/// Shift a date (days since the PostgreSQL epoch) by `days`, subtracting when
/// `sub` is set. Out-of-range results error like PostgreSQL (22008).
fn date_shift<'a>(date: i32, days: i64, sub: bool) -> Result<Datum<'a>, SqlError> {
    let delta = if sub { -days } else { days };
    let shifted = i64::from(date)
        .checked_add(delta)
        .and_then(|v| i32::try_from(v).ok());
    match shifted {
        Some(d) => Ok(Datum::Date(d)),
        None => Err(sql_err!("22008", "date out of range")),
    }
}

/// int4 operator int4 yields int4 (with range check), as in PostgreSQL.
fn narrow_int<'a>(v: i64, l: &Datum, r: &Datum) -> Result<Datum<'a>, SqlError> {
    let both_int4 = matches!(l, Datum::Int4(_)) && matches!(r, Datum::Int4(_));
    if both_int4 {
        return match i32::try_from(v) {
            Ok(small) => Ok(Datum::Int4(small)),
            Err(_) => Err(overflow("integer")),
        };
    }
    Ok(Datum::Int8(v))
}

fn as_multirange<'a>(d: &Datum<'a>) -> Result<(&'a str, RangeKind), SqlError> {
    match d {
        Datum::Multirange { text, kind } => Ok((text, *kind)),
        other => Err(type_mismatch("multirange operator", other)),
    }
}

/// Views a range or multirange as multirange text (a range is wrapped as a
/// one-element multirange), for operators that accept either.
fn as_multirange_coerce<'a>(
    d: &Datum<'a>,
    arena: &'a Arena,
) -> Result<(&'a str, RangeKind), SqlError> {
    match d {
        Datum::Multirange { text, kind } => Ok((text, *kind)),
        Datum::Range { text, kind } => {
            Ok((range::multirange_from_range(text, *kind, arena)?, *kind))
        }
        other => Err(type_mismatch("multirange operator", other)),
    }
}

/// Multirange set operators returning a multirange: `+` (union), `-`
/// (difference), `*` (intersection). Both operands must be multiranges of the
/// same subtype.
pub(crate) fn multirange_setop<'a>(
    operator: BinaryOp,
    l: Datum<'a>,
    r: Datum<'a>,
    arena: &'a Arena,
) -> Result<Datum<'a>, SqlError> {
    if l.is_null() || r.is_null() {
        return Ok(Datum::Null);
    }
    let (lt, lk) = as_multirange(&l)?;
    let (rt, rk) = as_multirange(&r)?;
    if lk != rk {
        return Err(range_mismatch());
    }
    let text = match operator {
        BinaryOp::Add => range::multirange_union(lt, rt, lk, arena)?,
        BinaryOp::Sub => range::multirange_difference(lt, rt, lk, arena)?,
        BinaryOp::Mul => range::multirange_intersect(lt, rt, lk, arena)?,
        _ => return Err(type_mismatch("multirange operator", &l)),
    };
    Ok(Datum::Multirange { text, kind: lk })
}

/// Multirange predicate operators: `@>` (contains), `<@` (contained by), `&&`
/// (overlaps). `@>`/`<@` accept a multirange, a range, or a bare element on the
/// contained side; `&&` accepts a multirange or a range on either side.
pub(crate) fn multirange_op<'a>(
    operator: BinaryOp,
    l: Datum<'a>,
    r: Datum<'a>,
    arena: &'a Arena,
) -> Result<Datum<'a>, SqlError> {
    use BinaryOp::{ContainedBy, Contains, Overlaps};
    if l.is_null() || r.is_null() {
        return Ok(Datum::Null);
    }
    match operator {
        Contains => multirange_contains(l, r, arena),
        ContainedBy => multirange_contains(r, l, arena),
        Overlaps => {
            let (lt, lk) = as_multirange_coerce(&l, arena)?;
            let (rt, rk) = as_multirange_coerce(&r, arena)?;
            if lk != rk {
                return Err(range_mismatch());
            }
            Ok(Datum::Bool(range::multirange_overlaps(lt, rt, lk)?))
        }
        _ => Err(type_mismatch("multirange operator", &l)),
    }
}

/// Whether `container` (a multirange) holds `contained` — a multirange, a range,
/// or a bare element.
fn multirange_contains<'a>(
    container: Datum<'a>,
    contained: Datum<'a>,
    arena: &'a Arena,
) -> Result<Datum<'a>, SqlError> {
    let (ct, ck) = as_multirange(&container)?;
    let held = match contained {
        Datum::Multirange { text, kind } => {
            if kind != ck {
                return Err(range_mismatch());
            }
            range::multirange_contains_multirange(ct, text, ck, arena)?
        }
        Datum::Range { text, kind } => {
            if kind != ck {
                return Err(range_mismatch());
            }
            let mr = range::multirange_from_range(text, ck, arena)?;
            range::multirange_contains_multirange(ct, mr, ck, arena)?
        }
        element => {
            let element_text = arena.alloc_str_display(element).map_err(|_| arena_full())?;
            range::multirange_contains_elem(ct, ck, element_text)?
        }
    };
    Ok(Datum::Bool(held))
}

/// Integer bitwise operators (`& | # << >>`). Both operands must be integers.
pub(crate) fn bitwise<'a>(operator: BinaryOp, l: Datum<'a>, r: Datum<'a>) -> Result<Datum<'a>, SqlError> {
    use BinaryOp::*;
    let int = |d: &Datum| -> Result<i64, SqlError> {
        match d {
            Datum::Int4(v) => Ok(i64::from(*v)),
            Datum::Int8(v) => Ok(*v),
            other => Err(type_mismatch("bitwise operator requires integers", other)),
        }
    };
    if l.is_null() || r.is_null() {
        return Ok(Datum::Null);
    }
    let (a, b) = (int(&l)?, int(&r)?);
    let v = match operator {
        BitAnd => a & b,
        BitOr => a | b,
        BitXor => a ^ b,
        Shl => a << (b & 63),
        Shr => a >> (b & 63),
        _ => unreachable!("bitwise only"),
    };
    // Result width follows the wider operand (int8 if either is int8).
    if matches!(l, Datum::Int8(_)) || matches!(r, Datum::Int8(_)) {
        Ok(Datum::Int8(v))
    } else {
        Ok(Datum::Int4(v as i32))
    }
}
