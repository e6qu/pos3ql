//! PostgreSQL write-ahead-log position scalar semantics.

use crate::mem::arena::Arena;
use crate::sql::eval::{SqlError, sqlstate};
use crate::sql::numeric::Numeric;
use crate::sql_err;

pub fn parse(value: &str) -> Result<u64, SqlError> {
    let bad = || {
        sql_err!(
            sqlstate::INVALID_TEXT_REPRESENTATION,
            "invalid input syntax for type pg_lsn: \"{}\"",
            value
        )
    };
    let (high, low) = value.split_once('/').ok_or_else(bad)?;
    if high.is_empty()
        || low.is_empty()
        || high.len() > 8
        || low.len() > 8
        || !high.bytes().all(|byte| byte.is_ascii_hexdigit())
        || !low.bytes().all(|byte| byte.is_ascii_hexdigit())
    {
        return Err(bad());
    }
    let high = u32::from_str_radix(high, 16).map_err(|_| bad())?;
    let low = u32::from_str_radix(low, 16).map_err(|_| bad())?;
    Ok((u64::from(high) << 32) | u64::from(low))
}

fn out_of_range() -> SqlError {
    sql_err!(sqlstate::INVALID_PARAMETER_VALUE, "pg_lsn out of range")
}

pub fn from_numeric(value: &Numeric<'_>) -> Result<u64, SqlError> {
    let value = value.to_i128().map_err(|_| out_of_range())?;
    u64::try_from(value).map_err(|_| out_of_range())
}

pub fn shift(
    value: u64,
    offset: &Numeric<'_>,
    subtract: bool,
    arena: &Arena,
) -> Result<u64, SqlError> {
    if offset.is_nan() {
        return Err(sql_err!(
            sqlstate::FEATURE_NOT_SUPPORTED,
            "cannot {} NaN {} pg_lsn",
            if subtract { "subtract" } else { "add" },
            if subtract { "from" } else { "to" }
        ));
    }
    // PostgreSQL performs numeric arithmetic first and only then rounds the
    // complete result to pg_lsn. Rounding the offset independently gets half
    // values wrong for subtraction (16 - 1.5 is 15, not 14).
    let base = Numeric::from_i128(i128::from(value), arena).map_err(|_| out_of_range())?;
    let result = if subtract {
        crate::sql::numeric::sub(&base, offset, arena)
    } else {
        crate::sql::numeric::add(&base, offset, arena)
    }
    .map_err(|_| out_of_range())?;
    from_numeric(&result)
}

pub fn difference<'a>(left: u64, right: u64, arena: &'a Arena) -> Result<Numeric<'a>, SqlError> {
    Numeric::from_i128(i128::from(left) - i128::from(right), arena)
}

pub fn hash(value: u64) -> i32 {
    crate::sql::identity::hash_uint32(crate::sql::identity::hash_int64_input(value as i64)) as i32
}

pub fn hash_extended(value: u64, seed: i64) -> i64 {
    crate::sql::identity::hash_uint32_extended(
        crate::sql::identity::hash_int64_input(value as i64),
        seed,
    ) as i64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_and_hash_match_postgresql_18() {
        assert_eq!(parse("0/0").unwrap(), 0);
        assert_eq!(parse("FFFFFFFF/FFFFFFFF").unwrap(), u64::MAX);
        assert!(parse("100000000/0").is_err());
        assert_eq!(hash(0), -272_711_505);
        assert_eq!(hash(1), -1_905_060_026);
        assert_eq!(hash(1u64 << 32), -1_905_060_026);
        assert_eq!(hash(u64::MAX), 385_747_274);
        assert_eq!(hash_extended(u64::MAX, 123), 2_652_575_173_807_735_193);
    }
}
