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
    cast_to, datum_numeric, eval_full, int_arg, interval_extract, num_f64, num_factor,
    overlaps_end_micros, overlaps_micros, sqlstate, text_arg, timestamp_micros, type_mismatch,
    ColumnLookup, EvalHooks, SqlError,
};

/// The session zone's offset (seconds east) at an instant.
fn session_offset(utc_micros: i64) -> i32 {
    crate::sql::timezone::session().resolve(utc_micros).0
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
    let p = int_arg(name, args, 0, arena, params, row, hooks)?.unwrap_or(6).clamp(0, 6);
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
            | "overlaps"
            | "extract"
            | "date_part"
            | "date_trunc"
    ) {
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
            "now" | "current_timestamp" | "transaction_timestamp" | "statement_timestamp"
            | "clock_timestamp" | "localtimestamp" => {
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
            // `date_bin(stride, source, origin)`: the stride-aligned bucket start at
            // or before `source`, measured from `origin`. Strides with a month or
            // year component are rejected, as in PostgreSQL.
            "date_bin" => {
                arity(3)?;
                // The stride is an interval — coerce a bare string literal.
                let stride = match cast_to(eval_full(args[0], arena, params, row, hooks)?, ColType::Interval, arena)? {
                    Datum::Interval(iv) => iv,
                    _ => return Ok(Datum::Null),
                };
                let source = eval_full(args[1], arena, params, row, hooks)?;
                let origin = eval_full(args[2], arena, params, row, hooks)?;
                let (source_micros, tz) = match source {
                    Datum::Timestamp(v) => (v, false),
                    Datum::Timestamptz(v) => (v, true),
                    Datum::Null => return Ok(Datum::Null),
                    other => return Err(type_mismatch("date_bin source must be a timestamp", &other)),
                };
                let origin_micros = match origin {
                    Datum::Timestamp(v) | Datum::Timestamptz(v) => v,
                    Datum::Null => return Ok(Datum::Null),
                    other => return Err(type_mismatch("date_bin origin must be a timestamp", &other)),
                };
                if stride.months != 0 {
                    return Err(sql_err!(
                        "0A000",
                        "timestamps cannot be binned into intervals containing months or years"
                    ));
                }
                let stride_micros = (stride.days as i64) * 86_400_000_000 + stride.micros;
                if stride_micros <= 0 {
                    return Err(sql_err!("22008", "stride must be greater than zero"));
                }
                let delta = source_micros - origin_micros;
                // Floor-division so the bucket start is at or before the source.
                let binned = origin_micros + delta.div_euclid(stride_micros) * stride_micros;
                Ok(if tz { Datum::Timestamptz(binned) } else { Datum::Timestamp(binned) })
            }
            // `isfinite`: always true — no infinite date/timestamp/interval exists.
            "isfinite" => {
                arity(1)?;
                match eval_full(args[0], arena, params, row, hooks)? {
                    Datum::Null => Ok(Datum::Null),
                    Datum::Date(_) | Datum::Timestamp(_) | Datum::Timestamptz(_)
                    | Datum::Interval(_) => Ok(Datum::Bool(true)),
                    other => Err(type_mismatch("isfinite requires a date/time/interval", &other)),
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
                let Datum::Text(fmt) = f else {
                    return Err(type_mismatch(name, &f));
                };
                // Temporal values format via the date/time codes; numeric values via
                // the number codes.
                let micros = match v {
                    Datum::Timestamp(t) | Datum::Timestamptz(t) => Some(t),
                    Datum::Date(d) => Some(d as i64 * 86_400_000_000),
                    Datum::Time(t) => Some(t),
                    Datum::Timetz(t, _) => Some(t),
                    _ => None,
                };
                if let Some(m) = micros {
                    return Ok(Datum::Text(to_char::timestamp(m, fmt, arena)?));
                }
                // A float8 input keeps its own sign bit even when the value rounds
                // to zero (covers -0.0 and small negatives) — PostgreSQL behavior.
                let float_negative = matches!(v, Datum::Float8(x) if x.is_sign_negative());
                let float_source = if let Datum::Float8(x) = v { Some(x) } else { None };
                // NaN/Infinity have no numeric form; the formatter reads them
                // from `float_source` (and fills with `#`, as PostgreSQL).
                let n = match float_source {
                    Some(x) if !x.is_finite() => Numeric::parse("0", arena)?,
                    _ => datum_numeric(name, v, arena)?,
                };
                Ok(Datum::Text(to_char::number(&n, fmt, float_negative, float_source, arena)?))
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
                let want = if name == "make_timestamp" || name == "make_timestamptz" { 6 } else { 3 };
                arity(want)?;
                // The seconds field is a double; every other field is an integer.
                let sec_idx = if name == "make_date" { usize::MAX } else { want - 1 };
                let mut ints = [0i64; 6];
                for (i, slot) in ints[..want].iter_mut().enumerate() {
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
                    "make_date" => {
                        Ok(Datum::Date(datetime::make_date(ints[0], ints[1], ints[2])?))
                    }
                    "make_time" => {
                        Ok(Datum::Time(datetime::make_time(ints[0], ints[1], sec)?))
                    }
                    "make_timestamptz" => Ok(Datum::Timestamptz(datetime::make_timestamp(
                        ints[0], ints[1], ints[2], ints[3], ints[4], sec,
                    )?)),
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
                    return Err(sql_err!("22008", "interval field value out of range"));
                };
                let sec_micros = (secs * 1_000_000.0).round();
                let micros = ints[4]
                    .checked_mul(3_600_000_000)
                    .and_then(|h| ints[5].checked_mul(60_000_000).and_then(|m| h.checked_add(m)))
                    .filter(|_| sec_micros.is_finite() && sec_micros.abs() < 9.2e18)
                    .and_then(|hm| hm.checked_add(sec_micros as i64));
                let Some(micros) = micros else {
                    return Err(sql_err!("22008", "interval field value out of range"));
                };
                Ok(Datum::Interval(Interval { months, days, micros }))
            }
            "timezone" => {
                // `timezone(zone, ts)` == `ts AT TIME ZONE zone`. A plain timestamp
                // is read as wall-clock time in `zone` and becomes the timestamptz
                // instant; a timestamptz instant becomes the wall-clock timestamp in
                // `zone`. The zone's offset can shift with DST, so it is resolved at
                // the relevant instant.
                arity(2)?;
                let Some(zone_name) = text_arg(name, args, 0, arena, params, row, hooks)? else {
                    return Ok(Datum::Null);
                };
                let zone = guc::parse_timezone(zone_name).ok_or_else(|| sql_err!("22023", "time zone \"{}\" not recognized", zone_name))?;
                match eval_full(args[1], arena, params, row, hooks)? {
                    Datum::Null => Ok(Datum::Null),
                    Datum::Timestamptz(utc) => {
                        let (offset_seconds, _) = zone.resolve(utc);
                        Ok(Datum::Timestamp(utc + i64::from(offset_seconds) * 1_000_000))
                    }
                    Datum::Timestamp(wall_clock) => {
                        // Resolve the offset at the wall-clock instant (exact away
                        // from the sub-hour DST transition windows).
                        let (offset_seconds, _) = zone.resolve(wall_clock);
                        Ok(Datum::Timestamptz(wall_clock - i64::from(offset_seconds) * 1_000_000))
                    }
                    other => Err(type_mismatch(name, &other)),
                }
            }
            "age" => {
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
            "overlaps" => {
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
                let (days, in_day) = match eval_full(args[1], arena, params, row, hooks)? {
                    Datum::Null => return Ok(Datum::Null),
                    Datum::Date(d) => (d as i64, 0i64),
                    Datum::Time(t) => (0, t),
                    Datum::Timetz(t, zone) => {
                        zone_secs = Some(zone);
                        (0, t)
                    }
                    Datum::Timestamp(t) | Datum::Timestamptz(t) => {
                        (t.div_euclid(86_400_000_000), t.rem_euclid(86_400_000_000))
                    }
                    // Interval fields come straight from the (months, days, micros)
                    // components (PostgreSQL's interval2tm), not a calendar date.
                    Datum::Interval(interval) => {
                        return interval_extract(name == "extract", field, interval, arena)
                    }
                    other => return Err(type_mismatch(name, &other)),
                };
                use datetime::{civil_from_days, day_of_week, days_from_civil, PG_EPOCH_DAYS, PG_EPOCH_SECS};
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
                    Some(if y > 0 { (y - 1) / 100 + 1 } else { y / 100 - 1 })
                } else if eq("millennium") {
                    Some(if y > 0 { (y - 1) / 1000 + 1 } else { y / 1000 - 1 })
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
                let micros_val: i64 = if eq("second") || eq("seconds") {
                    s * 1_000_000 + frac
                } else if eq("epoch") {
                    (days * 86_400_000_000 + in_day) + PG_EPOCH_SECS * 1_000_000
                } else {
                    return Err(sql_err!(
                        sqlstate::FEATURE_NOT_SUPPORTED,
                        "unit \"{}\" not recognized for {}()",
                        field,
                        name
                    ));
                };
                if name == "extract" {
                    let neg = micros_val < 0;
                    let a = micros_val.unsigned_abs();
                    let text = stack_format!(
                        40,
                        "{}{}.{:06}",
                        if neg { "-" } else { "" },
                        a / 1_000_000,
                        a % 1_000_000
                    );
                    Ok(Datum::Numeric(Numeric::parse(text.as_str(), arena)?))
                } else {
                    Ok(Datum::Float8(micros_val as f64 / 1_000_000.0))
                }
            }
            "date_trunc" => {
                arity(2)?;
                let Some(field) = text_arg(name, args, 0, arena, params, row, hooks)? else {
                    return Ok(Datum::Null);
                };
                let (is_tz, t) = match eval_full(args[1], arena, params, row, hooks)? {
                    Datum::Null => return Ok(Datum::Null),
                    Datum::Timestamp(t) => (false, t),
                    Datum::Timestamptz(t) => (true, t),
                    Datum::Date(d) => (false, d as i64 * 86_400_000_000),
                    other => return Err(type_mismatch(name, &other)),
                };
                use datetime::{civil_from_days, day_of_week, days_from_civil, PG_EPOCH_DAYS};
                let (days, in_day) = (t.div_euclid(86_400_000_000), t.rem_euclid(86_400_000_000));
                let (y, m, _d) = civil_from_days(days + PG_EPOCH_DAYS);
                let (seconds, _frac) = (in_day / 1_000_000, in_day % 1_000_000);
                let (h, minute, s) = (seconds / 3600, (seconds / 60) % 60, seconds % 60);
                let eq = |k: &str| field.eq_ignore_ascii_case(k);
                // (new day count since epoch, seconds within the day).
                let (new_days, sod): (i64, i64) = if eq("year") || eq("years") {
                    (days_from_civil(y, 1, 1) - PG_EPOCH_DAYS, 0)
                } else if eq("quarter") {
                    (days_from_civil(y, ((m - 1) / 3) * 3 + 1, 1) - PG_EPOCH_DAYS, 0)
                } else if eq("month") || eq("months") {
                    (days_from_civil(y, m, 1) - PG_EPOCH_DAYS, 0)
                } else if eq("week") {
                    let dow0 = day_of_week(days) as i64;
                    let isodow = if dow0 == 0 { 7 } else { dow0 };
                    (days - (isodow - 1), 0)
                } else if eq("day") || eq("days") {
                    (days, 0)
                } else if eq("hour") || eq("hours") {
                    (days, h * 3600)
                } else if eq("minute") || eq("minutes") {
                    (days, h * 3600 + minute * 60)
                } else if eq("second") || eq("seconds") {
                    (days, h * 3600 + minute * 60 + s)
                } else {
                    return Err(sql_err!(
                        sqlstate::FEATURE_NOT_SUPPORTED,
                        "unit \"{}\" not recognized for date_trunc()",
                        field
                    ));
                };
                let micros = new_days * 86_400_000_000 + sod * 1_000_000;
                Ok(if is_tz {
                    Datum::Timestamptz(micros)
                } else {
                    Datum::Timestamp(micros)
                })
            }
            _ => unreachable!("dispatch guard admitted an unhandled name"),
        }
    })())
}
