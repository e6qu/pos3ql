//! Binary-operator evaluation split out of the expression evaluator: the array
//! set operators (`@> <@ &&`) and the range predicate and set operators. These
//! take already-evaluated `Datum`s (they do not walk the AST), so they depend
//! only on the value modules, not on `eval_full`.

use crate::mem::arena::Arena;
use crate::sql::ast::{BinaryOp, Collation, UnaryOp};
use crate::sql::numeric::{self, Numeric};
use crate::sql::types::{Datum, Interval, RangeKind, RecordField};
use crate::sql::{array, datetime, range};
use crate::sql_err;

use super::cast_to;
use crate::sql::types::ColType;

use super::{
    SqlError, arena_full, as_f64, as_i64, bad_text, cast_to_text, concat, datum_f64, datum_numeric,
    division_by_zero, interval_cmp_value, json_exists, json_get, json_path, jsonb_concat,
    jsonb_delete, jsonb_delete_path, like_match, num_factor, out_of_range, overflow, parse_bool,
    parse_uuid, sqlstate, to_numeric, type_mismatch, type_name_of, validate_bits,
};

/// Text comparison is a distinct choke point because byte ordering is correct
/// only for bytewise collations. The default identity always delegates to the
/// startup-owned comparator instead of the process-global locale.
pub(crate) fn compare_text_collated<'a>(
    operator: BinaryOp,
    l: Datum<'a>,
    r: Datum<'a>,
    l_unknown: bool,
    r_unknown: bool,
    collation: Collation,
    catalog: Option<&dyn super::CatalogAccess>,
) -> Result<Datum<'a>, SqlError> {
    if l.is_null() || r.is_null() {
        return Ok(Datum::Null);
    }
    let l = if l_unknown { coerce_unknown(l, &r)? } else { l };
    let r = if r_unknown { coerce_unknown(r, &l)? } else { r };
    let (left, right) = match (l, r) {
        (Datum::Text(left), Datum::Text(right)) => (left, right),
        (Datum::Bpchar(left), Datum::Bpchar(right)) => {
            (left.trim_end_matches(' '), right.trim_end_matches(' '))
        }
        (Datum::Bpchar(left), Datum::Text(right)) => (left.trim_end_matches(' '), right),
        (Datum::Text(left), Datum::Bpchar(right)) => (left, right.trim_end_matches(' ')),
        _ => return compare(operator, l, r, false, false),
    };
    let ordering = match collation {
        Collation::None | Collation::C | Collation::Posix | Collation::UcsBasic => left.cmp(right),
        Collation::Default | Collation::Catalog(_) => catalog
            .ok_or_else(|| {
                sql_err!(
                    sqlstate::INTERNAL_ERROR,
                    "database collation comparator is unavailable"
                )
            })?
            .compare_text(collation, left, right)?,
    };
    let out = match operator {
        BinaryOp::Eq => ordering.is_eq(),
        BinaryOp::NotEq => ordering.is_ne(),
        BinaryOp::Lt => ordering.is_lt(),
        BinaryOp::LtEq => ordering.is_le(),
        BinaryOp::Gt => ordering.is_gt(),
        BinaryOp::GtEq => ordering.is_ge(),
        _ => unreachable!(),
    };
    Ok(Datum::Bool(out))
}

/// An unknown-type literal facing an array operand takes the array's type, the
/// way [`coerce_unknown`] gives it the type of a scalar one. It cannot live in
/// `coerce_unknown` itself: building an array value needs an arena, and the
/// callers that want an order rather than an operator do not have one.
fn coerce_unknown_array<'a>(
    v: Datum<'a>,
    other: &Datum,
    arena: &'a Arena,
) -> Result<Datum<'a>, SqlError> {
    match (v, other) {
        (Datum::Text(_), Datum::Array { element, .. }) => {
            cast_to(v, ColType::Array(*element), arena)
        }
        _ => Ok(v),
    }
}

pub(crate) fn array_set_op<'a>(
    operator: BinaryOp,
    l: Datum<'a>,
    r: Datum<'a>,
) -> Result<Datum<'a>, SqlError> {
    use BinaryOp::{ContainedBy, Contains, Overlaps};
    if l.is_null() || r.is_null() {
        return Ok(Datum::Null);
    }
    let (
        Datum::Array {
            element: le,
            raw: lr,
        },
        Datum::Array {
            element: re,
            raw: rr,
        },
    ) = (l, r)
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
    let subset =
        |sub_elem, sub_raw: &'a [u8], sup_elem, sup_raw: &'a [u8]| -> Result<bool, SqlError> {
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

/// `jsonb @> jsonb` (`Contains`) and `jsonb <@ jsonb` (`ContainedBy`) — deep
/// containment. Both operands must be `jsonb`; plain `json` has no containment
/// operator (PostgreSQL rejects it), so a non-jsonb operand errors.
pub(crate) fn jsonb_contains<'a>(
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
    // A jsonb operand contributes its stored text; an unknown string literal
    // coerces to jsonb (validated by the parse below). Anything else — a plain
    // `json`, a text column — has no containment operator.
    let jsonb_text = |d: &Datum<'a>, unknown: bool| -> Result<&'a str, SqlError> {
        match d {
            Datum::Json { text, jsonb: true } => Ok(text),
            Datum::Text(text) if unknown => Ok(text),
            _ => Err(sql_err!(
                sqlstate::UNDEFINED_FUNCTION,
                "operator does not exist: jsonb {} json — cast to jsonb",
                if operator == BinaryOp::Contains {
                    "@>"
                } else {
                    "<@"
                }
            )),
        }
    };
    let (container_text, contained_text) = if operator == BinaryOp::Contains {
        (jsonb_text(&l, l_unknown)?, jsonb_text(&r, r_unknown)?)
    } else {
        (jsonb_text(&r, r_unknown)?, jsonb_text(&l, l_unknown)?)
    };
    let container = crate::sql::json::parse(container_text, arena)?;
    let contained = crate::sql::json::parse(contained_text, arena)?;
    Ok(Datum::Bool(crate::sql::json::contains(
        &container, &contained,
    )))
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
    sql_err!(
        sqlstate::UNDEFINED_FUNCTION,
        "operator requires matching range types"
    )
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
            Ok(Datum::Bool(range::contains_range(
                container_text,
                text,
                container_kind,
            )?))
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
pub(crate) fn bit_concat<'a>(
    l: Datum<'a>,
    r: Datum<'a>,
    arena: &'a Arena,
) -> Result<Datum<'a>, SqlError> {
    if l.is_null() || r.is_null() {
        return Ok(Datum::Null);
    }
    let (a, b) = (bit_operand(&l)?, bit_operand(&r)?);
    let out = arena
        .alloc_slice_with(a.len() + b.len(), |i| {
            if i < a.len() {
                a.as_bytes()[i]
            } else {
                b.as_bytes()[i - a.len()]
            }
        })
        .map_err(|_| arena_full())?;
    Ok(Datum::Bit {
        bits: unsafe { core::str::from_utf8_unchecked(out) },
        varying: true,
    })
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
        return Err(sql_err!(
            sqlstate::STRING_DATA_LENGTH_MISMATCH,
            "cannot {} bit strings of different sizes",
            verb
        ));
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
    Ok(Datum::Bit {
        bits: unsafe { core::str::from_utf8_unchecked(out) },
        varying,
    })
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
    let left = if matches!(operator, BinaryOp::Shl) {
        count
    } else {
        -count
    };
    let len = bits.len() as i64;
    let src = bits.as_bytes();
    let out = arena
        .alloc_slice_with(bits.len(), |i| {
            let from = i as i64 + left;
            if (0..len).contains(&from) {
                src[from as usize]
            } else {
                b'0'
            }
        })
        .map_err(|_| arena_full())?;
    Ok(Datum::Bit {
        bits: unsafe { core::str::from_utf8_unchecked(out) },
        varying,
    })
}

/// Comparison of two bit strings: PostgreSQL compares bit-by-bit, and when one
/// is a prefix of the other the shorter sorts first — exactly the lexicographic
/// order of the `'0'`/`'1'` strings.
pub(crate) fn compare_bits<'a>(
    operator: BinaryOp,
    l: Datum<'a>,
    r: Datum<'a>,
) -> Result<Datum<'a>, SqlError> {
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
    // ORDER BY, DISTINCT and the aggregates ask for an order, not for a named
    // operator; `=` is what PostgreSQL reports when one of those meets two
    // types it has no comparison for.
    compare_datums_as("=", l, r)
}

/// Compares values under a resolved SQL collation.  Text and blank-padded char
/// share the database comparator; every other type retains its native order.
pub fn compare_datums_collated(
    storage: &crate::storage::Storage,
    collation: Collation,
    left: &Datum<'_>,
    right: &Datum<'_>,
) -> Result<core::cmp::Ordering, SqlError> {
    let (left, right) = match (left, right) {
        (Datum::Text(left), Datum::Text(right)) => (*left, *right),
        (Datum::Bpchar(left), Datum::Bpchar(right)) => {
            (left.trim_end_matches(' '), right.trim_end_matches(' '))
        }
        (Datum::Bpchar(left), Datum::Text(right)) => (left.trim_end_matches(' '), *right),
        (Datum::Text(left), Datum::Bpchar(right)) => (*left, right.trim_end_matches(' ')),
        _ => return compare_datums(left, right),
    };
    storage.compare_text(collation, left, right)
}

/// The evaluator-facing form of [`compare_datums_collated`].  Aggregate and
/// expression execution have a catalog capability rather than a `Storage`
/// reference, but must retain exactly the same default-collation contract.
pub fn compare_datums_with_catalog(
    collation: Collation,
    catalog: Option<&dyn super::CatalogAccess>,
    left: &Datum<'_>,
    right: &Datum<'_>,
) -> Result<core::cmp::Ordering, SqlError> {
    let (left, right) = match (left, right) {
        (Datum::Text(left), Datum::Text(right)) => (*left, *right),
        (Datum::Bpchar(left), Datum::Bpchar(right)) => {
            (left.trim_end_matches(' '), right.trim_end_matches(' '))
        }
        (Datum::Bpchar(left), Datum::Text(right)) => (left.trim_end_matches(' '), *right),
        (Datum::Text(left), Datum::Bpchar(right)) => (*left, right.trim_end_matches(' ')),
        _ => return compare_datums(left, right),
    };
    match collation {
        Collation::None | Collation::C | Collation::Posix | Collation::UcsBasic => {
            Ok(left.cmp(right))
        }
        Collation::Default | Collation::Catalog(_) => catalog
            .ok_or_else(|| {
                sql_err!(
                    sqlstate::INTERNAL_ERROR,
                    "database collation comparator is unavailable"
                )
            })?
            .compare_text(collation, left, right),
    }
}

/// Hashes a key tuple for the value index (the uniqueness/foreign-key probe
/// accelerator). The hash MUST agree with [`compare_datums`] equality — equal
/// values hash equal — so the index never misses a true collision; unequal
/// values that happen to share a hash are only false positives, re-verified by
/// the caller against the actual row. Types whose canonical form is subtle
/// (numeric, interval, timetz, ranges, arrays, non-jsonb json) hash to a
/// per-type constant: correct, but every value collides and is verified by the
/// caller; the common key types (integers, text,
/// char, uuid, dates/times, bytea, bit, jsonb, float) hash precisely. Writes are
/// little-endian so the hash is identical across platforms (the simulator
/// replays bit-for-bit).
pub fn hash_key(values: &[Datum], columns: &[u16]) -> u64 {
    use core::hash::Hasher as _;
    let mut hasher = crate::mem::fixed_map::Fnv1aHasher::default();
    for &col in columns {
        hash_datum(&values[col as usize], &mut hasher);
    }
    hasher.finish()
}

/// Hashes a tuple without ever excluding values equal under its SQL collation.
/// Locale equality has no portable fixed-width transform, so locale-capable
/// collation identities deliberately share one hash bucket and are rechecked.
pub fn hash_key_collated(values: &[Datum], columns: &[u16], collations: &[Collation]) -> u64 {
    use core::hash::Hasher as _;
    let mut hasher = crate::mem::fixed_map::Fnv1aHasher::default();
    for (index, &column) in columns.iter().enumerate() {
        let datum = &values[column as usize];
        if matches!(
            collations[index],
            Collation::Default | Collation::Catalog(_)
        ) && matches!(datum, Datum::Text(_) | Datum::Bpchar(_))
        {
            hasher.write(&[0x54]);
        } else {
            hash_datum(datum, &mut hasher);
        }
    }
    hasher.finish()
}

fn hash_datum(datum: &Datum, hasher: &mut crate::mem::fixed_map::Fnv1aHasher) {
    use core::hash::Hasher as _;
    // Canonical float bits: -0.0 and 0.0 are equal, and PostgreSQL's btree
    // treats every NaN as equal to every other, so both collapse to one form.
    let canon_float = |f: f64| -> u64 {
        if f == 0.0 {
            0
        } else if f.is_nan() {
            f64::NAN.to_bits()
        } else {
            f.to_bits()
        }
    };
    // A per-variant tag keeps distinct types from aliasing in a multi-column
    // key. Types that compare equal across representations share a tag on
    // purpose (an int is an int at any width; text and blank-stripped char
    // compare equal; a timestamp and timestamptz denote the same instant).
    match datum {
        Datum::Null => hasher.write(&[0]),
        Datum::PgDdlCommand => hasher.write(&[36]),
        Datum::Bool(b) => hasher.write(&[1, *b as u8]),
        Datum::Int2(v) => {
            hasher.write(&[2]);
            hasher.write(&(*v as i64).to_le_bytes());
        }
        Datum::Int4(v) => {
            hasher.write(&[2]);
            hasher.write(&(*v as i64).to_le_bytes());
        }
        Datum::Oid(v) => {
            hasher.write(&[35]);
            hasher.write(&v.to_le_bytes());
        }
        Datum::Int8(v) => {
            hasher.write(&[2]);
            hasher.write(&v.to_le_bytes());
        }
        Datum::Float4(f) => {
            hasher.write(&[3]);
            hasher.write(&canon_float(*f as f64).to_le_bytes());
        }
        Datum::Float8(f) => {
            hasher.write(&[3]);
            hasher.write(&canon_float(*f).to_le_bytes());
        }
        Datum::Text(s) => {
            hasher.write(&[4]);
            hasher.write(s.as_bytes());
        }
        Datum::Bpchar(s) => {
            hasher.write(&[4]);
            hasher.write(s.trim_end_matches(' ').as_bytes());
        }
        Datum::Regtype { referenced_oid, .. } => {
            hasher.write(&[33]);
            hasher.write(&referenced_oid.to_le_bytes());
        }
        Datum::RegObject {
            type_oid,
            referenced_oid,
            ..
        } => {
            hasher.write(&[34]);
            hasher.write(&type_oid.to_le_bytes());
            hasher.write(&referenced_oid.to_le_bytes());
        }
        Datum::Date(v) => {
            hasher.write(&[5]);
            hasher.write(&(*v as i64).to_le_bytes());
        }
        Datum::Timestamp(v) | Datum::Timestamptz(v) => {
            hasher.write(&[6]);
            hasher.write(&v.to_le_bytes());
        }
        Datum::Time(v) => {
            hasher.write(&[7]);
            hasher.write(&v.to_le_bytes());
        }
        Datum::Uuid(b) => {
            hasher.write(&[8]);
            hasher.write(b);
        }
        Datum::Bytea(b) => {
            hasher.write(&[9]);
            hasher.write(b);
        }
        Datum::Bit { bits, .. } => {
            hasher.write(&[10]);
            hasher.write(bits.as_bytes());
        }
        Datum::Json { text, jsonb: true } => {
            hasher.write(&[11]);
            hasher.write(text.as_bytes());
        }
        Datum::TsVector(text) => {
            hasher.write(&[37]);
            hasher.write(text.as_bytes());
        }
        Datum::TsQuery(text) => {
            hasher.write(&[38]);
            crate::sql::full_text::emit_query_hash(text.as_str(), |bytes| hasher.write(bytes));
        }
        // Subtle-canonical or non-comparable types: one constant per type. Every
        // value collides, so the caller verifies each candidate — correct, just
        // not accelerated for these column types.
        Datum::Numeric(_) => hasher.write(&[20]),
        Datum::Interval(_) => hasher.write(&[21]),
        Datum::Timetz(..) => hasher.write(&[22]),
        Datum::Range { .. } => hasher.write(&[23]),
        Datum::Multirange { .. } => hasher.write(&[24]),
        Datum::Geometry { .. } => hasher.write(&[39]),
        Datum::Array { .. } => hasher.write(&[25]),
        Datum::Int2Vector(raw) => {
            hasher.write(&[32]);
            hasher.write(raw);
        }
        Datum::OidVector(raw) => {
            hasher.write(&[33]);
            hasher.write(raw);
        }
        Datum::Record(_) | Datum::Composite { .. } | Datum::CompositeText { .. } => {
            hasher.write(&[26])
        }
        Datum::Json { jsonb: false, .. } => hasher.write(&[27]),
        // Network addresses hash by their comparison key (family, address,
        // mask), so equal values hash equal and distinct ones rarely collide.
        Datum::Inet(net) | Datum::Cidr(net) => {
            hasher.write(&[28]);
            hasher.write(&net.cmp_key());
        }
        Datum::Macaddr(b) => {
            hasher.write(&[29]);
            hasher.write(b);
        }
        Datum::Macaddr8(b) => {
            hasher.write(&[30]);
            hasher.write(b);
        }
        // Enum values are equal iff the same member; the label uniquely
        // identifies a member within a type, and cross-type comparison is
        // rejected at plan time, so hashing by label is consistent with `=`.
        Datum::Enum { label, .. } => {
            hasher.write(&[31]);
            hasher.write(label.as_bytes());
        }
    }
}

/// [`compare_datums`], reporting an undefined comparison under the operator the
/// query actually wrote — `true < 1` is `boolean < integer`, not `boolean =
/// integer`.
pub(crate) fn compare_datums_as(
    operator: &str,
    l: &Datum,
    r: &Datum,
) -> Result<core::cmp::Ordering, SqlError> {
    use core::cmp::Ordering;
    let ord = match (l, r) {
        (Datum::Bool(a), Datum::Bool(b)) => a.cmp(b),
        (Datum::Text(a), Datum::Text(b)) => a.cmp(b),
        (Datum::TsVector(a), Datum::TsVector(b)) => {
            crate::sql::full_text::compare_vector(a.as_str(), b.as_str())
        }
        (Datum::TsQuery(a), Datum::TsQuery(b)) => {
            crate::sql::full_text::compare_query(a.as_str(), b.as_str())
        }
        (Datum::Bpchar(a), Datum::Bpchar(b)) => {
            a.trim_end_matches(' ').cmp(b.trim_end_matches(' '))
        }
        (Datum::Bpchar(a), Datum::Text(b)) => a.trim_end_matches(' ').cmp(*b),
        (Datum::Text(a), Datum::Bpchar(b)) => (*a).cmp(b.trim_end_matches(' ')),
        (
            Datum::Regtype {
                referenced_oid: a, ..
            },
            Datum::Regtype {
                referenced_oid: b, ..
            },
        ) => a.cmp(b),
        (Datum::Regtype { referenced_oid, .. }, Datum::Oid(value)) => {
            referenced_oid.cmp(&(*value as i32))
        }
        (Datum::Oid(value), Datum::Regtype { referenced_oid, .. }) => {
            (*value as i32).cmp(referenced_oid)
        }
        (Datum::Regtype { referenced_oid, .. }, Datum::Int2(value)) => {
            referenced_oid.cmp(&i32::from(*value))
        }
        (Datum::Int2(value), Datum::Regtype { referenced_oid, .. }) => {
            i32::from(*value).cmp(referenced_oid)
        }
        (Datum::Regtype { referenced_oid, .. }, Datum::Int4(value)) => referenced_oid.cmp(value),
        (Datum::Int4(value), Datum::Regtype { referenced_oid, .. }) => value.cmp(referenced_oid),
        (Datum::Regtype { referenced_oid, .. }, Datum::Int8(value)) => {
            i64::from(*referenced_oid).cmp(value)
        }
        (Datum::Int8(value), Datum::Regtype { referenced_oid, .. }) => {
            value.cmp(&i64::from(*referenced_oid))
        }
        (
            Datum::RegObject {
                type_oid: at,
                referenced_oid: a,
                ..
            },
            Datum::RegObject {
                type_oid: bt,
                referenced_oid: b,
                ..
            },
        ) if at == bt => a.cmp(b),
        (Datum::RegObject { referenced_oid, .. }, Datum::Oid(value)) => {
            referenced_oid.cmp(&(*value as i32))
        }
        (Datum::Oid(value), Datum::RegObject { referenced_oid, .. }) => {
            (*value as i32).cmp(referenced_oid)
        }
        (Datum::RegObject { referenced_oid, .. }, Datum::Int2(value)) => {
            referenced_oid.cmp(&i32::from(*value))
        }
        (Datum::Int2(value), Datum::RegObject { referenced_oid, .. }) => {
            i32::from(*value).cmp(referenced_oid)
        }
        (Datum::RegObject { referenced_oid, .. }, Datum::Int4(value)) => referenced_oid.cmp(value),
        (Datum::Int4(value), Datum::RegObject { referenced_oid, .. }) => value.cmp(referenced_oid),
        (Datum::RegObject { referenced_oid, .. }, Datum::Int8(value)) => {
            i64::from(*referenced_oid).cmp(value)
        }
        (Datum::Int8(value), Datum::RegObject { referenced_oid, .. }) => {
            value.cmp(&i64::from(*referenced_oid))
        }
        (Datum::Date(a), Datum::Date(b)) => a.cmp(b),
        (Datum::Timestamp(a), Datum::Timestamp(b))
        | (Datum::Timestamptz(a), Datum::Timestamptz(b))
        | (Datum::Timestamp(a), Datum::Timestamptz(b))
        | (Datum::Timestamptz(a), Datum::Timestamp(b)) => a.cmp(b),
        (Datum::Time(a), Datum::Time(b)) => a.cmp(b),
        // PostgreSQL orders by the instant each denotes, then by zone, so two
        // values naming the same instant in different zones are ordered but
        // never equal — `12:00+00` and `13:00+01` are both 12:00 UTC, and the
        // first sorts after. The zone tiebreak runs on PostgreSQL's westward
        // sign, the negation of the stored offset.
        (Datum::Timetz(a, za), Datum::Timetz(b, zb)) => (a - *za as i64 * 1_000_000)
            .cmp(&(b - *zb as i64 * 1_000_000))
            .then_with(|| (-za).cmp(&-zb)),
        // Only `jsonb` compares; `json` keeps its original text, so equal
        // documents can differ byte for byte and PostgreSQL offers no operator.
        (
            Datum::Json {
                text: a,
                jsonb: true,
            },
            Datum::Json {
                text: b,
                jsonb: true,
            },
        ) => a.cmp(b),
        (Datum::Json { jsonb: false, .. }, Datum::Json { .. })
        | (Datum::Json { .. }, Datum::Json { jsonb: false, .. }) => {
            return Err(sql_err!(
                sqlstate::UNDEFINED_FUNCTION,
                "operator does not exist: json = json"
            ));
        }
        (
            Datum::Record(a) | Datum::Composite { fields: a, .. },
            Datum::Record(b) | Datum::Composite { fields: b, .. },
        ) => {
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
        (
            Datum::CompositeText {
                slot: left_slot,
                text: left,
                ..
            },
            Datum::CompositeText {
                slot: right_slot,
                text: right,
                ..
            },
        ) => left_slot.cmp(right_slot).then_with(|| left.cmp(right)),
        (Datum::Array { element, raw: ra }, Datum::Array { raw: rb, .. }) => {
            // PostgreSQL compares contents first, then dimensionality and bounds.
            let (length_a, length_b) = (array::len(ra), array::len(rb));
            for i in 0..length_a.min(length_b) {
                let x = array::get(ra, *element, i).unwrap_or(Datum::Null);
                let y = array::get(rb, *element, i).unwrap_or(Datum::Null);
                let c = compare_datums(&x, &y)?;
                if !c.is_eq() {
                    return Ok(c);
                }
            }
            let content = length_a.cmp(&length_b);
            if !content.is_eq() {
                return Ok(content);
            }
            let shape_a = array::shape(ra).expect("array datum invariant");
            let shape_b = array::shape(rb).expect("array datum invariant");
            for index in 0..shape_a.dimension_count().min(shape_b.dimension_count()) {
                let dimensions = shape_a.dimension(index).cmp(&shape_b.dimension(index));
                if !dimensions.is_eq() {
                    return Ok(dimensions);
                }
                let bounds = shape_a.lower_bound(index).cmp(&shape_b.lower_bound(index));
                if !bounds.is_eq() {
                    return Ok(bounds);
                }
            }
            shape_a.dimension_count().cmp(&shape_b.dimension_count())
        }
        (Datum::Date(a), Datum::Timestamp(b) | Datum::Timestamptz(b)) => {
            (i64::from(*a) * 86_400_000_000).cmp(b)
        }
        (Datum::Timestamp(a) | Datum::Timestamptz(a), Datum::Date(b)) => {
            a.cmp(&(i64::from(*b) * 86_400_000_000))
        }
        (Datum::Uuid(a), Datum::Uuid(b)) => a.cmp(b),
        (Datum::Bytea(a), Datum::Bytea(b)) => a.cmp(b),
        // Network addresses order by PostgreSQL's network_cmp: family, then
        // address bytes, then mask length. `inet` and `cidr` compare across.
        (Datum::Inet(a) | Datum::Cidr(a), Datum::Inet(b) | Datum::Cidr(b)) => {
            a.cmp_key().cmp(&b.cmp_key())
        }
        (Datum::Macaddr(a), Datum::Macaddr(b)) => a.cmp(b),
        (Datum::Macaddr8(a), Datum::Macaddr8(b)) => a.cmp(b),
        // Enum values order by their member sort key (PostgreSQL's
        // enumsortorder), never by label text. The planner rejects comparing
        // two different enum types, so equal sorts here mean equal members.
        (Datum::Enum { sort: a, .. }, Datum::Enum { sort: b, .. }) => {
            a.partial_cmp(b).unwrap_or(Ordering::Equal)
        }
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
                    "operator does not exist: {} {} {}",
                    ka.name(),
                    operator,
                    kb.name()
                ));
            }
            range::cmp_ranges(a, b, *ka)?
        }
        (Datum::Multirange { text: a, kind: ka }, Datum::Multirange { text: b, kind: kb }) => {
            if ka != kb {
                return Err(sql_err!(
                    sqlstate::UNDEFINED_FUNCTION,
                    "operator does not exist: {} {} {}",
                    ka.multirange_name(),
                    operator,
                    kb.multirange_name()
                ));
            }
            range::cmp_multiranges(a, b, *ka)?
        }
        // Numeric vs integer: compare exactly via numeric.
        (Datum::Numeric(_), Datum::Int2(_) | Datum::Int4(_) | Datum::Oid(_) | Datum::Int8(_))
        | (Datum::Int2(_) | Datum::Int4(_) | Datum::Oid(_) | Datum::Int8(_), Datum::Numeric(_)) => {
            // Fall through to the float comparison below only if exactness is
            // not required; integers convert to numeric exactly.
            return compare_numeric_int(l, r);
        }
        _ => {
            if let (Some(a), Some(b)) = (as_i64(l), as_i64(r)) {
                a.cmp(&b)
            } else if let (Some(a), Some(b)) = (as_f64(l), as_f64(r)) {
                // PostgreSQL float comparison treats NaN as largest.
                return Ok(a
                    .partial_cmp(&b)
                    .unwrap_or_else(|| match (a.is_nan(), b.is_nan()) {
                        (true, false) => Ordering::Greater,
                        (false, true) => Ordering::Less,
                        _ => Ordering::Equal,
                    }));
            } else {
                // PostgreSQL reports incompatible comparisons as
                // "operator does not exist" (42883), not a datatype mismatch.
                return Err(sql_err!(
                    sqlstate::UNDEFINED_FUNCTION,
                    "operator does not exist: {} {} {}",
                    type_name_of(l),
                    operator,
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
        Datum::Int2(_) => {
            let x: i64 = s.trim().parse().map_err(|_| bad_text(s, "smallint"))?;
            Datum::Int2(i16::try_from(x).map_err(|_| overflow("smallint"))?)
        }
        Datum::Int4(_) => Datum::Int4(s.trim().parse().map_err(|_| bad_text(s, "integer"))?),
        Datum::Oid(_) => Datum::Oid(super::cast::parse_oid(s)?),
        Datum::Int8(_) => Datum::Int8(s.trim().parse().map_err(|_| bad_text(s, "bigint"))?),
        Datum::Float8(_) => Datum::Float8(
            s.trim()
                .parse()
                .map_err(|_| bad_text(s, "double precision"))?,
        ),
        Datum::Float4(_) => {
            let t = s.trim();
            let x: f32 = t.parse().map_err(|_| bad_text(s, "real"))?;
            if x.is_infinite() && !super::cast::is_infinity_text(t) {
                return Err(out_of_range(t, "real"));
            }
            Datum::Float4(x)
        }
        Datum::Bool(_) => Datum::Bool(parse_bool(s)?),
        Datum::Date(_) => Datum::Date(datetime::parse_date(s)?),
        Datum::Timestamp(_) => Datum::Timestamp(datetime::parse_timestamp(s, false)?),
        Datum::Timestamptz(_) => Datum::Timestamptz(datetime::parse_timestamp(s, true)?),
        Datum::Uuid(_) => Datum::Uuid(parse_uuid(s)?),
        Datum::Bpchar(_) => Datum::Bpchar(s),
        Datum::Time(_) => Datum::Time(datetime::parse_time(s)?),
        Datum::Timetz(..) => {
            let (t, zone) = datetime::parse_timetz(s)?;
            Datum::Timetz(
                t,
                zone.unwrap_or_else(|| {
                    crate::sql::timezone::session()
                        .resolve(datetime::now_micros())
                        .0
                }),
            )
        }
        Datum::Interval(_) => Datum::Interval(datetime::parse_interval(s)?),
        Datum::Inet(_) => {
            Datum::Inet(crate::sql::net::parse_inet(s).ok_or_else(|| bad_text(s, "inet"))?)
        }
        Datum::Cidr(_) => {
            Datum::Cidr(crate::sql::net::parse_cidr(s).ok_or_else(|| bad_text(s, "cidr"))?)
        }
        Datum::Macaddr(_) => {
            Datum::Macaddr(crate::sql::net::parse_macaddr(s).ok_or_else(|| bad_text(s, "macaddr"))?)
        }
        Datum::Macaddr8(_) => Datum::Macaddr8(
            crate::sql::net::parse_macaddr8(s).ok_or_else(|| bad_text(s, "macaddr8"))?,
        ),
        _ => v,
    })
}

/// Range comparison operators (`=`, `<>`, `<`, `<=`, `>`, `>=`). Both operands
/// must be ranges of the same kind; ordering follows PostgreSQL `range_cmp`.
pub(crate) fn compare_ranges<'a>(
    operator: BinaryOp,
    l: Datum<'a>,
    r: Datum<'a>,
) -> Result<Datum<'a>, SqlError> {
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
    let symbol = match operator {
        BinaryOp::Eq => "=",
        BinaryOp::NotEq => "<>",
        BinaryOp::Lt => "<",
        BinaryOp::LtEq => "<=",
        BinaryOp::Gt => ">",
        _ => ">=",
    };
    let ord = compare_datums_as(symbol, &l, &r)?;
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
            return Ok(Datum::Interval(datetime::interval_scale(
                interval,
                num_factor(&r).expect("checked"),
                false,
            )));
        }
        (BinaryOp::Mul, _, Datum::Interval(interval)) if num_factor(&l).is_some() => {
            return Ok(Datum::Interval(datetime::interval_scale(
                interval,
                num_factor(&l).expect("checked"),
                false,
            )));
        }
        (BinaryOp::Div, Datum::Interval(interval), _) if num_factor(&r).is_some() => {
            return Ok(Datum::Interval(datetime::interval_scale(
                interval,
                num_factor(&r).expect("checked"),
                true,
            )));
        }
        (
            BinaryOp::Add | BinaryOp::Sub,
            dt @ (Datum::Timestamp(_) | Datum::Timestamptz(_) | Datum::Date(_)),
            Datum::Interval(interval),
        )
        | (
            BinaryOp::Add,
            Datum::Interval(interval),
            dt @ (Datum::Timestamp(_) | Datum::Timestamptz(_) | Datum::Date(_)),
        ) => {
            let base = match dt {
                Datum::Timestamp(t) | Datum::Timestamptz(t) => t,
                Datum::Date(d) => d as i64 * 86_400_000_000,
                _ => unreachable!(),
            };
            let signed = if operator == BinaryOp::Sub {
                Interval {
                    months: -interval.months,
                    days: -interval.days,
                    micros: -interval.micros,
                }
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
        // A time of day takes only the interval's time part and wraps within
        // the day; a timetz keeps the zone it already had.
        (BinaryOp::Add | BinaryOp::Sub, Datum::Time(t), Datum::Interval(interval))
        | (BinaryOp::Add, Datum::Interval(interval), Datum::Time(t)) => {
            let delta = if operator == BinaryOp::Sub {
                -interval.micros
            } else {
                interval.micros
            };
            return Ok(Datum::Time((t + delta).rem_euclid(86_400_000_000)));
        }
        (BinaryOp::Add | BinaryOp::Sub, Datum::Timetz(t, zone), Datum::Interval(interval))
        | (BinaryOp::Add, Datum::Interval(interval), Datum::Timetz(t, zone)) => {
            let delta = if operator == BinaryOp::Sub {
                -interval.micros
            } else {
                interval.micros
            };
            return Ok(Datum::Timetz((t + delta).rem_euclid(86_400_000_000), zone));
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
    // numeric (and neither is a float) -> numeric; real operator real -> real
    // (single precision); any other float pairing -> float8 (real mixed with
    // int/float8/numeric all widen to double precision).
    let either_numeric = matches!(l, Datum::Numeric(_)) || matches!(r, Datum::Numeric(_));
    let either_float = matches!(l, Datum::Float8(_) | Datum::Float4(_))
        || matches!(r, Datum::Float8(_) | Datum::Float4(_));
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
    // real operator real stays single precision (f32 arithmetic). Any other
    // float pairing falls through to the double-precision path below.
    if let (Datum::Float4(a), Datum::Float4(b)) = (l, r) {
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
            _ => unreachable!("Mod on floats rejected above"),
        };
        return Ok(Datum::Float4(out));
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
        None => Err(sql_err!(
            sqlstate::DATETIME_FIELD_OVERFLOW,
            "date out of range"
        )),
    }
}

/// int4 operator int4 yields int4 (with range check), as in PostgreSQL.
fn narrow_int<'a>(v: i64, l: &Datum, r: &Datum) -> Result<Datum<'a>, SqlError> {
    // The result type is the wider operand's — smallint op smallint stays
    // smallint and raises 22003 at ±32767, exactly PostgreSQL's rule.
    let width = |d: &Datum| match d {
        Datum::Int2(_) => 2u8,
        Datum::Int4(_) => 4,
        _ => 8,
    };
    match width(l).max(width(r)) {
        2 => match i16::try_from(v) {
            Ok(small) => Ok(Datum::Int2(small)),
            Err(_) => Err(overflow("smallint")),
        },
        4 => match i32::try_from(v) {
            Ok(small) => Ok(Datum::Int4(small)),
            Err(_) => Err(overflow("integer")),
        },
        _ => Ok(Datum::Int8(v)),
    }
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
pub(crate) fn bitwise<'a>(
    operator: BinaryOp,
    l: Datum<'a>,
    r: Datum<'a>,
) -> Result<Datum<'a>, SqlError> {
    use BinaryOp::*;
    let int = |d: &Datum| -> Result<i64, SqlError> {
        match d {
            Datum::Int2(v) => Ok(i64::from(*v)),
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
    let width = |d: &Datum| match d {
        Datum::Int2(_) => 2u8,
        Datum::Int4(_) => 4,
        _ => 8,
    };
    // A shift keeps its left operand's type (`1::int2 << 15` is smallint
    // -32768, truncating silently); the pairwise operators follow the wider
    // operand. Truncation, not overflow: PostgreSQL's C-level bit operators.
    let out_width = match operator {
        Shl | Shr => width(&l),
        _ => width(&l).max(width(&r)),
    };
    Ok(match out_width {
        2 => Datum::Int2(v as i16),
        4 => Datum::Int4(v as i32),
        _ => Datum::Int8(v),
    })
}

pub(crate) fn unary<'a>(
    operator: UnaryOp,
    v: Datum<'a>,
    arena: &'a Arena,
) -> Result<Datum<'a>, SqlError> {
    match (operator, v) {
        (_, Datum::Null) => Ok(Datum::Null),
        // ~bit flips every bit, preserving length and type.
        (UnaryOp::BitNot, Datum::Bit { bits, varying }) => {
            let out = arena
                .alloc_slice_with(bits.len(), |i| {
                    if bits.as_bytes()[i] == b'1' {
                        b'0'
                    } else {
                        b'1'
                    }
                })
                .map_err(|_| arena_full())?;
            Ok(Datum::Bit {
                bits: unsafe { core::str::from_utf8_unchecked(out) },
                varying,
            })
        }
        (UnaryOp::Neg, Datum::Int2(x)) => x
            .checked_neg()
            .map(Datum::Int2)
            .ok_or_else(|| overflow("smallint")),
        (UnaryOp::Neg, Datum::Int4(x)) => x
            .checked_neg()
            .map(Datum::Int4)
            .ok_or_else(|| overflow("integer")),
        (UnaryOp::Neg, Datum::Int8(x)) => x
            .checked_neg()
            .map(Datum::Int8)
            .ok_or_else(|| overflow("bigint")),
        (UnaryOp::Neg, Datum::Float4(x)) => Ok(Datum::Float4(-x)),
        (UnaryOp::Neg, Datum::Float8(x)) => Ok(Datum::Float8(-x)),
        (UnaryOp::Neg, Datum::Numeric(n)) => Ok(Datum::Numeric(Numeric {
            // Negating zero stays positive (no negative zero).
            sign: if n.is_zero() {
                numeric::Sign::Pos
            } else {
                match n.sign {
                    numeric::Sign::Pos => numeric::Sign::Neg,
                    numeric::Sign::Neg => numeric::Sign::Pos,
                    numeric::Sign::NaN => numeric::Sign::NaN,
                }
            },
            ..n
        })),
        (UnaryOp::Not, Datum::Bool(b)) => Ok(Datum::Bool(!b)),
        (UnaryOp::TextSearchNot, Datum::TsQuery(query)) => {
            crate::sql::full_text::not_query(query.as_str(), arena)
                .map(crate::sql::full_text::restore_query)
                .map(Datum::TsQuery)
        }
        (UnaryOp::BitNot, Datum::Int2(x)) => Ok(Datum::Int2(!x)),
        (UnaryOp::BitNot, Datum::Int4(x)) => Ok(Datum::Int4(!x)),
        (UnaryOp::BitNot, Datum::Int8(x)) => Ok(Datum::Int8(!x)),
        // ~inet inverts every address bit, preserving family and mask.
        (UnaryOp::BitNot, Datum::Inet(n) | Datum::Cidr(n)) => Ok(Datum::Inet(network_not(&n))),
        (UnaryOp::Neg, other) => Err(type_mismatch("-", &other)),
        (UnaryOp::Not, other) => match super::boolean_argument(other, "NOT")? {
            Datum::Bool(b) => Ok(Datum::Bool(!b)),
            _ => Ok(Datum::Null),
        },
        (UnaryOp::BitNot, other) => Err(type_mismatch("~", &other)),
        (UnaryOp::TextSearchNot, other) => Err(type_mismatch("!!", &other)),
        // The prefix arithmetic operators are evaluated through the functions
        // they compute, so a datum never reaches here wearing one.
        (UnaryOp::SquareRoot | UnaryOp::CubeRoot | UnaryOp::AbsoluteValue, _) => {
            unreachable!("prefix arithmetic operators evaluate through their function")
        }
    }
}

/// PostgreSQL row comparison (`(a,b) = (c,d)`, `<`, …): three-valued and
/// short-circuiting, distinct from the total order `compare_datums` gives
/// ORDER BY. Equality scans every pair — a definite inequality is `false`, an
/// all-equal row is `true`, and an otherwise-equal row with a NULL pair is
/// NULL. Ordering walks left to right — the first non-equal pair decides, and a
/// NULL pair reached before then yields NULL. The rows must have equal arity.
fn row_compare<'a>(
    operator: BinaryOp,
    a: &[RecordField<'a>],
    b: &[RecordField<'a>],
) -> Result<Datum<'a>, SqlError> {
    use BinaryOp::*;
    if a.len() != b.len() {
        return Err(sql_err!(
            sqlstate::SYNTAX_ERROR,
            "unequal number of entries in row expressions"
        ));
    }
    match operator {
        Eq | NotEq => {
            let mut saw_null = false;
            for (x, y) in a.iter().zip(b) {
                if x.value.is_null() || y.value.is_null() {
                    saw_null = true;
                } else if !compare_datums(&x.value, &y.value)?.is_eq() {
                    return Ok(Datum::Bool(operator == NotEq));
                }
            }
            if saw_null {
                Ok(Datum::Null)
            } else {
                Ok(Datum::Bool(operator == Eq))
            }
        }
        _ => {
            for (x, y) in a.iter().zip(b) {
                if x.value.is_null() || y.value.is_null() {
                    return Ok(Datum::Null);
                }
                let ordering = compare_datums(&x.value, &y.value)?;
                if !ordering.is_eq() {
                    return Ok(Datum::Bool(match operator {
                        Lt | LtEq => ordering.is_lt(),
                        _ => ordering.is_gt(),
                    }));
                }
            }
            Ok(Datum::Bool(matches!(operator, LtEq | GtEq)))
        }
    }
}

/// SQL equality as the membership tests (`IN`, `NOT IN`) need it: three-valued,
/// returning `None` for unknown. Rows go through [`row_compare`], so a NULL
/// *inside* a row makes the membership unknown; `compare_datums` alone gives
/// the total order ORDER BY wants, where a NULL field is just another value.
pub(crate) fn membership_eq<'a>(l: &Datum<'a>, r: &Datum<'a>) -> Result<Option<bool>, SqlError> {
    if l.is_null() || r.is_null() {
        return Ok(None);
    }
    if let (
        Datum::Record(a) | Datum::Composite { fields: a, .. },
        Datum::Record(b) | Datum::Composite { fields: b, .. },
    ) = (l, r)
    {
        return Ok(match row_compare(BinaryOp::Eq, a, b)? {
            Datum::Bool(equal) => Some(equal),
            _ => None,
        });
    }
    Ok(Some(compare_datums(l, r)?.is_eq()))
}

// --- network address operators ---------------------------------------------

use crate::sql::net::NetAddr;

/// Whether the first `bits` bits of two 16-byte addresses are equal.
fn bits_prefix_eq(a: &[u8; 16], b: &[u8; 16], bits: u8) -> bool {
    let full = (bits / 8) as usize;
    if a[..full] != b[..full] {
        return false;
    }
    let rem = bits % 8;
    if rem == 0 {
        return true;
    }
    let mask = 0xffu8 << (8 - rem);
    (a[full] & mask) == (b[full] & mask)
}

/// Whether network `sup` contains address `sub` (same family, `sup` is the
/// same size or larger, and they agree on `sup`'s masked prefix).
fn net_contains(sup: &NetAddr, sub: &NetAddr) -> bool {
    sup.family() == sub.family()
        && sup.bits() <= sub.bits()
        && bits_prefix_eq(sup.addr(), sub.addr(), sup.bits())
}

/// The network containment/overlap predicates (`<<`, `>>`, `<<=`, `>>=`, `&&`).
fn network_relop(operator: BinaryOp, l: &NetAddr, r: &NetAddr) -> bool {
    use BinaryOp::*;
    match operator {
        Shl => net_contains(r, l) && r.bits() < l.bits(),
        Shr => net_contains(l, r) && l.bits() < r.bits(),
        NetContainedEq => net_contains(r, l),
        NetContainsEq => net_contains(l, r),
        Overlaps => net_contains(l, r) || net_contains(r, l),
        _ => unreachable!("non-network operator routed to network_relop"),
    }
}

/// `inet & inet` / `inet | inet`: bytewise over the address, result mask the
/// larger of the two. Different families are an error.
fn network_bitwise(operator: BinaryOp, l: &NetAddr, r: &NetAddr) -> Result<NetAddr, SqlError> {
    if l.family() != r.family() {
        return Err(sql_err!(
            sqlstate::UNDEFINED_FUNCTION,
            "cannot AND/OR inet values of different sizes"
        ));
    }
    let mut addr = [0; 16];
    for ((out, left), right) in addr.iter_mut().zip(l.addr()).zip(r.addr()) {
        *out = match operator {
            BinaryOp::BitAnd => *left & *right,
            BinaryOp::BitOr => *left | *right,
            _ => unreachable!("non-bitwise operator routed to network_bitwise"),
        };
    }
    NetAddr::new(l.family(), l.bits().max(r.bits()), addr).ok_or_else(|| {
        sql_err!(
            sqlstate::INTERNAL_ERROR,
            "network operator produced invalid address"
        )
    })
}

/// `~inet`: invert every address bit (over the family's byte width).
fn network_not(n: &NetAddr) -> NetAddr {
    let mut addr = *n.addr();
    for byte in addr[..n.addr_len()].iter_mut() {
        *byte = !*byte;
    }
    NetAddr::new(n.family(), n.bits(), addr).expect("network complement preserves invariants")
}

/// Adds a signed delta to an address (big-endian over the family width),
/// preserving the mask. `None` on overflow past the family's range.
fn addr_offset(n: &NetAddr, delta: i64) -> Option<NetAddr> {
    let len = n.addr_len();
    let mut addr = *n.addr();
    let magnitude = delta.unsigned_abs();
    if delta >= 0 {
        let mut carry = magnitude;
        for byte in addr[..len].iter_mut().rev() {
            let sum = u64::from(*byte) + (carry & 0xff);
            *byte = sum as u8;
            carry = (carry >> 8) + (sum >> 8);
        }
        if carry != 0 {
            return None;
        }
    } else {
        let mut borrow = magnitude;
        for byte in addr[..len].iter_mut().rev() {
            let sub = i64::from(*byte) - (borrow & 0xff) as i64;
            if sub < 0 {
                *byte = (sub + 256) as u8;
                borrow = (borrow >> 8) + 1;
            } else {
                *byte = sub as u8;
                borrow >>= 8;
            }
        }
        if borrow != 0 {
            return None;
        }
    }
    NetAddr::new(n.family(), n.bits(), addr)
}

/// `inet - inet`: the signed distance between two addresses as int8. Both must
/// be the same family; a v6 distance beyond int8 overflows loudly.
fn network_distance(l: &NetAddr, r: &NetAddr) -> Result<i64, SqlError> {
    if l.family() != r.family() {
        return Err(sql_err!(
            sqlstate::UNDEFINED_FUNCTION,
            "cannot subtract inet values of different sizes"
        ));
    }
    let len = l.addr_len();
    let mut result: i64 = 0;
    for i in 0..len {
        let diff = i64::from(l.addr()[i]) - i64::from(r.addr()[i]);
        result = result
            .checked_mul(256)
            .and_then(|v| v.checked_add(diff))
            .ok_or_else(|| sql_err!(sqlstate::NUMERIC_OUT_OF_RANGE, "result is out of range"))?;
    }
    Ok(result)
}

/// Dispatches a binary network operator, given at least one network operand.
fn network_op<'a>(operator: BinaryOp, l: Datum<'a>, r: Datum<'a>) -> Result<Datum<'a>, SqlError> {
    use BinaryOp::*;
    if l.is_null() || r.is_null() {
        return Ok(Datum::Null);
    }
    let net = |d: &Datum| match d {
        Datum::Inet(n) | Datum::Cidr(n) => Some(*n),
        _ => None,
    };
    match operator {
        Shl | Shr | NetContainedEq | NetContainsEq | Overlaps => {
            let (Some(a), Some(b)) = (net(&l), net(&r)) else {
                return Err(net_operand_error(operator, &l, &r));
            };
            Ok(Datum::Bool(network_relop(operator, &a, &b)))
        }
        BitAnd | BitOr => {
            let (Some(a), Some(b)) = (net(&l), net(&r)) else {
                return Err(net_operand_error(operator, &l, &r));
            };
            Ok(Datum::Inet(network_bitwise(operator, &a, &b)?))
        }
        Add | Sub => {
            match (net(&l), net(&r)) {
                // inet - inet -> int8 distance.
                (Some(a), Some(b)) if operator == Sub => Ok(Datum::Int8(network_distance(&a, &b)?)),
                // inet ± integer.
                (Some(a), None) => {
                    let delta = as_i64(&r).ok_or_else(|| net_operand_error(operator, &l, &r))?;
                    let signed = if operator == Sub { -delta } else { delta };
                    addr_offset(&a, signed).map(Datum::Inet).ok_or_else(|| {
                        sql_err!(sqlstate::NUMERIC_OUT_OF_RANGE, "result is out of range")
                    })
                }
                // integer + inet.
                (None, Some(b)) if operator == Add => {
                    let delta = as_i64(&l).ok_or_else(|| net_operand_error(operator, &l, &r))?;
                    addr_offset(&b, delta).map(Datum::Inet).ok_or_else(|| {
                        sql_err!(sqlstate::NUMERIC_OUT_OF_RANGE, "result is out of range")
                    })
                }
                _ => Err(net_operand_error(operator, &l, &r)),
            }
        }
        _ => Err(net_operand_error(operator, &l, &r)),
    }
}

/// The "operator does not exist" error for an unsupported network operand pair.
fn net_operand_error(operator: BinaryOp, l: &Datum, r: &Datum) -> SqlError {
    sql_err!(
        sqlstate::UNDEFINED_FUNCTION,
        "operator does not exist: {} {:?} {}",
        type_name_of(l),
        operator,
        type_name_of(r)
    )
}

/// Whether a datum is an `inet` or `cidr`.
fn is_network(d: &Datum) -> bool {
    matches!(d, Datum::Inet(_) | Datum::Cidr(_))
}

pub(crate) fn binary<'a>(
    operator: BinaryOp,
    l: Datum<'a>,
    r: Datum<'a>,
    l_unknown: bool,
    r_unknown: bool,
    arena: &'a Arena,
) -> Result<Datum<'a>, SqlError> {
    use BinaryOp::*;
    match operator {
        And | Or => logic(operator, l, r),
        Concat => match (l, r) {
            (Datum::TsVector(left), Datum::TsVector(right)) => {
                crate::sql::full_text::concat_vectors(left.as_str(), right.as_str(), arena)
                    .map(crate::sql::full_text::restore_vector)
                    .map(Datum::TsVector)
            }
            (Datum::TsQuery(left), Datum::TsQuery(right)) => {
                crate::sql::full_text::concat_queries(left.as_str(), right.as_str(), arena)
                    .map(crate::sql::full_text::restore_query)
                    .map(Datum::TsQuery)
            }
            (Datum::Bit { .. }, _) | (_, Datum::Bit { .. }) => bit_concat(l, r, arena),
            // jsonb || jsonb: object merge (right wins), array concat, else
            // wrap-and-concat. (Plain json has no `||` operator in PostgreSQL,
            // but our `Datum::Json` carries a `jsonb` flag; concat applies only
            // to the jsonb form.)
            (Datum::Json { jsonb: true, .. }, _) | (_, Datum::Json { jsonb: true, .. }) => {
                jsonb_concat(l, r, arena)
            }
            _ => concat(l, r, l_unknown, r_unknown, arena),
        },
        TextSearchMatch => {
            if l.is_null() || r.is_null() {
                return Ok(Datum::Null);
            }
            let (vector, query) = match (l, r) {
                (Datum::TsVector(vector), Datum::TsQuery(query))
                | (Datum::TsQuery(query), Datum::TsVector(vector)) => {
                    (vector.as_str(), query.as_str())
                }
                (Datum::Text(document), Datum::TsQuery(query))
                | (Datum::TsQuery(query), Datum::Text(document)) => {
                    let vector = crate::sql::full_text::to_tsvector(
                        crate::sql::full_text::TextSearchConfig::English,
                        document,
                        arena,
                    )?;
                    (vector, query.as_str())
                }
                (left, right) => {
                    return Err(sql_err!(
                        sqlstate::UNDEFINED_FUNCTION,
                        "operator does not exist: {} @@ {}",
                        type_name_of(&left),
                        type_name_of(&right)
                    ));
                }
            };
            crate::sql::full_text::matches(vector, query, arena).map(Datum::Bool)
        }
        TextSearchPhrase => match (l, r) {
            (Datum::Null, _) | (_, Datum::Null) => Ok(Datum::Null),
            (Datum::TsQuery(left), Datum::TsQuery(right)) => {
                crate::sql::full_text::phrase_queries(left.as_str(), right.as_str(), arena)
                    .map(crate::sql::full_text::restore_query)
                    .map(Datum::TsQuery)
            }
            (left, right) => Err(sql_err!(
                sqlstate::UNDEFINED_FUNCTION,
                "operator does not exist: {} <-> {}",
                type_name_of(&left),
                type_name_of(&right)
            )),
        },
        Eq | NotEq | Lt | LtEq | Gt | GtEq
            if matches!(l, Datum::Array { .. }) || matches!(r, Datum::Array { .. }) =>
        {
            let l = coerce_unknown_array(l, &r, arena)?;
            let r = coerce_unknown_array(r, &l, arena)?;
            compare(operator, l, r, false, false)
        }
        Eq | NotEq | Lt | LtEq | Gt | GtEq => match (l, r) {
            (Datum::Range { .. }, _) | (_, Datum::Range { .. }) => compare_ranges(operator, l, r),
            (Datum::Bit { .. }, _) | (_, Datum::Bit { .. }) => compare_bits(operator, l, r),
            // Row comparison uses PostgreSQL's three-valued, short-circuiting
            // semantics (NULL-propagating), distinct from the total order that
            // `compare_datums` gives ORDER BY / DISTINCT.
            (
                Datum::Record(a) | Datum::Composite { fields: a, .. },
                Datum::Record(b) | Datum::Composite { fields: b, .. },
            ) => row_compare(operator, a, b),
            _ => compare(operator, l, r, l_unknown, r_unknown),
        },
        // `jsonb - text`/`text[]`/`integer` deletes a key, keys, or an element.
        Sub if matches!(l, Datum::Json { jsonb: true, .. }) => jsonb_delete(l, r, arena),
        Add | Sub if is_network(&l) || is_network(&r) => network_op(operator, l, r),
        Add | Sub | Mul | Div | Mod => match (l, r) {
            (Datum::Multirange { .. }, _) | (_, Datum::Multirange { .. }) => {
                multirange_setop(operator, l, r, arena)
            }
            (Datum::Range { .. }, _) | (_, Datum::Range { .. }) => {
                range_setop(operator, l, r, arena)
            }
            _ => arithmetic(operator, l, r, l_unknown, r_unknown, arena),
        },
        JsonGet | JsonGetText => json_get(l, r, operator == JsonGetText, arena),
        JsonPath | JsonPathText => json_path(l, r, operator == JsonPathText, arena),
        JsonDeletePath => jsonb_delete_path(l, r, arena),
        JsonExists | JsonExistsAny | JsonExistsAll => json_exists(operator, l, r, arena),
        Shl | Shr if is_network(&l) || is_network(&r) => network_op(operator, l, r),
        Shl | Shr => match (l, r) {
            (Datum::Range { .. }, _) | (_, Datum::Range { .. }) => range_op(operator, l, r, arena),
            (Datum::Bit { .. }, _) => bit_shift(operator, l, r, arena),
            _ => bitwise(operator, l, r),
        },
        NetContainedEq | NetContainsEq => network_op(operator, l, r),
        BitAnd | BitOr if is_network(&l) || is_network(&r) => network_op(operator, l, r),
        BitAnd | BitOr | BitXor => match (l, r) {
            (Datum::Bit { .. }, _) | (_, Datum::Bit { .. }) => bit_bitwise(operator, l, r, arena),
            _ => bitwise(operator, l, r),
        },
        // `inet && inet` overlap.
        Overlaps if is_network(&l) || is_network(&r) => network_op(operator, l, r),
        // Array containment/overlap: `@>` `<@` `&&` over two arrays.
        Contains | ContainedBy | Overlaps
            if matches!(l, Datum::Array { .. }) || matches!(r, Datum::Array { .. }) =>
        {
            let l = coerce_unknown_array(l, &r, arena)?;
            let r = coerce_unknown_array(r, &l, arena)?;
            array_set_op(operator, l, r)
        }
        Contains | ContainedBy | Overlaps | NotLeftOf | NotRightOf | Adjacent
            if matches!(l, Datum::Multirange { .. }) || matches!(r, Datum::Multirange { .. }) =>
        {
            multirange_op(operator, l, r, arena)
        }
        // `jsonb @> jsonb` / `jsonb <@ jsonb` deep containment.
        Contains | ContainedBy
            if matches!(l, Datum::Json { .. }) || matches!(r, Datum::Json { .. }) =>
        {
            jsonb_contains(operator, l, r, l_unknown, r_unknown, arena)
        }
        Overlaps if matches!(l, Datum::TsQuery(_)) || matches!(r, Datum::TsQuery(_)) => {
            match (l, r) {
                (Datum::Null, _) | (_, Datum::Null) => Ok(Datum::Null),
                (Datum::TsQuery(left), Datum::TsQuery(right)) => {
                    crate::sql::full_text::and_queries(left.as_str(), right.as_str(), arena)
                        .map(crate::sql::full_text::restore_query)
                        .map(Datum::TsQuery)
                }
                _ => unreachable!("tsquery overlap guard"),
            }
        }
        Contains | ContainedBy
            if matches!(l, Datum::TsQuery(_)) || matches!(r, Datum::TsQuery(_)) =>
        {
            match (l, r) {
                (Datum::Null, _) | (_, Datum::Null) => Ok(Datum::Null),
                (Datum::TsQuery(left), Datum::TsQuery(right)) => {
                    let contains = if operator == Contains {
                        crate::sql::full_text::query_contains(left.as_str(), right.as_str(), arena)?
                    } else {
                        crate::sql::full_text::query_contains(right.as_str(), left.as_str(), arena)?
                    };
                    Ok(Datum::Bool(contains))
                }
                _ => unreachable!("tsquery containment guard"),
            }
        }
        Contains | ContainedBy | Overlaps | NotLeftOf | NotRightOf | Adjacent => {
            range_op(operator, l, r, arena)
        }
        // Only reached as the per-element operator of a quantified `LIKE ANY/ALL`.
        Like | ILike => {
            if l.is_null() || r.is_null() {
                return Ok(Datum::Null);
            }
            let text = cast_to_text(l, arena)?;
            let pattern = cast_to_text(r, arena)?;
            Ok(Datum::Bool(like_match(
                text,
                pattern,
                operator == ILike,
                Some('\\'),
            )))
        }
        Pow => {
            // PostgreSQL `^` stays numeric when an operand is numeric (and none
            // is float8); otherwise it is double-precision exponentiation.
            if l.is_null() || r.is_null() {
                return Ok(Datum::Null);
            }
            let any_numeric = matches!(l, Datum::Numeric(_)) || matches!(r, Datum::Numeric(_));
            let any_float = matches!(l, Datum::Float8(_)) || matches!(r, Datum::Float8(_));
            if any_numeric && !any_float {
                let a = datum_numeric("^", l, arena)?;
                let b = datum_numeric("^", r, arena)?;
                return Ok(Datum::Numeric(numeric::pow(&a, &b, arena)?));
            }
            let (a, b) = (datum_f64("^", l)?, datum_f64("^", r)?);
            Ok(Datum::Float8(a.powf(b)))
        }
    }
}

/// SQL three-valued AND/OR.
pub(crate) fn logic<'a>(
    operator: BinaryOp,
    l: Datum<'a>,
    r: Datum<'a>,
) -> Result<Datum<'a>, SqlError> {
    let as_bool = |d: &Datum| -> Result<Option<bool>, SqlError> {
        match d {
            Datum::Null => Ok(None),
            Datum::Bool(b) => Ok(Some(*b)),
            other => match super::boolean_argument(*other, "AND/OR")? {
                Datum::Bool(b) => Ok(Some(b)),
                _ => Ok(None),
            },
        }
    };
    let (a, b) = (as_bool(&l)?, as_bool(&r)?);
    let out = match (operator, a, b) {
        (BinaryOp::And, Some(false), _) | (BinaryOp::And, _, Some(false)) => Some(false),
        (BinaryOp::And, Some(true), Some(true)) => Some(true),
        (BinaryOp::And, _, _) => None,
        (BinaryOp::Or, Some(true), _) | (BinaryOp::Or, _, Some(true)) => Some(true),
        (BinaryOp::Or, Some(false), Some(false)) => Some(false),
        (BinaryOp::Or, _, _) => None,
        _ => unreachable!(),
    };
    Ok(out.map_or(Datum::Null, Datum::Bool))
}

#[cfg(test)]
mod hash_tests {
    use super::*;
    use crate::sql::types::Datum;

    fn h(d: Datum) -> u64 {
        hash_key(&[d], &[0])
    }

    #[test]
    fn hash_agrees_with_equality() {
        // -0.0 and 0.0 are equal and must hash equal (float canonicalization).
        assert_eq!(h(Datum::Float8(0.0)), h(Datum::Float8(-0.0)));
        assert_eq!(h(Datum::Float4(0.0)), h(Datum::Float4(-0.0)));
        // Integer widths of the same value hash equal.
        assert_eq!(h(Datum::Int2(5)), h(Datum::Int4(5)));
        assert_eq!(h(Datum::Int4(5)), h(Datum::Int8(5)));
        // A blank-padded char equals its stripped form and the same text.
        assert_eq!(h(Datum::Bpchar("a")), h(Datum::Bpchar("a   ")));
        assert_eq!(h(Datum::Text("a")), h(Datum::Bpchar("a  ")));
        assert_eq!(
            h(Datum::TsQuery(crate::sql::full_text::restore_query("'a'"))),
            h(Datum::TsQuery(crate::sql::full_text::restore_query(
                "'a':*AB"
            ))),
        );
        // Distinct values differ (not required for correctness, but the point
        // of the index is that the common types separate).
        assert_ne!(h(Datum::Int4(5)), h(Datum::Int4(6)));
        assert_ne!(h(Datum::Text("a")), h(Datum::Text("b")));
        // The load-bearing invariant, cross-checked against compare_datums:
        // equal values hash equal, so the index never misses a true collision.
        for (a, b) in [
            (Datum::Float8(0.0), Datum::Float8(-0.0)),
            (Datum::Bpchar("a"), Datum::Bpchar("a  ")),
            (Datum::Text("a"), Datum::Bpchar("a ")),
            (
                Datum::TsQuery(crate::sql::full_text::restore_query("'a'")),
                Datum::TsQuery(crate::sql::full_text::restore_query("'a':*AB")),
            ),
        ] {
            assert!(compare_datums(&a, &b).unwrap().is_eq(), "{a:?} == {b:?}");
            assert_eq!(h(a), h(b));
        }
    }

    #[test]
    fn locale_capable_collation_hash_never_excludes_an_equal_text_pair() {
        let left = [Datum::Text("a")];
        let right = [Datum::Text("A")];
        for collation in [Collation::Default, Collation::Catalog(0)] {
            assert_eq!(
                hash_key_collated(&left, &[0], &[collation]),
                hash_key_collated(&right, &[0], &[collation]),
            );
        }
    }
}
