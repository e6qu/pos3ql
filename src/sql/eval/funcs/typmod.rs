//! Type-modifier input functions (`numerictypmodin(cstring[])` and its
//! siblings). Each reads the modifier list a type name carries, as
//! PostgreSQL 18.6's `ArrayGetIntegerTypmods` (arrayutils.c) does, and
//! returns the encoded `atttypmod` or that type's error.

use crate::sql::ast::Expr;
use crate::sql::types::{ArrElem, Datum, INTERVAL_FULL_RANGE, IntervalRange};
use crate::sql_err;

use super::super::{
    ColumnLookup, EvalHooks, SqlError, eval_full, sqlstate, type_name_of_pub as type_name_of,
};

/// The largest modifier list a type accepts; a longer list is invalid for
/// every type here.
const MAX_TYPMODS: usize = 2;

/// `MaxAttrSize`: the largest declared length of a character type.
const MAX_ATTR_SIZE: i32 = 10 * 1024 * 1024;

/// The functions this module answers, with the type each one encodes.
fn typmod_kind(name: &str) -> Option<TypmodKind> {
    Some(match name {
        "numerictypmodin" => TypmodKind::Numeric,
        "bpchartypmodin" => TypmodKind::Character("char"),
        "varchartypmodin" => TypmodKind::Character("varchar"),
        "bittypmodin" => TypmodKind::Bit("bit"),
        "varbittypmodin" => TypmodKind::Bit("varbit"),
        "timetypmodin" => TypmodKind::Temporal("TIME", ""),
        "timetztypmodin" => TypmodKind::Temporal("TIME", " WITH TIME ZONE"),
        "timestamptypmodin" => TypmodKind::Temporal("TIMESTAMP", ""),
        "timestamptztypmodin" => TypmodKind::Temporal("TIMESTAMP", " WITH TIME ZONE"),
        "intervaltypmodin" => TypmodKind::Interval,
        _ => return None,
    })
}

#[derive(Clone, Copy)]
enum TypmodKind {
    Numeric,
    Character(&'static str),
    Bit(&'static str),
    Temporal(&'static str, &'static str),
    Interval,
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn dispatch<'a>(
    name: &str,
    args: &[&Expr<'a>],
    star: bool,
    arena: &'a crate::mem::arena::Arena,
    params: &[Datum<'a>],
    row: &impl ColumnLookup<'a>,
    hooks: &EvalHooks<'_, 'a>,
) -> Option<Result<Datum<'a>, SqlError>> {
    let kind = typmod_kind(name)?;
    Some((|| {
        if star || args.len() != 1 {
            return Err(sql_err!(
                sqlstate::UNDEFINED_FUNCTION,
                "function {}(...) with {} arguments does not exist",
                name,
                if star { 1 } else { args.len() }
            ));
        }
        let raw = match eval_full(args[0], arena, params, row, hooks)? {
            Datum::Null => return Ok(Datum::Null),
            Datum::Array {
                element: ArrElem::Cstring,
                raw,
            } => raw,
            Datum::Text(text) if matches!(args[0], Expr::Str(_) | Expr::Param(_)) => {
                crate::sql::array::parse_literal(text, ArrElem::Cstring, arena)?
            }
            other => {
                return Err(sql_err!(
                    sqlstate::UNDEFINED_FUNCTION,
                    "function {}({}) does not exist",
                    name,
                    type_name_of(&other)
                ));
            }
        };
        let mut typmods = [0i32; MAX_TYPMODS];
        let count = integer_typmods(raw, &mut typmods)?;
        encode(kind, &typmods[..count.min(MAX_TYPMODS)], count).map(Datum::Int4)
    })())
}

/// Reads a one-dimensional, NULL-free modifier list into `out`, returning its
/// length; entries past `out` are validated but not kept.
fn integer_typmods(raw: &[u8], out: &mut [i32; MAX_TYPMODS]) -> Result<usize, SqlError> {
    let shape = crate::sql::array::shape(raw).ok_or_else(|| {
        sql_err!(
            sqlstate::INTERNAL_ERROR,
            "typmod array has an invalid encoding"
        )
    })?;
    if shape.dimension_count() != 1 {
        return Err(sql_err!(
            sqlstate::ARRAY_SUBSCRIPT_ERROR,
            "typmod array must be one-dimensional"
        ));
    }
    let mut count = 0;
    for element in crate::sql::array::elements(raw, ArrElem::Cstring) {
        if element.is_null() {
            return Err(sql_err!(
                sqlstate::NULL_VALUE_NOT_ALLOWED,
                "typmod array must not contain nulls"
            ));
        }
    }
    for element in crate::sql::array::elements(raw, ArrElem::Cstring) {
        let Datum::Text(text) = element else {
            return Err(sql_err!(
                sqlstate::INTERNAL_ERROR,
                "typmod array element is not a cstring"
            ));
        };
        let value = super::super::cast::parse_int_bounded(
            text,
            i64::from(i32::MIN),
            i64::from(i32::MAX),
            "integer",
        )? as i32;
        if let Some(slot) = out.get_mut(count) {
            *slot = value;
        }
        count += 1;
    }
    Ok(count)
}

fn invalid(message: &'static str) -> SqlError {
    sql_err!(sqlstate::INVALID_PARAMETER_VALUE, "{}", message)
}

/// PostgreSQL clamps an over-precise temporal modifier to 6 with a WARNING.
fn clamped_precision(type_name: &str, suffix: &str, precision: i32) -> Result<i32, SqlError> {
    crate::sql::eval::raise_eval_message(
        crate::sql::eval::EvalMessageLevel::Warning,
        sqlstate::INVALID_PARAMETER_VALUE,
        format_args!(
            "{}({}){} precision reduced to maximum allowed, {}",
            type_name, precision, suffix, MAX_TEMPORAL_PRECISION
        ),
    )?;
    Ok(MAX_TEMPORAL_PRECISION)
}

const MAX_TEMPORAL_PRECISION: i32 = 6;

/// The `atttypmod` that `typmods` (of `count` entries) encodes for `kind`.
fn encode(kind: TypmodKind, typmods: &[i32], count: usize) -> Result<i32, SqlError> {
    match kind {
        TypmodKind::Numeric => {
            let (precision, scale) = match (count, typmods) {
                (1, [precision]) => (*precision, 0),
                (2, [precision, scale]) => (*precision, *scale),
                _ => return Err(invalid("invalid NUMERIC type modifier")),
            };
            if !(1..=1000).contains(&precision) {
                return Err(sql_err!(
                    sqlstate::INVALID_PARAMETER_VALUE,
                    "NUMERIC precision {} must be between 1 and 1000",
                    precision
                ));
            }
            if !(-1000..=1000).contains(&scale) {
                return Err(sql_err!(
                    sqlstate::INVALID_PARAMETER_VALUE,
                    "NUMERIC scale {} must be between -1000 and 1000",
                    scale
                ));
            }
            Ok(((precision << 16) | (scale & 0x7ff)) + 4)
        }
        TypmodKind::Character(type_name) => {
            let [length] = typmods else {
                return Err(invalid("invalid type modifier"));
            };
            if count != 1 {
                return Err(invalid("invalid type modifier"));
            }
            if *length < 1 {
                return Err(sql_err!(
                    sqlstate::INVALID_PARAMETER_VALUE,
                    "length for type {} must be at least 1",
                    type_name
                ));
            }
            if *length > MAX_ATTR_SIZE {
                return Err(sql_err!(
                    sqlstate::INVALID_PARAMETER_VALUE,
                    "length for type {} cannot exceed {}",
                    type_name,
                    MAX_ATTR_SIZE
                ));
            }
            Ok(length + 4)
        }
        TypmodKind::Bit(type_name) => {
            let [length] = typmods else {
                return Err(invalid("invalid type modifier"));
            };
            if count != 1 {
                return Err(invalid("invalid type modifier"));
            }
            if *length < 1 {
                return Err(sql_err!(
                    sqlstate::INVALID_PARAMETER_VALUE,
                    "length for type {} must be at least 1",
                    type_name
                ));
            }
            if *length > MAX_ATTR_SIZE * 8 {
                return Err(sql_err!(
                    sqlstate::INVALID_PARAMETER_VALUE,
                    "length for type {} cannot exceed {}",
                    type_name,
                    MAX_ATTR_SIZE * 8
                ));
            }
            Ok(*length)
        }
        TypmodKind::Temporal(type_name, suffix) => {
            let [precision] = typmods else {
                return Err(invalid("invalid type modifier"));
            };
            if count != 1 {
                return Err(invalid("invalid type modifier"));
            }
            if *precision < 0 {
                return Err(sql_err!(
                    sqlstate::INVALID_PARAMETER_VALUE,
                    "{}({}){} precision must not be negative",
                    type_name,
                    precision,
                    suffix
                ));
            }
            if *precision > MAX_TEMPORAL_PRECISION {
                return clamped_precision(type_name, suffix, *precision);
            }
            Ok(*precision)
        }
        TypmodKind::Interval => {
            let Some(&range) = typmods.first() else {
                return Err(invalid("invalid INTERVAL type modifier"));
            };
            let valid_range = u16::try_from(range)
                .ok()
                .and_then(IntervalRange::from_mask)
                .is_some();
            if !valid_range {
                return Err(invalid("invalid INTERVAL type modifier"));
            }
            match (count, typmods) {
                (1, _) if range == i32::from(INTERVAL_FULL_RANGE) => Ok(-1),
                (1, _) => Ok((range << 16) | 0xFFFF),
                (2, [_, precision]) => {
                    if *precision < 0 {
                        return Err(sql_err!(
                            sqlstate::INVALID_PARAMETER_VALUE,
                            "INTERVAL({}) precision must not be negative",
                            precision
                        ));
                    }
                    let precision = if *precision > MAX_TEMPORAL_PRECISION {
                        clamped_precision("INTERVAL", "", *precision)?
                    } else {
                        *precision
                    };
                    Ok((range << 16) | precision)
                }
                _ => Err(invalid("invalid INTERVAL type modifier")),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{TypmodKind, encode};

    /// Encodings verified against PostgreSQL 18.6.
    #[test]
    fn typmods_encode_as_postgresql() {
        let encoded = |kind, typmods: &[i32]| encode(kind, typmods, typmods.len()).unwrap();
        assert_eq!(encoded(TypmodKind::Numeric, &[10, 2]), 655_366);
        assert_eq!(encoded(TypmodKind::Numeric, &[10, -2]), 657_410);
        assert_eq!(encoded(TypmodKind::Character("varchar"), &[5]), 9);
        assert_eq!(encoded(TypmodKind::Bit("bit"), &[4]), 4);
        assert_eq!(encoded(TypmodKind::Temporal("TIME", ""), &[3]), 3);
        assert_eq!(encoded(TypmodKind::Interval, &[32767]), -1);
        assert_eq!(encoded(TypmodKind::Interval, &[4]), 327_679);
        assert_eq!(encoded(TypmodKind::Interval, &[32767, 3]), 2_147_418_115);
        assert_eq!(
            encode(TypmodKind::Interval, &[3], 1)
                .unwrap_err()
                .sqlstate
                .as_str(),
            "22023"
        );
        assert_eq!(
            encode(TypmodKind::Numeric, &[1, 2], 3)
                .unwrap_err()
                .sqlstate
                .as_str(),
            "22023"
        );
        crate::sql::eval::clear_eval_messages();
        assert_eq!(encoded(TypmodKind::Temporal("TIME", ""), &[7]), 6);
        assert_eq!(encoded(TypmodKind::Interval, &[32767, 9]), 2_147_418_118);
        let first = crate::sql::eval::peek_eval_message().unwrap();
        assert_eq!(first.level, crate::sql::eval::EvalMessageLevel::Warning);
        assert_eq!(first.sqlstate.as_str(), "22023");
        assert_eq!(
            first.message.as_str(),
            "TIME(7) precision reduced to maximum allowed, 6"
        );
        crate::sql::eval::pop_eval_message();
        assert_eq!(
            crate::sql::eval::peek_eval_message()
                .unwrap()
                .message
                .as_str(),
            "INTERVAL(9) precision reduced to maximum allowed, 6"
        );
        crate::sql::eval::clear_eval_messages();
        assert!(crate::sql::eval::peek_eval_message().is_none());
    }
}
