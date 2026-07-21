//! Binary-operator evaluation split out of the expression evaluator: the array
//! set operators (`@> <@ &&`) and the range predicate and set operators. These
//! take already-evaluated `Datum`s (they do not walk the AST), so they depend
//! only on the value modules, not on `eval_full`.

use crate::mem::arena::Arena;
use crate::sql::ast::BinaryOp;
use crate::sql::numeric::{self, Numeric};
use crate::sql::types::{Datum, RangeKind};
use crate::sql::{array, datetime, range};
use crate::sql_err;

use super::{
    arena_full, as_f64, as_i64, bad_text, interval_cmp_value, parse_bool, parse_uuid, sqlstate,
    type_mismatch, type_name_of, validate_bits, SqlError,
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
