//! How a foreign key compares a referencing column with its referenced column.
//!
//! PostgreSQL 18.6 picks the operator in `ATAddForeignKeyConstraint`
//! ([tablecmds.c](https://github.com/postgres/postgres/blob/REL_18_6/src/backend/commands/tablecmds.c#L10330-L10460)):
//! the equality operator of the referenced unique index's btree operator
//! family taking the referencing type, when the family also compares two
//! referencing values; otherwise, when both types implicitly coerce to the
//! operator class input type, that type's own equality after the coercion.
//! Every check and action compares through the same choice.

use crate::mem::arena::Arena;
use crate::sql::eval::{SqlError, sqlstate};
use crate::sql::types::{ColType, Datum};
use crate::sql_err;

/// The comparison of one referencing column with its referenced column
/// (PostgreSQL's `conpfeqop` entry).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum KeyEquality {
    /// The referenced operator family compares the referencing type
    /// directly: the same type or a cross-type member of the family.
    Direct,
    /// The referencing value is implicitly cast to the operator class input
    /// type, which then compares with the referenced value.
    Cast(ColType),
}

/// The input type of a type's default btree operator class, or `None` for
/// the polymorphic classes (arrays, enums, ranges, multiranges and records),
/// which accept only the referenced type itself here. PostgreSQL's `record`
/// class also accepts a different composite type; that pair is rejected
/// with `0A000` (see [`dissimilar_composites`]).
pub(crate) fn operator_class_input(referenced: ColType) -> Option<ColType> {
    match referenced {
        ColType::Varchar => Some(ColType::Text),
        ColType::Cidr => Some(ColType::Inet),
        ColType::Regtype
        | ColType::Regproc
        | ColType::Regprocedure
        | ColType::Regoper
        | ColType::Regoperator
        | ColType::Regclass
        | ColType::Regnamespace
        | ColType::Regrole
        | ColType::Regconfig
        | ColType::Regdictionary
        | ColType::Regcollation => Some(ColType::Oid),
        ColType::Array(_)
        | ColType::Enum(_)
        | ColType::Range(_)
        | ColType::Multirange(_)
        | ColType::Composite(_) => None,
        other => Some(other),
    }
}

/// The built-in btree families with cross-type equality members
/// (`integer_ops`, `float_ops`, `datetime_ops`, `text_ops`): any two of the
/// family's types compare directly.
fn cross_type_family(value: ColType) -> Option<u8> {
    match value {
        ColType::Int2 | ColType::Int4 | ColType::Int8 => Some(0),
        ColType::Float4 | ColType::Float8 => Some(1),
        ColType::Date | ColType::Timestamp | ColType::Timestamptz => Some(2),
        ColType::Text | ColType::Name => Some(3),
        _ => None,
    }
}

/// The comparison PostgreSQL uses for a referencing column of base type
/// `referencing` and a referenced column of base type `referenced`, or
/// `None` when the key cannot be implemented with built-in operators.
pub(crate) fn key_equality(referencing: ColType, referenced: ColType) -> Option<KeyEquality> {
    if referencing == referenced {
        return Some(KeyEquality::Direct);
    }
    let input = operator_class_input(referenced)?;
    if referencing == input
        || cross_type_family(referencing)
            .is_some_and(|family| cross_type_family(input) == Some(family))
    {
        return Some(KeyEquality::Direct);
    }
    let (Some(from), Some(to)) = (referencing.try_oid(), input.try_oid()) else {
        return None;
    };
    crate::sql::catalog::builtin_cast_context(from, to)
        .is_some_and(crate::storage::CastContext::permits_implicit)
        .then_some(KeyEquality::Cast(input))
}

/// Two different composite types, which PostgreSQL compares field by field
/// and pos3ql does not implement as a key.
pub(crate) fn dissimilar_composites(referencing: ColType, referenced: ColType) -> bool {
    matches!(
        (referencing, referenced),
        (ColType::Composite(left), ColType::Composite(right)) if left != right
    )
}

/// The resolved comparison of a stored key; DDL rejected every other pair.
pub(crate) fn stored_key_equality(
    referencing: ColType,
    referenced: ColType,
) -> Result<KeyEquality, SqlError> {
    key_equality(referencing, referenced).ok_or_else(|| {
        sql_err!(
            sqlstate::INTERNAL_ERROR,
            "foreign key column types {} and {} have no equality operator",
            referencing.name(),
            referenced.name()
        )
    })
}

/// The value a referencing column contributes to the comparison.
pub(crate) fn comparison_value<'a>(
    equality: KeyEquality,
    value: Datum<'a>,
    arena: &'a Arena,
) -> Result<Datum<'a>, SqlError> {
    match equality {
        KeyEquality::Cast(target) if !value.is_null() => {
            match (crate::sql::eval::cast_to(value, target, arena)?, target) {
                // `cast_to` yields the text of a blank-padded value; the
                // comparison needs `bpchar` semantics (trailing blanks ignored).
                (Datum::Text(text), ColType::Bpchar) => Ok(Datum::Bpchar(text)),
                (cast, _) => Ok(cast),
            }
        }
        _ => Ok(value),
    }
}

/// A value of the referenced column's own type equal to the comparison value
/// `key`, so the referenced unique key cache can be probed: `Some(None)` when
/// no referenced value can equal it, `None` when only a scan can decide.
pub(crate) fn referenced_probe_value<'a>(
    key: Datum<'a>,
    referenced: ColType,
) -> Option<Option<Datum<'a>>> {
    let integer = match key {
        Datum::Int2(value) => Some(i64::from(value)),
        Datum::Int4(value) => Some(i64::from(value)),
        Datum::Int8(value) => Some(value),
        _ => None,
    };
    match (key, referenced) {
        (_, ColType::Int2) if let Some(value) = integer => {
            Some(i16::try_from(value).ok().map(Datum::Int2))
        }
        (_, ColType::Int4) if let Some(value) = integer => {
            Some(i32::try_from(value).ok().map(Datum::Int4))
        }
        (_, ColType::Int8) if let Some(value) = integer => Some(Some(Datum::Int8(value))),
        (Datum::Float4(value), ColType::Float4) => Some(Some(Datum::Float4(value))),
        (Datum::Float4(value), ColType::Float8) => Some(Some(Datum::Float8(f64::from(value)))),
        (Datum::Float8(value), ColType::Float8) => Some(Some(Datum::Float8(value))),
        (Datum::Float8(value), ColType::Float4) => {
            let narrowed = value as f32;
            Some(
                (f64::from(narrowed) == value || value.is_nan()).then_some(Datum::Float4(narrowed)),
            )
        }
        _ => None,
    }
}

/// PostgreSQL 18.6's btree equality operators: (operator, left type, right
/// type), from `pg_operator` joined to `pg_amop` (`amopmethod = 403`) on a
/// fresh `initdb`.
const BTREE_EQUALITY_OPERATORS: [(i32, i32, i32); 53] = [
    (91, 16, 16),
    (1955, 17, 17),
    (92, 18, 18),
    (93, 19, 19),
    (254, 19, 25),
    (410, 20, 20),
    (1868, 20, 21),
    (416, 20, 23),
    (1862, 21, 20),
    (94, 21, 21),
    (532, 21, 23),
    (15, 23, 20),
    (533, 23, 21),
    (96, 23, 23),
    (260, 25, 19),
    (98, 25, 25),
    (607, 26, 26),
    (387, 27, 27),
    (649, 30, 30),
    (620, 700, 700),
    (1120, 700, 701),
    (1130, 701, 700),
    (670, 701, 701),
    (3362, 774, 774),
    (900, 790, 790),
    (1220, 829, 829),
    (1201, 869, 869),
    (1054, 1042, 1042),
    (1093, 1082, 1082),
    (2347, 1082, 1114),
    (2360, 1082, 1184),
    (1108, 1083, 1083),
    (2373, 1114, 1082),
    (2060, 1114, 1114),
    (2536, 1114, 1184),
    (2386, 1184, 1082),
    (2542, 1184, 1114),
    (1320, 1184, 1184),
    (1330, 1186, 1186),
    (1550, 1266, 1266),
    (1784, 1560, 1560),
    (1804, 1562, 1562),
    (1752, 1700, 1700),
    (2988, 2249, 2249),
    (1070, 2277, 2277),
    (2972, 2950, 2950),
    (3222, 3220, 3220),
    (3516, 3500, 3500),
    (3629, 3614, 3614),
    (3676, 3615, 3615),
    (3240, 3802, 3802),
    (3882, 3831, 3831),
    (2860, 4537, 4537),
];

/// The type a btree operator class declares for `value`: polymorphic
/// classes declare `anyarray`, `anyenum`, `anyrange`, `anymultirange` or
/// `record`.
fn operator_input_oid(value: ColType) -> Option<i32> {
    match value {
        ColType::Array(_) => Some(2277),
        ColType::Enum(_) => Some(3500),
        ColType::Range(_) => Some(3831),
        ColType::Multirange(_) => Some(4537),
        ColType::Composite(_) => Some(2249),
        other => operator_class_input(other)?.try_oid(),
    }
}

fn btree_equality_operator(left: ColType, right: ColType) -> Option<i32> {
    let (left, right) = (operator_input_oid(left)?, operator_input_oid(right)?);
    BTREE_EQUALITY_OPERATORS
        .iter()
        .find_map(|&(operator, l, r)| (l == left && r == right).then_some(operator))
}

/// PostgreSQL's `conpfeqop`, `conppeqop` and `conffeqop` entries for one key
/// column: referenced = referencing, referenced = referenced and
/// referencing = referencing. Zero when the operator is not catalogued.
pub(crate) fn key_operators(referencing: ColType, referenced: ColType) -> [i32; 3] {
    let input = operator_class_input(referenced).unwrap_or(referenced);
    let referenced_equality = btree_equality_operator(input, input).unwrap_or(0);
    match key_equality(referencing, referenced) {
        Some(KeyEquality::Direct) => [
            btree_equality_operator(input, referencing).unwrap_or(0),
            referenced_equality,
            btree_equality_operator(referencing, referencing).unwrap_or(0),
        ],
        Some(KeyEquality::Cast(_)) | None => [referenced_equality; 3],
    }
}

#[cfg(all(test, not(all(miri, feature = "miri-ownership"))))]
mod tests {
    use super::*;

    #[test]
    fn key_equality_follows_the_referenced_operator_family() {
        use ColType::*;
        assert_eq!(key_equality(Int4, Int8), Some(KeyEquality::Direct));
        assert_eq!(key_equality(Int8, Int2), Some(KeyEquality::Direct));
        assert_eq!(key_equality(Float8, Float4), Some(KeyEquality::Direct));
        assert_eq!(key_equality(Date, Timestamptz), Some(KeyEquality::Direct));
        assert_eq!(key_equality(Name, Varchar), Some(KeyEquality::Direct));
        assert_eq!(key_equality(Int8, Float4), Some(KeyEquality::Cast(Float4)));
        assert_eq!(
            key_equality(Int4, Numeric),
            Some(KeyEquality::Cast(Numeric))
        );
        assert_eq!(
            key_equality(Varchar, Bpchar),
            Some(KeyEquality::Cast(Bpchar))
        );
        assert_eq!(key_equality(Bpchar, Text), Some(KeyEquality::Cast(Text)));
        assert_eq!(key_equality(Int8, Oid), Some(KeyEquality::Cast(Oid)));
        assert_eq!(key_equality(Numeric, Int4), None);
        assert_eq!(key_equality(Float8, Numeric), None);
        assert_eq!(key_equality(Oid, Int4), None);
        assert_eq!(key_equality(Name, Bpchar), None);
        assert_eq!(
            key_equality(
                Array(crate::sql::types::ArrElem::Int2),
                Array(crate::sql::types::ArrElem::Int4)
            ),
            None
        );
        assert_eq!(key_operators(Int4, Int8), [416, 410, 96]);
        assert_eq!(key_operators(Int2, Numeric), [1752; 3]);
    }

    #[test]
    fn probe_values_keep_only_exactly_equal_keys() {
        assert!(matches!(
            referenced_probe_value(Datum::Int8(5_000_000_000), ColType::Int4),
            Some(None)
        ));
        assert!(matches!(
            referenced_probe_value(Datum::Int8(7), ColType::Int2),
            Some(Some(Datum::Int2(7)))
        ));
        assert!(matches!(
            referenced_probe_value(Datum::Float8(0.1), ColType::Float4),
            Some(None)
        ));
        assert!(matches!(
            referenced_probe_value(Datum::Float8(0.5), ColType::Float4),
            Some(Some(Datum::Float4(0.5)))
        ));
        assert!(referenced_probe_value(Datum::Text("x"), ColType::Name).is_none());
    }
}
