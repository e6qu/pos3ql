//! Date/time and temporal-formatting built-ins.
//!
//! Covers the current-time family (`now`/`current_timestamp`/…,
//! `current_date`), bucketing and inspection (`date_bin`, `isfinite`,
//! `extract`/`date_part`, `date_trunc`), construction (`make_date`/`make_time`/
//! `make_timestamp`/`make_timestamptz`, `make_interval`), interval normalization
//! (`age`, `justify_hours`/`justify_days`/`justify_interval`), period overlap
//! (`overlaps`), timezone shifting (`timezone`), and the temporal formatting
//! conversions (`to_char`, `to_timestamp`, `to_date`). The numeric `to_number`
//! and the regex `similar_to` sit amid these in the router but are not temporal,
//! so they stay there.

use crate::sql::ast::Expr;
use crate::sql::numeric::Numeric;
use crate::sql::types::{ColType, Datum, Interval};
use crate::sql::{datetime, guc, to_char};
use crate::{sql_err, stack_format};

use super::super::{
    ColumnLookup, EvalHooks, SqlError, arena_full, cast_to, datum_numeric, eval_full, int_arg,
    interval_extract, num_f64, num_factor, overlaps_end_micros, overlaps_micros, sqlstate,
    text_arg, text_view, timestamp_micros, type_mismatch,
};

/// The session zone's offset (seconds east) at an instant.
fn session_offset(utc_micros: i64) -> i32 {
    crate::sql::timezone::session().resolve(utc_micros).0
}

fn timezone_from_datum(value: Datum<'_>) -> Result<crate::sql::timezone::Timezone, SqlError> {
    match text_view(value) {
        Datum::Text(name) => guc::parse_timezone(name).ok_or_else(|| {
            sql_err!(
                sqlstate::INVALID_PARAMETER_VALUE,
                "time zone \"{}\" not recognized",
                name
            )
        }),
        Datum::Interval(interval) if interval.months == 0 && interval.days == 0 => {
            let seconds = i32::try_from(interval.micros / 1_000_000).map_err(|_| {
                sql_err!(
                    sqlstate::INVALID_PARAMETER_VALUE,
                    "interval time zone is out of range"
                )
            })?;
            Ok(crate::sql::timezone::Timezone::fixed(seconds, ""))
        }
        Datum::Interval(_) => Err(sql_err!(
            sqlstate::INVALID_PARAMETER_VALUE,
            "interval time zone must not include months or days"
        )),
        other => Err(type_mismatch("timezone", &other)),
    }
}

fn temporal_support(name: &str) -> bool {
    datetime::is_comparison_function(name)
        || matches!(
            name,
            "date_cmp"
                | "date_cmp_timestamp"
                | "date_cmp_timestamptz"
                | "time_cmp"
                | "timetz_cmp"
                | "timestamp_cmp"
                | "timestamp_cmp_date"
                | "timestamp_cmp_timestamptz"
                | "timestamptz_cmp"
                | "timestamptz_cmp_date"
                | "timestamptz_cmp_timestamp"
                | "interval_cmp"
                | "date_larger"
                | "date_smaller"
                | "time_larger"
                | "time_smaller"
                | "timetz_larger"
                | "timetz_smaller"
                | "timestamp_larger"
                | "timestamp_smaller"
                | "timestamptz_larger"
                | "timestamptz_smaller"
                | "interval_larger"
                | "interval_smaller"
                | "date_mi"
                | "date_pli"
                | "date_mii"
                | "date_pl_interval"
                | "date_mi_interval"
                | "time_mi_time"
                | "time_pl_interval"
                | "time_mi_interval"
                | "timetz_pl_interval"
                | "timetz_mi_interval"
                | "timestamp_mi"
                | "timestamp_pl_interval"
                | "timestamp_mi_interval"
                | "timestamptz_mi"
                | "timestamptz_pl_interval"
                | "timestamptz_mi_interval"
                | "interval_um"
                | "interval_pl"
                | "interval_mi"
                | "interval_mul"
                | "mul_d_interval"
                | "interval_div"
                | "datetime_pl"
                | "timedate_pl"
                | "datetimetz_pl"
                | "timetzdate_pl"
                | "interval_pl_date"
                | "interval_pl_time"
                | "interval_pl_timetz"
                | "interval_pl_timestamp"
                | "interval_pl_timestamptz"
                | "integer_pl_date"
        )
}

fn temporal_hash_input(name: &str, value: Datum<'_>) -> Result<u32, SqlError> {
    let int64 = |value| crate::sql::identity::hash_int64_input(value);
    match (name, value) {
        ("hashdate" | "hashdateextended", Datum::Date(days)) => Ok(days as u32),
        (
            "time_hash"
            | "time_hash_extended"
            | "timestamp_hash"
            | "timestamp_hash_extended"
            | "timestamptz_hash"
            | "timestamptz_hash_extended",
            Datum::Time(micros) | Datum::Timestamp(micros) | Datum::Timestamptz(micros),
        ) => Ok(int64(micros)),
        ("interval_hash" | "interval_hash_extended", Datum::Interval(interval)) => {
            // interval_cmp_value uses a 128-bit 30-day-month span; PostgreSQL
            // deliberately hashes only its low 64 bits for compatibility.
            let span = i128::from(interval.micros)
                + i128::from(interval.days) * 86_400_000_000
                + i128::from(interval.months) * 30 * 86_400_000_000;
            Ok(int64(span as i64))
        }
        (_, other) => Err(type_mismatch(name, &other)),
    }
}

/// Rounds `micros` to an optional fractional-second precision argument, which
/// the SQL-standard niladic functions accept as `current_time(3)`.
#[allow(clippy::too_many_arguments)]
fn round_to_precision<'a, R: ColumnLookup<'a>>(
    name: &str,
    args: &[&Expr<'a>],
    arena: &'a crate::mem::arena::Arena,
    params: &[Datum<'a>],
    row: &R,
    hooks: &EvalHooks<'_, 'a>,
    micros: i64,
) -> Result<i64, SqlError> {
    if args.is_empty() {
        return Ok(micros);
    }
    if args.len() > 1 {
        return Err(sql_err!(
            sqlstate::UNDEFINED_FUNCTION,
            "function {} takes at most one argument",
            name
        ));
    }
    let p = int_arg(name, args, 0, arena, params, row, hooks)?
        .unwrap_or(6)
        .clamp(0, 6);
    let scale = 10i64.pow(6 - p as u32);
    Ok(micros.div_euclid(scale) * scale)
}

/// Handles the date/time family. Returns `None` if `name` is not one of these
/// functions, leaving the router to keep matching.
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
    if !matches!(
        name,
        "now"
            | "current_timestamp"
            | "transaction_timestamp"
            | "statement_timestamp"
            | "clock_timestamp"
            | "timeofday"
            | "date_add"
            | "date_subtract"
            | "date"
            | "timestamp"
            | "timestamptz"
            | "time"
            | "timetz"
            | "interval"
            | "hashdate"
            | "hashdateextended"
            | "time_hash"
            | "time_hash_extended"
            | "timetz_hash"
            | "timetz_hash_extended"
            | "timestamp_hash"
            | "timestamp_hash_extended"
            | "timestamptz_hash"
            | "timestamptz_hash_extended"
            | "interval_hash"
            | "interval_hash_extended"
            | "date_bin"
            | "isfinite"
            | "current_date"
            | "current_time"
            | "localtime"
            | "localtimestamp"
            | "to_char"
            | "to_timestamp"
            | "to_date"
            | "make_date"
            | "make_time"
            | "make_timestamp"
            | "make_timestamptz"
            | "make_interval"
            | "timezone"
            | "age"
            | "justify_hours"
            | "justify_days"
            | "justify_interval"
            | crate::sql::parser::OVERLAPS_PERIODS
            | "extract"
            | "date_part"
            | "date_trunc"
    ) && !temporal_support(name)
    {
        return None;
    }
    let arity = |n: usize| -> Result<(), SqlError> {
        if args.len() != n || star {
            Err(sql_err!(
                sqlstate::UNDEFINED_FUNCTION,
                "function {}(...) with {} arguments does not exist",
                name,
                if star { 1 } else { args.len() }
            ))
        } else {
            Ok(())
        }
    };
    Some((|| -> Result<Datum<'a>, SqlError> {
        match name {
            "hashdate"
            | "hashdateextended"
            | "time_hash"
            | "time_hash_extended"
            | "timestamp_hash"
            | "timestamp_hash_extended"
            | "timestamptz_hash"
            | "timestamptz_hash_extended"
            | "interval_hash"
            | "interval_hash_extended" => {
                let extended = name.ends_with("extended");
                arity(if extended { 2 } else { 1 })?;
                let value = eval_full(args[0], arena, params, row, hooks)?;
                if value.is_null() {
                    return Ok(Datum::Null);
                }
                let input = temporal_hash_input(name, value)?;
                if extended {
                    let seed = int_arg(name, args, 1, arena, params, row, hooks)?
                        .ok_or_else(|| type_mismatch(name, &Datum::Null))?;
                    Ok(Datum::Int8(
                        crate::sql::identity::hash_uint32_extended(input, seed) as i64,
                    ))
                } else {
                    Ok(Datum::Int4(crate::sql::identity::hash_uint32(input) as i32))
                }
            }
            "timetz_hash" | "timetz_hash_extended" => {
                let extended = name.ends_with("extended");
                arity(if extended { 2 } else { 1 })?;
                let value = eval_full(args[0], arena, params, row, hooks)?;
                let Datum::Timetz(micros, seconds_east) = value else {
                    if value.is_null() {
                        return Ok(Datum::Null);
                    }
                    return Err(type_mismatch(name, &value));
                };
                let time_input = crate::sql::identity::hash_int64_input(micros);
                let zone = (-seconds_east) as u32;
                if extended {
                    let seed = int_arg(name, args, 1, arena, params, row, hooks)?
                        .ok_or_else(|| type_mismatch(name, &Datum::Null))?;
                    let time = crate::sql::identity::hash_uint32_extended(time_input, seed);
                    let zone = crate::sql::identity::hash_uint32_extended(zone, seed);
                    Ok(Datum::Int8((time ^ zone) as i64))
                } else {
                    let time = crate::sql::identity::hash_uint32(time_input);
                    let zone = crate::sql::identity::hash_uint32(zone);
                    Ok(Datum::Int4((time ^ zone) as i32))
                }
            }
            "date" | "timestamp" | "timestamptz" | "time" | "timetz" | "interval" => {
                if !matches!(args.len(), 1 | 2) || star {
                    return Err(sql_err!(
                        sqlstate::UNDEFINED_FUNCTION,
                        "function {}(...) with {} arguments does not exist",
                        name,
                        args.len()
                    ));
                }
                let first = eval_full(args[0], arena, params, row, hooks)?;
                if first.is_null() {
                    return Ok(Datum::Null);
                }
                let target = match name {
                    "date" => ColType::Date,
                    "timestamp" => ColType::Timestamp,
                    "timestamptz" => ColType::Timestamptz,
                    "time" => ColType::Time,
                    "timetz" => ColType::Timetz,
                    "interval" => ColType::Interval,
                    _ => unreachable!(),
                };
                if args.len() == 1 {
                    return cast_to(first, target, arena);
                }
                let second = eval_full(args[1], arena, params, row, hooks)?;
                if second.is_null() {
                    return Ok(Datum::Null);
                }
                if matches!(
                    (&first, &second, name),
                    (Datum::Date(_), Datum::Time(_), "timestamp" | "timestamptz")
                        | (Datum::Date(_), Datum::Timetz(..), "timestamptz")
                ) {
                    let combined = super::super::operators::binary(
                        crate::sql::ast::BinaryOp::Add,
                        first,
                        second,
                        false,
                        false,
                        arena,
                    )?;
                    return cast_to(combined, target, arena);
                }
                let typmod = match second {
                    Datum::Int2(value) => i32::from(value),
                    Datum::Int4(value) => value,
                    Datum::Int8(value) => i32::try_from(value).map_err(|_| {
                        sql_err!(sqlstate::NUMERIC_OUT_OF_RANGE, "integer out of range")
                    })?,
                    other => return Err(type_mismatch(name, &other)),
                };
                crate::sql::exec::apply_typmod(first, target, typmod, arena)
            }
            name if datetime::is_comparison_function(name) => {
                arity(2)?;
                let left = eval_full(args[0], arena, params, row, hooks)?;
                let right = eval_full(args[1], arena, params, row, hooks)?;
                if left.is_null() || right.is_null() {
                    return Ok(Datum::Null);
                }
                let ordering = super::super::operators::compare_datums(&left, &right)?;
                let value = if name.contains("_eq") {
                    ordering.is_eq()
                } else if name.contains("_ne") {
                    ordering.is_ne()
                } else if name.contains("_lt") {
                    ordering.is_lt()
                } else if name.contains("_le") {
                    ordering.is_le()
                } else if name.contains("_gt") {
                    ordering.is_gt()
                } else {
                    ordering.is_ge()
                };
                Ok(Datum::Bool(value))
            }
            "date_cmp"
            | "date_cmp_timestamp"
            | "date_cmp_timestamptz"
            | "time_cmp"
            | "timetz_cmp"
            | "timestamp_cmp"
            | "timestamp_cmp_date"
            | "timestamp_cmp_timestamptz"
            | "timestamptz_cmp"
            | "timestamptz_cmp_date"
            | "timestamptz_cmp_timestamp"
            | "interval_cmp" => {
                arity(2)?;
                let left = eval_full(args[0], arena, params, row, hooks)?;
                let right = eval_full(args[1], arena, params, row, hooks)?;
                if left.is_null() || right.is_null() {
                    return Ok(Datum::Null);
                }
                Ok(Datum::Int4(
                    match super::super::operators::compare_datums(&left, &right)? {
                        core::cmp::Ordering::Less => -1,
                        core::cmp::Ordering::Equal => 0,
                        core::cmp::Ordering::Greater => 1,
                    },
                ))
            }
            "date_larger"
            | "date_smaller"
            | "time_larger"
            | "time_smaller"
            | "timetz_larger"
            | "timetz_smaller"
            | "timestamp_larger"
            | "timestamp_smaller"
            | "timestamptz_larger"
            | "timestamptz_smaller"
            | "interval_larger"
            | "interval_smaller" => {
                arity(2)?;
                let left = eval_full(args[0], arena, params, row, hooks)?;
                let right = eval_full(args[1], arena, params, row, hooks)?;
                if left.is_null() || right.is_null() {
                    return Ok(Datum::Null);
                }
                let ordering = super::super::operators::compare_datums(&left, &right)?;
                let take_left = if name.ends_with("larger") {
                    ordering.is_gt()
                } else {
                    ordering.is_lt()
                };
                Ok(if take_left { left } else { right })
            }
            "interval_um" => {
                arity(1)?;
                match eval_full(args[0], arena, params, row, hooks)? {
                    Datum::Null => Ok(Datum::Null),
                    Datum::Interval(interval) => Ok(Datum::Interval(
                        match datetime::interval_infinity_sign(interval) {
                            1 => datetime::INTERVAL_NEG_INFINITY,
                            -1 => datetime::INTERVAL_INFINITY,
                            _ => Interval {
                                months: interval.months.checked_neg().ok_or_else(|| {
                                    sql_err!(
                                        sqlstate::INTERVAL_FIELD_OVERFLOW,
                                        "interval out of range"
                                    )
                                })?,
                                days: interval.days.checked_neg().ok_or_else(|| {
                                    sql_err!(
                                        sqlstate::INTERVAL_FIELD_OVERFLOW,
                                        "interval out of range"
                                    )
                                })?,
                                micros: interval.micros.checked_neg().ok_or_else(|| {
                                    sql_err!(
                                        sqlstate::INTERVAL_FIELD_OVERFLOW,
                                        "interval out of range"
                                    )
                                })?,
                            },
                        },
                    )),
                    other => Err(type_mismatch(name, &other)),
                }
            }
            name if temporal_support(name) => {
                arity(2)?;
                let left = eval_full(args[0], arena, params, row, hooks)?;
                let right = eval_full(args[1], arena, params, row, hooks)?;
                if left.is_null() || right.is_null() {
                    return Ok(Datum::Null);
                }
                let operator = if matches!(name, "interval_mul" | "mul_d_interval") {
                    crate::sql::ast::BinaryOp::Mul
                } else if name == "interval_div" {
                    crate::sql::ast::BinaryOp::Div
                } else if matches!(
                    name,
                    "date_mi"
                        | "date_mii"
                        | "date_mi_interval"
                        | "time_mi_time"
                        | "time_mi_interval"
                        | "timetz_mi_interval"
                        | "timestamp_mi"
                        | "timestamp_mi_interval"
                        | "timestamptz_mi"
                        | "timestamptz_mi_interval"
                        | "interval_mi"
                ) {
                    crate::sql::ast::BinaryOp::Sub
                } else {
                    crate::sql::ast::BinaryOp::Add
                };
                super::super::operators::binary(operator, left, right, false, false, arena)
            }
            "now"
            | "current_timestamp"
            | "transaction_timestamp"
            | "statement_timestamp"
            | "clock_timestamp"
            | "localtimestamp" => {
                // Only `clock_timestamp` reads the clock; `statement_timestamp`
                // is fixed for the statement and the rest for the transaction.
                let base = match name {
                    "clock_timestamp" => datetime::now_micros(),
                    "statement_timestamp" => datetime::statement_micros(),
                    _ => datetime::transaction_micros(),
                };
                let micros = round_to_precision(name, args, arena, params, row, hooks, base)?;
                Ok(if name == "localtimestamp" {
                    // The session's wall clock, with no zone attached.
                    Datum::Timestamp(micros + session_offset(micros) as i64 * 1_000_000)
                } else {
                    Datum::Timestamptz(micros)
                })
            }
            "timeofday" => {
                arity(0)?;
                let now = datetime::now_micros();
                let (offset, abbreviation) = crate::sql::timezone::session().resolve(now);
                let local = now
                    .checked_add(i64::from(offset) * 1_000_000)
                    .ok_or_else(|| {
                        sql_err!(sqlstate::DATETIME_FIELD_OVERFLOW, "timestamp out of range")
                    })?;
                let days = local.div_euclid(86_400_000_000);
                let in_day = local.rem_euclid(86_400_000_000);
                let (year, month, day) = datetime::civil_from_days(days + datetime::PG_EPOCH_DAYS);
                let seconds = in_day / 1_000_000;
                let micros = in_day % 1_000_000;
                let weekdays = ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"];
                let months = [
                    "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov",
                    "Dec",
                ];
                let rendered = stack_format!(
                    64,
                    "{} {} {:02} {:02}:{:02}:{:02}.{:06} {} {}",
                    weekdays[datetime::day_of_week(days)],
                    months[month as usize - 1],
                    day,
                    seconds / 3600,
                    seconds / 60 % 60,
                    seconds % 60,
                    micros,
                    year,
                    abbreviation.as_str()
                );
                Ok(Datum::Text(
                    arena
                        .alloc_str(rendered.as_str())
                        .map_err(|_| arena_full())?,
                ))
            }
            // `current_time` carries the session's offset; `localtime` is the
            // same wall clock with the zone dropped.
            "current_time" | "localtime" => {
                let now = datetime::transaction_micros();
                let offset = session_offset(now);
                let local = round_to_precision(name, args, arena, params, row, hooks, now)?
                    + offset as i64 * 1_000_000;
                let in_day = local.rem_euclid(86_400_000_000);
                Ok(if name == "current_time" {
                    Datum::Timetz(in_day, offset)
                } else {
                    Datum::Time(in_day)
                })
            }
            "date_add" | "date_subtract" => {
                if !matches!(args.len(), 2 | 3) || star {
                    return Err(sql_err!(
                        sqlstate::UNDEFINED_FUNCTION,
                        "function {}(...) with {} arguments does not exist",
                        name,
                        args.len()
                    ));
                }
                let source = eval_full(args[0], arena, params, row, hooks)?;
                let interval = eval_full(args[1], arena, params, row, hooks)?;
                if source.is_null() || interval.is_null() {
                    return Ok(Datum::Null);
                }
                let Datum::Timestamptz(source) = cast_to(source, ColType::Timestamptz, arena)?
                else {
                    unreachable!("timestamptz cast returned another type")
                };
                let Datum::Interval(mut interval) = cast_to(interval, ColType::Interval, arena)?
                else {
                    unreachable!("interval cast returned another type")
                };
                if name == "date_subtract" {
                    interval = match datetime::interval_infinity_sign(interval) {
                        1 => datetime::INTERVAL_NEG_INFINITY,
                        -1 => datetime::INTERVAL_INFINITY,
                        _ => Interval {
                            months: interval.months.checked_neg().ok_or_else(|| {
                                sql_err!(sqlstate::DATETIME_FIELD_OVERFLOW, "interval out of range")
                            })?,
                            days: interval.days.checked_neg().ok_or_else(|| {
                                sql_err!(sqlstate::DATETIME_FIELD_OVERFLOW, "interval out of range")
                            })?,
                            micros: interval.micros.checked_neg().ok_or_else(|| {
                                sql_err!(sqlstate::DATETIME_FIELD_OVERFLOW, "interval out of range")
                            })?,
                        },
                    };
                }
                let zone = if args.len() == 3 {
                    let zone = eval_full(args[2], arena, params, row, hooks)?;
                    if zone.is_null() {
                        return Ok(Datum::Null);
                    }
                    timezone_from_datum(zone)?
                } else {
                    crate::sql::timezone::session()
                };
                datetime::checked_add_timestamptz_in_zone(source, interval, zone)
                    .map(Datum::Timestamptz)
                    .ok_or_else(|| {
                        sql_err!(sqlstate::DATETIME_FIELD_OVERFLOW, "timestamp out of range")
                    })
            }
            // `date_bin(stride, source, origin)`: the stride-aligned bucket start at
            // or before `source`, measured from `origin`. Strides with a month or
            // year component are rejected, as in PostgreSQL.
            "date_bin" => {
                arity(3)?;
                // The stride is an interval — coerce a bare string literal.
                let stride = match cast_to(
                    eval_full(args[0], arena, params, row, hooks)?,
                    ColType::Interval,
                    arena,
                )? {
                    Datum::Interval(iv) => iv,
                    _ => return Ok(Datum::Null),
                };
                let source = eval_full(args[1], arena, params, row, hooks)?;
                let origin = eval_full(args[2], arena, params, row, hooks)?;
                let (source_micros, tz) = match source {
                    Datum::Timestamp(v) => (v, false),
                    Datum::Timestamptz(v) => (v, true),
                    Datum::Null => return Ok(Datum::Null),
                    other => {
                        return Err(type_mismatch("date_bin source must be a timestamp", &other));
                    }
                };
                let origin_micros = match origin {
                    Datum::Timestamp(v) | Datum::Timestamptz(v) => v,
                    Datum::Null => return Ok(Datum::Null),
                    other => {
                        return Err(type_mismatch("date_bin origin must be a timestamp", &other));
                    }
                };
                if matches!(
                    source_micros,
                    datetime::TIMESTAMP_INFINITY | datetime::TIMESTAMP_NEG_INFINITY
                ) {
                    return Ok(if tz {
                        Datum::Timestamptz(source_micros)
                    } else {
                        Datum::Timestamp(source_micros)
                    });
                }
                if matches!(
                    origin_micros,
                    datetime::TIMESTAMP_INFINITY | datetime::TIMESTAMP_NEG_INFINITY
                ) {
                    return Err(sql_err!(
                        sqlstate::DATETIME_FIELD_OVERFLOW,
                        "origin out of range"
                    ));
                }
                if stride.months != 0 {
                    return Err(sql_err!(
                        sqlstate::FEATURE_NOT_SUPPORTED,
                        "timestamps cannot be binned into intervals containing months or years"
                    ));
                }
                let stride_micros = i64::from(stride.days)
                    .checked_mul(86_400_000_000)
                    .and_then(|days| days.checked_add(stride.micros))
                    .ok_or_else(|| {
                        sql_err!(sqlstate::DATETIME_FIELD_OVERFLOW, "interval out of range")
                    })?;
                if stride_micros <= 0 {
                    return Err(sql_err!(
                        sqlstate::DATETIME_FIELD_OVERFLOW,
                        "stride must be greater than zero"
                    ));
                }
                let delta = i128::from(source_micros) - i128::from(origin_micros);
                // Floor-division so the bucket start is at or before the source.
                let binned = i128::from(origin_micros)
                    + delta.div_euclid(i128::from(stride_micros)) * i128::from(stride_micros);
                let binned = i64::try_from(binned).map_err(|_| {
                    sql_err!(sqlstate::DATETIME_FIELD_OVERFLOW, "timestamp out of range")
                })?;
                Ok(if tz {
                    Datum::Timestamptz(binned)
                } else {
                    Datum::Timestamp(binned)
                })
            }
            // `isfinite` recognizes the sentinels for the three temporal
            // families on which PostgreSQL defines infinity.
            "isfinite" => {
                arity(1)?;
                match eval_full(args[0], arena, params, row, hooks)? {
                    Datum::Null => Ok(Datum::Null),
                    Datum::Date(value) => Ok(Datum::Bool(!matches!(
                        value,
                        datetime::DATE_INFINITY | datetime::DATE_NEG_INFINITY
                    ))),
                    Datum::Timestamp(value) | Datum::Timestamptz(value) => {
                        Ok(Datum::Bool(!matches!(
                            value,
                            datetime::TIMESTAMP_INFINITY | datetime::TIMESTAMP_NEG_INFINITY
                        )))
                    }
                    Datum::Interval(value) => {
                        Ok(Datum::Bool(datetime::interval_infinity_sign(value) == 0))
                    }
                    other => Err(type_mismatch(
                        "isfinite requires a date/time/interval",
                        &other,
                    )),
                }
            }
            "current_date" => {
                arity(0)?;
                // Today in the session zone, as of the transaction's clock.
                let local = datetime::transaction_micros()
                    + session_offset(datetime::transaction_micros()) as i64 * 1_000_000;
                Ok(Datum::Date(local.div_euclid(86_400_000_000) as i32))
            }
            "to_char" => {
                arity(2)?;
                let v = eval_full(args[0], arena, params, row, hooks)?;
                let f = eval_full(args[1], arena, params, row, hooks)?;
                if v.is_null() || f.is_null() {
                    return Ok(Datum::Null);
                }
                let f = text_view(f);
                let Datum::Text(fmt) = f else {
                    return Err(type_mismatch(name, &f));
                };
                match v {
                    Datum::Timestamp(value) | Datum::Timestamptz(value)
                        if matches!(
                            value,
                            datetime::TIMESTAMP_INFINITY | datetime::TIMESTAMP_NEG_INFINITY
                        ) =>
                    {
                        return Ok(Datum::Null);
                    }
                    Datum::Date(datetime::DATE_INFINITY | datetime::DATE_NEG_INFINITY) => {
                        return Ok(Datum::Null);
                    }
                    Datum::Timestamp(value) => {
                        return Ok(Datum::Text(to_char::timestamp(value, fmt, arena)?));
                    }
                    Datum::Timestamptz(value) => {
                        let (offset, abbreviation) = crate::sql::timezone::session().resolve(value);
                        return Ok(Datum::Text(to_char::timestamptz(
                            value,
                            offset,
                            abbreviation.as_str(),
                            fmt,
                            arena,
                        )?));
                    }
                    Datum::Date(value) => {
                        return Ok(Datum::Text(to_char::timestamp(
                            i64::from(value) * 86_400_000_000,
                            fmt,
                            arena,
                        )?));
                    }
                    Datum::Time(value) => {
                        return Ok(Datum::Text(to_char::time(value, fmt, arena)?));
                    }
                    Datum::Interval(value) => {
                        return Ok(Datum::Text(to_char::interval(value, fmt, arena)?));
                    }
                    Datum::Timetz(..) => {
                        return Err(sql_err!(
                            sqlstate::UNDEFINED_FUNCTION,
                            "function to_char(time with time zone, text) does not exist"
                        ));
                    }
                    _ => {}
                }
                // A float8 input keeps its own sign bit even when the value rounds
                // to zero (covers -0.0 and small negatives) — PostgreSQL behavior.
                // real widens to double precision for to_char, so it shares the
                // float8 sign-bit and NaN/Infinity handling.
                let float_negative = matches!(v, Datum::Float8(x) if x.is_sign_negative())
                    || matches!(v, Datum::Float4(x) if x.is_sign_negative());
                let float_source = match v {
                    Datum::Float8(x) => Some(x),
                    Datum::Float4(x) => Some(f64::from(x)),
                    _ => None,
                };
                // NaN/Infinity have no numeric form; the formatter reads them
                // from `float_source` (and fills with `#`, as PostgreSQL).
                let n = match float_source {
                    Some(x) if !x.is_finite() => Numeric::parse("0", arena)?,
                    _ => datum_numeric(name, v, arena)?,
                };
                Ok(Datum::Text(to_char::number(
                    &n,
                    fmt,
                    float_negative,
                    float_source,
                    arena,
                )?))
            }
            // `to_timestamp(double)` converts a Unix epoch (seconds) to timestamptz.
            "to_timestamp" if args.len() == 1 => {
                match eval_full(args[0], arena, params, row, hooks)? {
                    Datum::Null => Ok(Datum::Null),
                    d => {
                        let Some(seconds) = num_factor(&d) else {
                            return Err(type_mismatch(name, &d));
                        };
                        let micros = (seconds * 1_000_000.0).round() as i64
                            - datetime::PG_EPOCH_DAYS * 86_400_000_000;
                        Ok(Datum::Timestamptz(micros))
                    }
                }
            }
            "to_date" | "to_timestamp" => {
                arity(2)?;
                let (Some(s), Some(fmt)) = (
                    text_arg(name, args, 0, arena, params, row, hooks)?,
                    text_arg(name, args, 1, arena, params, row, hooks)?,
                ) else {
                    return Ok(Datum::Null);
                };
                if name == "to_date" {
                    Ok(Datum::Date(datetime::to_date(s, fmt)?))
                } else {
                    Ok(Datum::Timestamptz(datetime::to_timestamp(s, fmt)?))
                }
            }
            "make_date" | "make_time" | "make_timestamp" | "make_timestamptz" => {
                let field_count = if name == "make_timestamp" || name == "make_timestamptz" {
                    6
                } else {
                    3
                };
                if name == "make_timestamptz" {
                    if !matches!(args.len(), 6 | 7) || star {
                        return Err(sql_err!(
                            sqlstate::UNDEFINED_FUNCTION,
                            "function {}(...) with {} arguments does not exist",
                            name,
                            args.len()
                        ));
                    }
                } else {
                    arity(field_count)?;
                }
                // The seconds field is a double; every other field is an integer.
                let sec_idx = if name == "make_date" {
                    usize::MAX
                } else {
                    field_count - 1
                };
                let mut ints = [0i64; 6];
                for (i, slot) in ints[..field_count].iter_mut().enumerate() {
                    if i == sec_idx {
                        continue;
                    }
                    match int_arg(name, args, i, arena, params, row, hooks)? {
                        Some(v) => *slot = v,
                        None => return Ok(Datum::Null),
                    }
                }
                let sec = if sec_idx == usize::MAX {
                    0.0
                } else {
                    match num_f64(name, args, sec_idx, arena, params, row, hooks)? {
                        Some(v) => v,
                        None => return Ok(Datum::Null),
                    }
                };
                match name {
                    "make_date" => Ok(Datum::Date(datetime::make_date(ints[0], ints[1], ints[2])?)),
                    "make_time" => Ok(Datum::Time(datetime::make_time(ints[0], ints[1], sec)?)),
                    "make_timestamptz" => {
                        let local = datetime::make_timestamp(
                            ints[0], ints[1], ints[2], ints[3], ints[4], sec,
                        )?;
                        let zone = if args.len() == 7 {
                            let zone = eval_full(args[6], arena, params, row, hooks)?;
                            if zone.is_null() {
                                return Ok(Datum::Null);
                            }
                            timezone_from_datum(zone)?
                        } else {
                            crate::sql::timezone::session()
                        };
                        Ok(Datum::Timestamptz(zone.resolve_local(local).ok_or_else(
                            || {
                                sql_err!(
                                    sqlstate::DATETIME_FIELD_OVERFLOW,
                                    "timestamp out of range"
                                )
                            },
                        )?))
                    }
                    _ => Ok(Datum::Timestamp(datetime::make_timestamp(
                        ints[0], ints[1], ints[2], ints[3], ints[4], sec,
                    )?)),
                }
            }
            "make_interval" => {
                // Seven positional fields (the parser desugars named arguments):
                // years, months, weeks, days, hours, mins (integers) and secs
                // (double precision). Years fold into months and weeks into days,
                // matching PostgreSQL's interval field composition.
                arity(7)?;
                let mut ints = [0i64; 6];
                for (i, slot) in ints.iter_mut().enumerate() {
                    match int_arg(name, args, i, arena, params, row, hooks)? {
                        Some(v) => *slot = v,
                        None => return Ok(Datum::Null),
                    }
                }
                let secs = match num_f64(name, args, 6, arena, params, row, hooks)? {
                    Some(v) => v,
                    None => return Ok(Datum::Null),
                };
                let months = ints[0]
                    .checked_mul(12)
                    .and_then(|y| y.checked_add(ints[1]))
                    .and_then(|m| i32::try_from(m).ok());
                let days = ints[2]
                    .checked_mul(7)
                    .and_then(|w| w.checked_add(ints[3]))
                    .and_then(|d| i32::try_from(d).ok());
                let (Some(months), Some(days)) = (months, days) else {
                    return Err(sql_err!(
                        sqlstate::DATETIME_FIELD_OVERFLOW,
                        "interval field value out of range"
                    ));
                };
                let sec_micros = (secs * 1_000_000.0).round();
                let micros = ints[4]
                    .checked_mul(3_600_000_000)
                    .and_then(|h| {
                        ints[5]
                            .checked_mul(60_000_000)
                            .and_then(|m| h.checked_add(m))
                    })
                    .filter(|_| sec_micros.is_finite() && sec_micros.abs() < 9.2e18)
                    .and_then(|hm| hm.checked_add(sec_micros as i64));
                let Some(micros) = micros else {
                    return Err(sql_err!(
                        sqlstate::DATETIME_FIELD_OVERFLOW,
                        "interval field value out of range"
                    ));
                };
                Ok(Datum::Interval(Interval {
                    months,
                    days,
                    micros,
                }))
            }
            "timezone" => {
                // The two-argument forms implement `AT TIME ZONE`; PostgreSQL
                // 18's one-argument forms implement `AT LOCAL`. A zone is a
                // name or a fixed interval with no month/day fields.
                if !matches!(args.len(), 1 | 2) || star {
                    return Err(sql_err!(
                        sqlstate::UNDEFINED_FUNCTION,
                        "function {}(...) with {} arguments does not exist",
                        name,
                        args.len()
                    ));
                }
                let (zone, value_index) = if args.len() == 1 {
                    (crate::sql::timezone::session(), 0)
                } else {
                    let zone = eval_full(args[0], arena, params, row, hooks)?;
                    if zone.is_null() {
                        return Ok(Datum::Null);
                    }
                    (timezone_from_datum(zone)?, 1)
                };
                match text_view(eval_full(args[value_index], arena, params, row, hooks)?) {
                    Datum::Null => Ok(Datum::Null),
                    Datum::Timestamptz(utc)
                        if matches!(
                            utc,
                            datetime::TIMESTAMP_INFINITY | datetime::TIMESTAMP_NEG_INFINITY
                        ) =>
                    {
                        Ok(Datum::Timestamp(utc))
                    }
                    Datum::Timestamptz(utc) => {
                        let (offset_seconds, _) = zone.resolve(utc);
                        Ok(Datum::Timestamp(
                            utc + i64::from(offset_seconds) * 1_000_000,
                        ))
                    }
                    // An untyped literal coerces to timestamp *with* time
                    // zone (session-zone interpreted), and the operator then
                    // converts it into the named zone — PostgreSQL's
                    // resolution of `'2021-07-04 12:00' AT TIME ZONE z`.
                    Datum::Text(s) => {
                        let utc = datetime::parse_timestamp(s, true)?;
                        if matches!(
                            utc,
                            datetime::TIMESTAMP_INFINITY | datetime::TIMESTAMP_NEG_INFINITY
                        ) {
                            return Ok(Datum::Timestamp(utc));
                        }
                        let (offset_seconds, _) = zone.resolve(utc);
                        Ok(Datum::Timestamp(
                            utc + i64::from(offset_seconds) * 1_000_000,
                        ))
                    }
                    Datum::Timestamp(wall_clock)
                        if matches!(
                            wall_clock,
                            datetime::TIMESTAMP_INFINITY | datetime::TIMESTAMP_NEG_INFINITY
                        ) =>
                    {
                        Ok(Datum::Timestamptz(wall_clock))
                    }
                    Datum::Timestamp(wall_clock) => Ok(Datum::Timestamptz(
                        zone.resolve_local(wall_clock).ok_or_else(|| {
                            sql_err!(sqlstate::DATETIME_FIELD_OVERFLOW, "timestamp out of range")
                        })?,
                    )),
                    Datum::Timetz(time, old_offset) => {
                        let new_offset = zone.resolve(datetime::transaction_micros()).0;
                        let shifted = time - i64::from(old_offset) * 1_000_000
                            + i64::from(new_offset) * 1_000_000;
                        Ok(Datum::Timetz(
                            shifted.rem_euclid(86_400_000_000),
                            new_offset,
                        ))
                    }
                    other => Err(type_mismatch(name, &other)),
                }
            }
            "age" => {
                if args.len() == 1
                    && crate::sql::eval::static_type_pub(args[0], row) == Some(ColType::Xid)
                {
                    let transaction_id = match eval_full(args[0], arena, params, row, hooks)? {
                        Datum::Oid(value) => value,
                        Datum::Null => return Ok(Datum::Null),
                        other => return Err(type_mismatch(name, &other)),
                    };
                    return hooks
                        .catalog
                        .ok_or_else(|| {
                            sql_err!(
                                sqlstate::FEATURE_NOT_SUPPORTED,
                                "transaction identity access is unavailable"
                            )
                        })?
                        .transaction_id_age(transaction_id)
                        .map(Datum::Int4);
                }
                // `age(a, b)` is the symbolic interval a - b; `age(a)` measures from
                // the current date at midnight.
                if args.len() != 1 && args.len() != 2 || star {
                    return Err(sql_err!(
                        sqlstate::UNDEFINED_FUNCTION,
                        "function {}(...) with {} arguments does not exist",
                        name,
                        args.len()
                    ));
                }
                let a = eval_full(args[0], arena, params, row, hooks)?;
                if a.is_null() {
                    return Ok(Datum::Null);
                }
                let a = timestamp_micros(name, a)?;
                let b = if args.len() == 2 {
                    match eval_full(args[1], arena, params, row, hooks)? {
                        Datum::Null => return Ok(Datum::Null),
                        other => timestamp_micros(name, other)?,
                    }
                } else {
                    let day = 86_400_000_000i64;
                    datetime::now_micros().div_euclid(day) * day
                };
                if a == b
                    && matches!(
                        a,
                        datetime::TIMESTAMP_INFINITY | datetime::TIMESTAMP_NEG_INFINITY
                    )
                {
                    return Err(sql_err!(
                        sqlstate::INTERVAL_FIELD_OVERFLOW,
                        "interval out of range"
                    ));
                }
                Ok(Datum::Interval(datetime::age_between(a, b)))
            }
            "justify_hours" | "justify_days" | "justify_interval" => {
                arity(1)?;
                match eval_full(args[0], arena, params, row, hooks)? {
                    Datum::Null => Ok(Datum::Null),
                    Datum::Interval(interval) => Ok(Datum::Interval(match name {
                        "justify_hours" => datetime::justify_hours(interval),
                        "justify_days" => datetime::justify_days(interval),
                        _ => datetime::justify_interval(interval),
                    })),
                    other => Err(type_mismatch(name, &other)),
                }
            }
            // `(s1, e1) OVERLAPS (s2, e2)`: whether two half-open time periods
            // overlap, comparing in microseconds. The end of each pair may be an
            // interval (the period's length); pairs are normalized so start <= end.
            // Any NULL endpoint → NULL.
            crate::sql::parser::OVERLAPS_PERIODS => {
                arity(4)?;
                let s1 = eval_full(args[0], arena, params, row, hooks)?;
                let e1 = eval_full(args[1], arena, params, row, hooks)?;
                let s2 = eval_full(args[2], arena, params, row, hooks)?;
                let e2 = eval_full(args[3], arena, params, row, hooks)?;
                let (Some(mut a_start), Some(mut a_end)) =
                    (overlaps_micros(&s1), overlaps_end_micros(&s1, &e1))
                else {
                    return Ok(Datum::Null);
                };
                let (Some(mut b_start), Some(mut b_end)) =
                    (overlaps_micros(&s2), overlaps_end_micros(&s2, &e2))
                else {
                    return Ok(Datum::Null);
                };
                if a_start > a_end {
                    core::mem::swap(&mut a_start, &mut a_end);
                }
                if b_start > b_end {
                    core::mem::swap(&mut b_start, &mut b_end);
                }
                // Put the earlier start first; equal starts always overlap, else the
                // later start must fall before the earlier period's end.
                if a_start > b_start {
                    core::mem::swap(&mut a_start, &mut b_start);
                    core::mem::swap(&mut a_end, &mut b_end);
                }
                let result = a_start == b_start || b_start < a_end;
                Ok(Datum::Bool(result))
            }
            "extract" | "date_part" => {
                arity(2)?;
                let Some(field) = text_arg(name, args, 0, arena, params, row, hooks)? else {
                    return Ok(Datum::Null);
                };
                // A time of day has no date, so its date fields read zero and
                // only the time ones — plus, for timetz, `timezone` — apply.
                let mut zone_secs: Option<i32> = None;
                let mut instant_micros: Option<i64> = None;
                let mut time_only = false;
                let mut date_only = false;
                let value = eval_full(args[1], arena, params, row, hooks)?;
                let infinity = match value {
                    Datum::Date(datetime::DATE_INFINITY)
                    | Datum::Timestamp(datetime::TIMESTAMP_INFINITY)
                    | Datum::Timestamptz(datetime::TIMESTAMP_INFINITY) => 1,
                    Datum::Date(datetime::DATE_NEG_INFINITY)
                    | Datum::Timestamp(datetime::TIMESTAMP_NEG_INFINITY)
                    | Datum::Timestamptz(datetime::TIMESTAMP_NEG_INFINITY) => -1,
                    Datum::Interval(interval) => datetime::interval_infinity_sign(interval),
                    _ => 0,
                };
                if infinity != 0 {
                    let monotonic = [
                        "epoch",
                        "julian",
                        "year",
                        "isoyear",
                        "decade",
                        "century",
                        "millennium",
                    ]
                    .iter()
                    .any(|unit| field.eq_ignore_ascii_case(unit));
                    if !monotonic {
                        return Ok(Datum::Null);
                    }
                    return Ok(if name == "extract" {
                        Datum::Numeric(if infinity > 0 {
                            Numeric::POS_INFINITY
                        } else {
                            Numeric::NEG_INFINITY
                        })
                    } else {
                        Datum::Float8(if infinity > 0 {
                            f64::INFINITY
                        } else {
                            f64::NEG_INFINITY
                        })
                    });
                }
                let (days, in_day) = match value {
                    Datum::Null => return Ok(Datum::Null),
                    Datum::Date(d) => {
                        date_only = true;
                        (d as i64, 0i64)
                    }
                    Datum::Time(t) => {
                        time_only = true;
                        (0, t)
                    }
                    Datum::Timetz(t, zone) => {
                        time_only = true;
                        zone_secs = Some(zone);
                        (0, t)
                    }
                    Datum::Timestamp(t) => {
                        (t.div_euclid(86_400_000_000), t.rem_euclid(86_400_000_000))
                    }
                    Datum::Timestamptz(t) => {
                        let offset = session_offset(t);
                        zone_secs = Some(offset);
                        instant_micros = Some(t);
                        let local =
                            t.checked_add(i64::from(offset) * 1_000_000)
                                .ok_or_else(|| {
                                    sql_err!(
                                        sqlstate::DATETIME_FIELD_OVERFLOW,
                                        "timestamp out of range"
                                    )
                                })?;
                        (
                            local.div_euclid(86_400_000_000),
                            local.rem_euclid(86_400_000_000),
                        )
                    }
                    // Interval fields come straight from the (months, days, micros)
                    // components (PostgreSQL's interval2tm), not a calendar date.
                    Datum::Interval(interval) => {
                        return interval_extract(name == "extract", field, interval, arena);
                    }
                    other => return Err(type_mismatch(name, &other)),
                };
                use datetime::{
                    PG_EPOCH_DAYS, PG_EPOCH_SECS, civil_from_days, day_of_week, days_from_civil,
                };
                let (y, m, d) = civil_from_days(days + PG_EPOCH_DAYS);
                let (seconds, frac) = (in_day / 1_000_000, in_day % 1_000_000);
                let (h, minute, s) = (seconds / 3600, (seconds / 60) % 60, seconds % 60);
                let eq = |k: &str| field.eq_ignore_ascii_case(k);
                // `timezone` is the offset in seconds east, which only a value
                // carrying its own zone has.
                if let Some(zone) = zone_secs
                    && (eq("timezone") || eq("timezone_hour") || eq("timezone_minute"))
                {
                    let v = if eq("timezone") {
                        zone as i64
                    } else if eq("timezone_hour") {
                        (zone / 3600) as i64
                    } else {
                        ((zone % 3600) / 60) as i64
                    };
                    return Ok(if name == "extract" {
                        Datum::Numeric(crate::sql::numeric::Numeric::from_i64(v, arena)?)
                    } else {
                        Datum::Float8(v as f64)
                    });
                }
                let dow0 = day_of_week(days) as i64;
                // Integer-valued fields.
                let int_val: Option<i64> = if eq("year") || eq("years") {
                    Some(y)
                } else if eq("month") || eq("months") {
                    Some(m as i64)
                } else if eq("day") || eq("days") {
                    Some(d as i64)
                } else if eq("hour") || eq("hours") {
                    Some(h)
                } else if eq("minute") || eq("minutes") {
                    Some(minute)
                } else if eq("dow") {
                    Some(dow0)
                } else if eq("isodow") {
                    Some(if dow0 == 0 { 7 } else { dow0 })
                } else if eq("doy") {
                    Some(days_from_civil(y, m, d) - days_from_civil(y, 1, 1) + 1)
                } else if eq("quarter") {
                    Some((m as i64 - 1) / 3 + 1)
                } else if eq("decade") {
                    Some(y.div_euclid(10))
                } else if eq("century") {
                    Some(if y > 0 {
                        (y - 1) / 100 + 1
                    } else {
                        y / 100 - 1
                    })
                } else if eq("millennium") {
                    Some(if y > 0 {
                        (y - 1) / 1000 + 1
                    } else {
                        y / 1000 - 1
                    })
                } else if eq("microseconds") {
                    Some(s * 1_000_000 + frac)
                } else if eq("week") {
                    // ISO week: the week that contains this row's Thursday.
                    let isodow = if dow0 == 0 { 7 } else { dow0 };
                    let thursday = days + (4 - isodow);
                    let (ty, tm, td) = civil_from_days(thursday + PG_EPOCH_DAYS);
                    Some((days_from_civil(ty, tm, td) - days_from_civil(ty, 1, 1)) / 7 + 1)
                } else if eq("isoyear") {
                    // ISO year: the year owning the ISO week (i.e. of that Thursday).
                    let isodow = if dow0 == 0 { 7 } else { dow0 };
                    let thursday = days + (4 - isodow);
                    Some(civil_from_days(thursday + PG_EPOCH_DAYS).0)
                } else {
                    None
                };
                if let Some(interval) = int_val {
                    return Ok(if name == "extract" {
                        Datum::Numeric(Numeric::from_i64(interval, arena)?)
                    } else {
                        Datum::Float8(interval as f64)
                    });
                }
                // Fractional fields, scaled to microseconds.
                let micros_val: i64 = if eq("second")
                    || eq("seconds")
                    || eq("millisecond")
                    || eq("milliseconds")
                {
                    s * 1_000_000 + frac
                } else if eq("epoch") {
                    let value = match instant_micros {
                        Some(instant) => instant,
                        None if time_only => in_day,
                        None => days
                            .checked_mul(86_400_000_000)
                            .and_then(|date| date.checked_add(in_day))
                            .ok_or_else(|| {
                                sql_err!(
                                    sqlstate::DATETIME_FIELD_OVERFLOW,
                                    "timestamp out of range"
                                )
                            })?,
                    };
                    if time_only {
                        value
                    } else {
                        value
                            .checked_add(PG_EPOCH_SECS * 1_000_000)
                            .ok_or_else(|| {
                                sql_err!(
                                    sqlstate::DATETIME_FIELD_OVERFLOW,
                                    "timestamp out of range"
                                )
                            })?
                    }
                } else if eq("julian") && !time_only {
                    // PostgreSQL epoch 2000-01-01 is Julian day 2451545.
                    (days + 2_451_545)
                        .checked_mul(86_400_000_000)
                        .and_then(|date| date.checked_add(in_day))
                        .ok_or_else(|| {
                            sql_err!(sqlstate::DATETIME_FIELD_OVERFLOW, "timestamp out of range")
                        })?
                } else {
                    return Err(sql_err!(
                        sqlstate::FEATURE_NOT_SUPPORTED,
                        "unit \"{}\" not recognized for {}()",
                        field,
                        name
                    ));
                };
                if name == "extract" {
                    if eq("julian") {
                        let whole = micros_val.div_euclid(86_400_000_000);
                        if date_only {
                            return Ok(Datum::Numeric(Numeric::from_i64(whole, arena)?));
                        }
                        let remainder = micros_val.rem_euclid(86_400_000_000);
                        let fraction = crate::sql::numeric::div(
                            &Numeric::from_i64(remainder, arena)?,
                            &Numeric::from_i64(86_400_000_000, arena)?,
                            arena,
                        )?;
                        return Ok(Datum::Numeric(crate::sql::numeric::add(
                            &Numeric::from_i64(whole, arena)?,
                            &fraction,
                            arena,
                        )?));
                    }
                    let neg = micros_val < 0;
                    let a = micros_val.unsigned_abs();
                    let divisor = if eq("millisecond") || eq("milliseconds") {
                        1_000
                    } else if eq("julian") {
                        86_400_000_000
                    } else {
                        1_000_000
                    };
                    let decimals = if eq("millisecond") || eq("milliseconds") {
                        3
                    } else if eq("julian") {
                        20
                    } else {
                        6
                    };
                    let text = stack_format!(
                        64,
                        "{}{}.{:0width$}",
                        if neg { "-" } else { "" },
                        a / divisor,
                        a % divisor,
                        width = decimals
                    );
                    Ok(Datum::Numeric(Numeric::parse(text.as_str(), arena)?))
                } else {
                    let divisor = if eq("millisecond") || eq("milliseconds") {
                        1_000.0
                    } else if eq("julian") {
                        86_400_000_000.0
                    } else {
                        1_000_000.0
                    };
                    Ok(Datum::Float8(micros_val as f64 / divisor))
                }
            }
            "date_trunc" => {
                if !matches!(args.len(), 2 | 3) || star {
                    return Err(sql_err!(
                        sqlstate::UNDEFINED_FUNCTION,
                        "function {}(...) with {} arguments does not exist",
                        name,
                        args.len()
                    ));
                }
                let Some(field) = text_arg(name, args, 0, arena, params, row, hooks)? else {
                    return Ok(Datum::Null);
                };
                let value = eval_full(args[1], arena, params, row, hooks)?;
                if let Datum::Interval(interval) = value {
                    if args.len() != 2 {
                        return Err(sql_err!(
                            sqlstate::UNDEFINED_FUNCTION,
                            "function date_trunc(text, interval, text) does not exist"
                        ));
                    }
                    if datetime::interval_infinity_sign(interval) != 0 {
                        return Ok(Datum::Interval(interval));
                    }
                    let eq = |unit: &str| field.eq_ignore_ascii_case(unit);
                    let mut truncated = interval;
                    if eq("millennium") || eq("millennia") {
                        truncated.months = truncated.months / 12_000 * 12_000;
                        truncated.days = 0;
                        truncated.micros = 0;
                    } else if eq("century") || eq("centuries") {
                        truncated.months = truncated.months / 1_200 * 1_200;
                        truncated.days = 0;
                        truncated.micros = 0;
                    } else if eq("decade") || eq("decades") {
                        truncated.months = truncated.months / 120 * 120;
                        truncated.days = 0;
                        truncated.micros = 0;
                    } else if eq("year") || eq("years") {
                        truncated.months = truncated.months / 12 * 12;
                        truncated.days = 0;
                        truncated.micros = 0;
                    } else if eq("quarter") {
                        truncated.months = truncated.months / 3 * 3;
                        truncated.days = 0;
                        truncated.micros = 0;
                    } else if eq("month") || eq("months") {
                        truncated.days = 0;
                        truncated.micros = 0;
                    } else if eq("day") || eq("days") {
                        truncated.micros = 0;
                    } else if eq("hour") || eq("hours") {
                        truncated.micros = truncated.micros / 3_600_000_000 * 3_600_000_000;
                    } else if eq("minute") || eq("minutes") {
                        truncated.micros = truncated.micros / 60_000_000 * 60_000_000;
                    } else if eq("second") || eq("seconds") {
                        truncated.micros = truncated.micros / 1_000_000 * 1_000_000;
                    } else if eq("millisecond") || eq("milliseconds") {
                        truncated.micros = truncated.micros / 1_000 * 1_000;
                    } else if eq("microsecond") || eq("microseconds") {
                    } else {
                        return Err(sql_err!(
                            sqlstate::FEATURE_NOT_SUPPORTED,
                            "unit \"{}\" not supported for type interval",
                            field
                        ));
                    }
                    return Ok(Datum::Interval(truncated));
                }
                let zone = if args.len() == 3 {
                    let zone = eval_full(args[2], arena, params, row, hooks)?;
                    if zone.is_null() {
                        return Ok(Datum::Null);
                    }
                    timezone_from_datum(zone)?
                } else {
                    crate::sql::timezone::session()
                };
                let explicit_zone = args.len() == 3;
                let (is_tz, t) = match value {
                    Datum::Null => return Ok(Datum::Null),
                    Datum::Timestamp(t)
                        if matches!(
                            t,
                            datetime::TIMESTAMP_INFINITY | datetime::TIMESTAMP_NEG_INFINITY
                        ) =>
                    {
                        return Ok(if explicit_zone {
                            Datum::Timestamptz(t)
                        } else {
                            Datum::Timestamp(t)
                        });
                    }
                    Datum::Timestamp(t) if explicit_zone => {
                        let utc = crate::sql::timezone::session()
                            .resolve_local(t)
                            .ok_or_else(|| {
                                sql_err!(
                                    sqlstate::DATETIME_FIELD_OVERFLOW,
                                    "timestamp out of range"
                                )
                            })?;
                        let offset = i64::from(zone.resolve(utc).0) * 1_000_000;
                        (
                            true,
                            utc.checked_add(offset).ok_or_else(|| {
                                sql_err!(
                                    sqlstate::DATETIME_FIELD_OVERFLOW,
                                    "timestamp out of range"
                                )
                            })?,
                        )
                    }
                    Datum::Timestamp(t) => (false, t),
                    Datum::Timestamptz(t) => {
                        if matches!(
                            t,
                            datetime::TIMESTAMP_INFINITY | datetime::TIMESTAMP_NEG_INFINITY
                        ) {
                            return Ok(Datum::Timestamptz(t));
                        }
                        let offset = i64::from(zone.resolve(t).0) * 1_000_000;
                        (
                            true,
                            t.checked_add(offset).ok_or_else(|| {
                                sql_err!(
                                    sqlstate::DATETIME_FIELD_OVERFLOW,
                                    "timestamp out of range"
                                )
                            })?,
                        )
                    }
                    // A date promotes to timestamptz here, as PostgreSQL
                    // resolves date_trunc(text, date) through that cast.
                    Datum::Date(datetime::DATE_INFINITY) => {
                        return Ok(Datum::Timestamptz(datetime::TIMESTAMP_INFINITY));
                    }
                    Datum::Date(datetime::DATE_NEG_INFINITY) => {
                        return Ok(Datum::Timestamptz(datetime::TIMESTAMP_NEG_INFINITY));
                    }
                    Datum::Date(d) => {
                        let local = i64::from(d).checked_mul(86_400_000_000).ok_or_else(|| {
                            sql_err!(sqlstate::DATETIME_FIELD_OVERFLOW, "timestamp out of range")
                        })?;
                        if explicit_zone {
                            let utc = crate::sql::timezone::session()
                                .resolve_local(local)
                                .ok_or_else(|| {
                                    sql_err!(
                                        sqlstate::DATETIME_FIELD_OVERFLOW,
                                        "timestamp out of range"
                                    )
                                })?;
                            let offset = i64::from(zone.resolve(utc).0) * 1_000_000;
                            (
                                true,
                                utc.checked_add(offset).ok_or_else(|| {
                                    sql_err!(
                                        sqlstate::DATETIME_FIELD_OVERFLOW,
                                        "timestamp out of range"
                                    )
                                })?,
                            )
                        } else {
                            (true, local)
                        }
                    }
                    other => return Err(type_mismatch(name, &other)),
                };
                use datetime::{PG_EPOCH_DAYS, civil_from_days, day_of_week, days_from_civil};
                let (days, in_day) = (t.div_euclid(86_400_000_000), t.rem_euclid(86_400_000_000));
                let (y, m, _d) = civil_from_days(days + PG_EPOCH_DAYS);
                let (seconds, _frac) = (in_day / 1_000_000, in_day % 1_000_000);
                let (h, minute, s) = (seconds / 3600, (seconds / 60) % 60, seconds % 60);
                let eq = |k: &str| field.eq_ignore_ascii_case(k);
                // (new day count since epoch, seconds within the day).
                let (new_days, sod, micros_remainder): (i64, i64, i64) =
                    if eq("millennium") || eq("millennia") {
                        let first = if y > 0 {
                            (y - 1) / 1_000 * 1_000 + 1
                        } else {
                            y / 1_000 * 1_000
                        };
                        (days_from_civil(first, 1, 1) - PG_EPOCH_DAYS, 0, 0)
                    } else if eq("century") || eq("centuries") {
                        let first = if y > 0 {
                            (y - 1) / 100 * 100 + 1
                        } else {
                            y / 100 * 100
                        };
                        (days_from_civil(first, 1, 1) - PG_EPOCH_DAYS, 0, 0)
                    } else if eq("decade") || eq("decades") {
                        let first = y / 10 * 10;
                        (days_from_civil(first, 1, 1) - PG_EPOCH_DAYS, 0, 0)
                    } else if eq("year") || eq("years") {
                        (days_from_civil(y, 1, 1) - PG_EPOCH_DAYS, 0, 0)
                    } else if eq("quarter") {
                        (
                            days_from_civil(y, ((m - 1) / 3) * 3 + 1, 1) - PG_EPOCH_DAYS,
                            0,
                            0,
                        )
                    } else if eq("month") || eq("months") {
                        (days_from_civil(y, m, 1) - PG_EPOCH_DAYS, 0, 0)
                    } else if eq("week") {
                        let dow0 = day_of_week(days) as i64;
                        let isodow = if dow0 == 0 { 7 } else { dow0 };
                        (days - (isodow - 1), 0, 0)
                    } else if eq("day") || eq("days") {
                        (days, 0, 0)
                    } else if eq("hour") || eq("hours") {
                        (days, h * 3600, 0)
                    } else if eq("minute") || eq("minutes") {
                        (days, h * 3600 + minute * 60, 0)
                    } else if eq("second") || eq("seconds") {
                        (days, h * 3600 + minute * 60 + s, 0)
                    } else if eq("millisecond") || eq("milliseconds") {
                        (
                            days,
                            h * 3600 + minute * 60 + s,
                            in_day % 1_000_000 / 1_000 * 1_000,
                        )
                    } else if eq("microsecond") || eq("microseconds") {
                        (days, h * 3600 + minute * 60 + s, in_day % 1_000_000)
                    } else {
                        return Err(sql_err!(
                            sqlstate::FEATURE_NOT_SUPPORTED,
                            "unit \"{}\" not recognized for date_trunc()",
                            field
                        ));
                    };
                let micros = new_days
                    .checked_mul(86_400_000_000)
                    .and_then(|date| {
                        sod.checked_mul(1_000_000)
                            .and_then(|time| time.checked_add(micros_remainder))
                            .and_then(|time| date.checked_add(time))
                    })
                    .ok_or_else(|| {
                        sql_err!(sqlstate::DATETIME_FIELD_OVERFLOW, "timestamp out of range")
                    })?;
                Ok(if is_tz {
                    Datum::Timestamptz(zone.resolve_local(micros).ok_or_else(|| {
                        sql_err!(sqlstate::DATETIME_FIELD_OVERFLOW, "timestamp out of range")
                    })?)
                } else {
                    Datum::Timestamp(micros)
                })
            }
            _ => unreachable!("dispatch guard admitted an unhandled name"),
        }
    })())
}
