//! Binary-operator evaluation split out of the expression evaluator: the array
//! set operators (`@> <@ &&`) and the range predicate and set operators. These
//! take already-evaluated `Datum`s (they do not walk the AST), so they depend
//! only on the value modules, not on `eval_full`.

use crate::mem::arena::Arena;
use crate::sql::array;
use crate::sql::ast::BinaryOp;
use crate::sql::range;
use crate::sql::types::{Datum, RangeKind};
use crate::sql_err;

use super::{
    arena_full, as_i64, compare_datums, sqlstate, type_mismatch, type_name_of, validate_bits,
    SqlError,
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
