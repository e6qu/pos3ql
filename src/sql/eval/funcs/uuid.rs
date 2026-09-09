//! PostgreSQL UUID generation and inspection functions. Algorithm and catalog
//! behavior follow PostgreSQL 18's `src/backend/utils/adt/uuid.c`:
//! <https://github.com/postgres/postgres/blob/REL_18_STABLE/src/backend/utils/adt/uuid.c>.

use core::cell::Cell;

use crate::sql::ast::Expr;
use crate::sql::types::{ColType, Datum, Interval};
use crate::{sql_err, stack_format};

use super::super::{
    ColumnLookup, EvalHooks, SqlError, cast_to, eval_full, expression_type_identity, sqlstate,
};

const NS_PER_SECOND: i64 = 1_000_000_000;
const NS_PER_MILLISECOND: i64 = 1_000_000;
const NS_PER_MICROSECOND: i64 = 1_000;
const US_PER_MILLISECOND: i64 = 1_000;
const PG_UNIX_EPOCH_OFFSET_US: i64 = crate::sql::datetime::PG_EPOCH_SECS * 1_000_000;
const UUID_V7_MIN_TIMESTAMP: i64 = -PG_UNIX_EPOCH_OFFSET_US;
const UUID_V7_MAX_TIMESTAMP: i64 =
    (((1_i64 << 48) - 1) * US_PER_MILLISECOND) - PG_UNIX_EPOCH_OFFSET_US;
const GREGORIAN_TO_PG_EPOCH_US: u64 = 152_384 * 86_400_000_000;

#[cfg(any(target_os = "macos", target_env = "msvc"))]
const SUBMS_MINIMAL_STEP_NS: i64 = NS_PER_MILLISECOND / (1 << 10) + 1;
#[cfg(not(any(target_os = "macos", target_env = "msvc")))]
const SUBMS_MINIMAL_STEP_NS: i64 = NS_PER_MILLISECOND / (1 << 12) + 1;

std::thread_local! {
    /// PostgreSQL advances UUIDv7's clock fraction by a minimum step in each
    /// backend. The executor is thread-confined, so this is the equivalent
    /// allocation-free state boundary here.
    static PREVIOUS_UUID_V7_NS: Cell<i64> = const { Cell::new(0) };
}

fn undefined_function(name: &str, argument: Option<ColType>) -> SqlError {
    let arguments = argument.map_or_else(crate::util::StackStr::<64>::new, |argument| {
        stack_format!(64, "{}", argument.name())
    });
    sql_err!(
        sqlstate::UNDEFINED_FUNCTION,
        "function {}({}) does not exist",
        name,
        arguments.as_str()
    )
}

fn strong_random(bytes: &mut [u8]) -> Result<(), SqlError> {
    let result = unsafe { libc::getentropy(bytes.as_mut_ptr().cast(), bytes.len()) };
    if result == 0 {
        Ok(())
    } else {
        Err(sql_err!(
            sqlstate::INTERNAL_ERROR,
            "could not generate random values"
        ))
    }
}

fn set_version(uuid: &mut [u8; 16], version: u8) {
    uuid[6] = (uuid[6] & 0x0f) | (version << 4);
    uuid[8] = (uuid[8] & 0x3f) | 0x80;
}

fn uuid_v4() -> Result<[u8; 16], SqlError> {
    let mut uuid = [0_u8; 16];
    strong_random(&mut uuid)?;
    set_version(&mut uuid, 4);
    Ok(uuid)
}

fn real_time_ns_ascending() -> Result<i64, SqlError> {
    let duration = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_err(|_| {
            sql_err!(
                sqlstate::DATETIME_FIELD_OVERFLOW,
                "timestamp out of range for UUID version 7"
            )
        })?;
    let seconds = i64::try_from(duration.as_secs()).map_err(|_| {
        sql_err!(
            sqlstate::DATETIME_FIELD_OVERFLOW,
            "timestamp out of range for UUID version 7"
        )
    })?;
    let observed = seconds
        .checked_mul(NS_PER_SECOND)
        .and_then(|value| value.checked_add(i64::from(duration.subsec_nanos())))
        .ok_or_else(|| {
            sql_err!(
                sqlstate::DATETIME_FIELD_OVERFLOW,
                "timestamp out of range for UUID version 7"
            )
        })?;
    PREVIOUS_UUID_V7_NS.with(|previous| {
        let prior = previous.get();
        let next = if prior
            .checked_add(SUBMS_MINIMAL_STEP_NS)
            .is_some_and(|minimum| minimum >= observed)
        {
            prior.checked_add(SUBMS_MINIMAL_STEP_NS).ok_or_else(|| {
                sql_err!(
                    sqlstate::DATETIME_FIELD_OVERFLOW,
                    "timestamp out of range for UUID version 7"
                )
            })?
        } else {
            observed
        };
        previous.set(next);
        Ok(next)
    })
}

fn uuid_v7_from_time(unix_ts_ms: u64, sub_ms_ns: u32) -> Result<[u8; 16], SqlError> {
    debug_assert!(unix_ts_ms < 1_u64 << 48);
    debug_assert!(sub_ms_ns < NS_PER_MILLISECOND as u32);
    let mut uuid = [0_u8; 16];
    let timestamp = unix_ts_ms.to_be_bytes();
    uuid[..6].copy_from_slice(&timestamp[2..]);

    let fraction = (u64::from(sub_ms_ns) * (1 << 12) / NS_PER_MILLISECOND as u64) as u16;
    uuid[6] = (fraction >> 8) as u8;
    uuid[7] = fraction as u8;
    strong_random(&mut uuid[8..])?;

    #[cfg(any(target_os = "macos", target_env = "msvc"))]
    {
        uuid[7] ^= uuid[8] >> 6;
    }

    set_version(&mut uuid, 7);
    Ok(uuid)
}

fn uuid_v7(shift: Option<Interval>) -> Result<[u8; 16], SqlError> {
    let ns = real_time_ns_ascending()?;
    let timestamp = ns / NS_PER_MICROSECOND - PG_UNIX_EPOCH_OFFSET_US;
    let timestamp = match shift {
        Some(interval) => crate::sql::datetime::checked_add_timestamptz(timestamp, interval),
        None => Some(timestamp),
    }
    .ok_or_else(|| {
        sql_err!(
            sqlstate::DATETIME_FIELD_OVERFLOW,
            "timestamp out of range for UUID version 7"
        )
    })?;
    if !(UUID_V7_MIN_TIMESTAMP..=UUID_V7_MAX_TIMESTAMP).contains(&timestamp) {
        return Err(sql_err!(
            sqlstate::DATETIME_FIELD_OVERFLOW,
            "timestamp out of range for UUID version 7"
        ));
    }
    let unix_us = timestamp + PG_UNIX_EPOCH_OFFSET_US;
    uuid_v7_from_time(
        (unix_us / US_PER_MILLISECOND) as u64,
        ((unix_us % US_PER_MILLISECOND) * NS_PER_MICROSECOND + ns % NS_PER_MICROSECOND) as u32,
    )
}

fn extract_version(uuid: [u8; 16]) -> Option<i16> {
    ((uuid[8] & 0xc0) == 0x80).then_some(i16::from(uuid[6] >> 4))
}

fn extract_timestamp(uuid: [u8; 16]) -> Option<i64> {
    if (uuid[8] & 0xc0) != 0x80 {
        return None;
    }
    match uuid[6] >> 4 {
        1 => {
            let ticks = (u64::from(uuid[0]) << 24)
                | (u64::from(uuid[1]) << 16)
                | (u64::from(uuid[2]) << 8)
                | u64::from(uuid[3])
                | (u64::from(uuid[4]) << 40)
                | (u64::from(uuid[5]) << 32)
                | (u64::from(uuid[6] & 0x0f) << 56)
                | (u64::from(uuid[7]) << 48);
            i64::try_from(ticks / 10)
                .ok()
                .and_then(|micros| micros.checked_sub(GREGORIAN_TO_PG_EPOCH_US as i64))
        }
        7 => {
            let milliseconds = uuid[..6]
                .iter()
                .fold(0_u64, |value, byte| (value << 8) | u64::from(*byte));
            i64::try_from(milliseconds)
                .ok()
                .and_then(|value| value.checked_mul(US_PER_MILLISECOND))
                .and_then(|value| value.checked_sub(PG_UNIX_EPOCH_OFFSET_US))
        }
        _ => None,
    }
}

fn expected_argument<'a>(
    name: &str,
    expression: &'a Expr<'a>,
    expected: ColType,
    arena: &'a crate::mem::arena::Arena,
    params: &[Datum<'a>],
    row: &impl ColumnLookup<'a>,
    hooks: &EvalHooks<'_, 'a>,
) -> Result<Datum<'a>, SqlError> {
    let value = eval_full(expression, arena, params, row, hooks)?;
    let actual_oid = expression_type_identity(expression, row, hooks)?.routine_argument_oid(&value);
    if actual_oid == expected.oid()
        || matches!(
            (expected, value),
            (ColType::Uuid, Datum::Uuid(_)) | (ColType::Interval, Datum::Interval(_))
        )
    {
        return Ok(value);
    }
    if actual_oid == crate::sql::types::oid::UNKNOWN {
        return cast_to(value, expected, arena);
    }
    Err(undefined_function(name, ColType::from_oid(actual_oid)))
}

fn positional_arguments(argument_names: &[Option<&str>]) -> bool {
    argument_names.is_empty() || argument_names.iter().all(Option::is_none)
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn dispatch<'a>(
    written_name: &str,
    args: &[&'a Expr<'a>],
    argument_names: &[Option<&str>],
    star: bool,
    arena: &'a crate::mem::arena::Arena,
    params: &[Datum<'a>],
    row: &impl ColumnLookup<'a>,
    hooks: &EvalHooks<'_, 'a>,
) -> Option<Result<Datum<'a>, SqlError>> {
    let name = match written_name.split_once('.') {
        Some(("pg_catalog", name)) => name,
        Some(_) => return None,
        None => written_name,
    };
    if !matches!(
        name,
        "gen_random_uuid" | "uuidv4" | "uuidv7" | "uuid_extract_timestamp" | "uuid_extract_version"
    ) {
        return None;
    }
    Some((|| {
        if star {
            return Err(undefined_function(name, None));
        }
        match name {
            "gen_random_uuid" | "uuidv4" => {
                if !args.is_empty() || !positional_arguments(argument_names) {
                    return Err(undefined_function(name, None));
                }
                uuid_v4().map(Datum::Uuid)
            }
            "uuidv7" => match args {
                [] if positional_arguments(argument_names) => uuid_v7(None).map(Datum::Uuid),
                [shift]
                    if positional_arguments(argument_names)
                        || matches!(argument_names, [Some(argument)] if argument.eq_ignore_ascii_case("shift")) =>
                {
                    match expected_argument(
                        name,
                        shift,
                        ColType::Interval,
                        arena,
                        params,
                        row,
                        hooks,
                    )? {
                        Datum::Null => Ok(Datum::Null),
                        Datum::Interval(interval) => uuid_v7(Some(interval)).map(Datum::Uuid),
                        _ => unreachable!("validated interval argument"),
                    }
                }
                _ => Err(undefined_function(name, None)),
            },
            "uuid_extract_timestamp" | "uuid_extract_version" => {
                let [argument] = args else {
                    return Err(undefined_function(name, None));
                };
                if !positional_arguments(argument_names) {
                    return Err(undefined_function(name, None));
                }
                match expected_argument(name, argument, ColType::Uuid, arena, params, row, hooks)? {
                    Datum::Null => Ok(Datum::Null),
                    Datum::Uuid(uuid) if name == "uuid_extract_timestamp" => {
                        Ok(extract_timestamp(uuid).map_or(Datum::Null, Datum::Timestamptz))
                    }
                    Datum::Uuid(uuid) => Ok(extract_version(uuid).map_or(Datum::Null, Datum::Int2)),
                    _ => unreachable!("validated UUID argument"),
                }
            }
            _ => unreachable!(),
        }
    })())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_and_timestamp_extraction_match_postgresql_fixtures() {
        let v7 = crate::sql::eval::parse_uuid("018cc251-f400-7abc-8000-000000000000").unwrap();
        assert_eq!(extract_version(v7), Some(7));
        assert_eq!(extract_timestamp(v7), Some(757_382_400_000_000));

        let v1 = crate::sql::eval::parse_uuid("04c296c2-0c98-11f0-8000-000000000000").unwrap();
        assert_eq!(extract_version(v1), Some(1));
        assert_eq!(extract_timestamp(v1), Some(796_565_950_290_912));

        let non_rfc = crate::sql::eval::parse_uuid("a0eebc99-9c0b-4ef8-3b6d-6bb9bd380a11").unwrap();
        assert_eq!(extract_version(non_rfc), None);
        assert_eq!(extract_timestamp(non_rfc), None);
    }

    #[test]
    fn generated_versions_set_rfc_variant_and_advance_v7() {
        let v4 = uuid_v4().unwrap();
        assert_eq!(extract_version(v4), Some(4));

        let first = uuid_v7(None).unwrap();
        let second = uuid_v7(None).unwrap();
        assert_eq!(extract_version(first), Some(7));
        assert!(first < second);
        assert!(extract_timestamp(first).is_some());
    }
}
