//! Date/time storage and text I/O.
//!
//! Storage matches PostgreSQL's on-disk convention: dates are days since
//! 2000-01-01, timestamps are microseconds since 2000-01-01 00:00:00.
//! Civil-date math is Howard Hinnant's public-domain algorithms
//! (<https://howardhinnant.github.io/date_algorithms.html>). The session
//! time zone is fixed at UTC.

use crate::sql_err;
use crate::util::StackStr;

use super::eval::SqlError;

/// Days between 1970-01-01 and 2000-01-01.
pub const PG_EPOCH_DAYS: i64 = 10_957;
/// Seconds between the unix and PostgreSQL epochs.
pub const PG_EPOCH_SECS: i64 = 946_684_800;

pub fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

pub fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y.rem_euclid(400);
    let mp = if m > 2 { m - 3 } else { m + 9 } as i64;
    let doy = (153 * mp + 2) / 5 + d as i64 - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

fn days_in_month(y: i64, m: u32) -> u32 {
    match m {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 => {
            if (y % 4 == 0 && y % 100 != 0) || y % 400 == 0 {
                29
            } else {
                28
            }
        }
        _ => 0,
    }
}

/// Parses `YYYY-MM-DD` into days since 2000-01-01. Malformed input is
/// 22007 (invalid_datetime_format); a well-formed but impossible date is
/// 22008 (datetime_field_overflow), matching PostgreSQL.
pub fn parse_date(s: &str) -> Result<i32, SqlError> {
    let bad = || {
        sql_err!(
            "22007",
            "invalid input syntax for type date: \"{}\"",
            s
        )
    };
    let out_of_range = || {
        sql_err!(
            "22008",
            "date/time field value out of range: \"{}\"",
            s
        )
    };
    let t = s.trim();
    let mut parts = t.splitn(3, '-');
    let (y, m, d) = (
        parts.next().and_then(|p| p.parse::<i64>().ok()).ok_or_else(bad)?,
        parts.next().and_then(|p| p.parse::<u32>().ok()).ok_or_else(bad)?,
        parts.next().and_then(|p| p.parse::<u32>().ok()).ok_or_else(bad)?,
    );
    if !(1..=12).contains(&m) || d < 1 || d > days_in_month(y, m) {
        return Err(out_of_range());
    }
    let days = days_from_civil(y, m, d) - PG_EPOCH_DAYS;
    i32::try_from(days).map_err(|_| out_of_range())
}

const MONTH_ABBR: [&str; 12] = [
    "jan", "feb", "mar", "apr", "may", "jun", "jul", "aug", "sep", "oct", "nov", "dec",
];
const MONTH_FULL: [&str; 12] = [
    "january", "february", "march", "april", "may", "june", "july", "august", "september",
    "october", "november", "december",
];

/// Parses `input` guided by a `to_date`/`to_timestamp` format string into
/// `(year, month, day, hour, minute, second)`. Supports the common field codes
/// (`YYYY`/`YYY`/`YY`/`Y`, `MM`, `DD`, `HH24`/`HH12`/`HH`, `MI`, `SS`,
/// `MON`/`MONTH`) with any non-code characters treated as skippable separators;
/// unrecognized letter codes are rejected loudly.
pub fn parse_formatted(input: &str, fmt: &str) -> Result<(i64, u32, u32, i64, i64, i64), SqlError> {
    let bad = || sql_err!("22007", "invalid value for input string");
    let (mut y, mut month, mut d, mut h, mut mi, mut s) = (2000i64, 1u32, 1u32, 0i64, 0i64, 0i64);
    let ib = input.as_bytes();
    let fb = fmt.as_bytes();
    let mut ip = 0usize;
    let mut fi = 0usize;
    // Reads up to `width` decimal digits (skipping leading spaces) into an int.
    let read_num = |ip: &mut usize, width: usize| -> Option<i64> {
        while *ip < ib.len() && ib[*ip] == b' ' {
            *ip += 1;
        }
        let start = *ip;
        let mut v: i64 = 0;
        while *ip < ib.len() && *ip - start < width && ib[*ip].is_ascii_digit() {
            v = v * 10 + (ib[*ip] - b'0') as i64;
            *ip += 1;
        }
        if *ip == start { None } else { Some(v) }
    };
    let starts_with_ci = |bytes: &[u8], at: usize, word: &[u8]| -> bool {
        at + word.len() <= bytes.len()
            && bytes[at..at + word.len()].eq_ignore_ascii_case(word)
    };
    while fi < fb.len() {
        let up = fb[fi].to_ascii_uppercase();
        // Longest field codes first.
        if starts_with_ci(fb, fi, b"HH24") || starts_with_ci(fb, fi, b"HH12") {
            h = read_num(&mut ip, 2).ok_or_else(bad)?;
            fi += 4;
        } else if starts_with_ci(fb, fi, b"YYYY") {
            y = read_num(&mut ip, 4).ok_or_else(bad)?;
            fi += 4;
        } else if starts_with_ci(fb, fi, b"MONTH") {
            month = read_month(input, &mut ip, false).ok_or_else(bad)?;
            fi += 5;
        } else if starts_with_ci(fb, fi, b"MON") {
            month = read_month(input, &mut ip, true).ok_or_else(bad)?;
            fi += 3;
        } else if starts_with_ci(fb, fi, b"YYY") {
            y = read_num(&mut ip, 3).ok_or_else(bad)?;
            fi += 3;
        } else if up == b'H' && starts_with_ci(fb, fi, b"HH") {
            h = read_num(&mut ip, 2).ok_or_else(bad)?;
            fi += 2;
        } else if starts_with_ci(fb, fi, b"YY") {
            let v = read_num(&mut ip, 2).ok_or_else(bad)?;
            y = if v < 70 { 2000 + v } else { 1900 + v };
            fi += 2;
        } else if starts_with_ci(fb, fi, b"MM") {
            month = read_num(&mut ip, 2).ok_or_else(bad)? as u32;
            fi += 2;
        } else if starts_with_ci(fb, fi, b"DD") {
            d = read_num(&mut ip, 2).ok_or_else(bad)? as u32;
            fi += 2;
        } else if starts_with_ci(fb, fi, b"MI") {
            mi = read_num(&mut ip, 2).ok_or_else(bad)?;
            fi += 2;
        } else if starts_with_ci(fb, fi, b"SS") {
            s = read_num(&mut ip, 2).ok_or_else(bad)?;
            fi += 2;
        } else if up == b'Y' {
            y = read_num(&mut ip, 1).ok_or_else(bad)?;
            fi += 1;
        } else if up.is_ascii_alphabetic() {
            return Err(sql_err!("22007", "unsupported to_date/to_timestamp code"));
        } else {
            // Separator: skip one non-alphanumeric input character if present.
            if ip < ib.len() && !ib[ip].is_ascii_alphanumeric() {
                ip += 1;
            }
            fi += 1;
        }
    }
    if !(1..=12).contains(&month) || d < 1 || d > days_in_month(y, month) {
        return Err(sql_err!("22008", "date/time field value out of range"));
    }
    Ok((y, month, d, h, mi, s))
}

/// Reads a month name (abbreviated when `abbr`, else full) at `*ip`, returning
/// the 1-based month.
fn read_month(input: &str, ip: &mut usize, abbr: bool) -> Option<u32> {
    let bytes = input.as_bytes();
    while *ip < bytes.len() && bytes[*ip] == b' ' {
        *ip += 1;
    }
    let table: &[&str] = if abbr { &MONTH_ABBR } else { &MONTH_FULL };
    for (i, name) in table.iter().enumerate() {
        let nb = name.as_bytes();
        if *ip + nb.len() <= bytes.len() && bytes[*ip..*ip + nb.len()].eq_ignore_ascii_case(nb) {
            *ip += nb.len();
            return Some(i as u32 + 1);
        }
    }
    // `MON` also accepts the full name; `MONTH` also accepts the abbreviation.
    let other: &[&str] = if abbr { &MONTH_FULL } else { &MONTH_ABBR };
    for (i, name) in other.iter().enumerate() {
        let nb = name.as_bytes();
        if *ip + nb.len() <= bytes.len() && bytes[*ip..*ip + nb.len()].eq_ignore_ascii_case(nb) {
            *ip += nb.len();
            return Some(i as u32 + 1);
        }
    }
    None
}

/// `to_date`: parses a formatted date into days since 2000-01-01.
pub fn to_date(input: &str, fmt: &str) -> Result<i32, SqlError> {
    let (y, month, d, _, _, _) = parse_formatted(input, fmt)?;
    make_date(y, month as i64, d as i64)
}

/// `to_timestamp`: parses a formatted timestamp into microseconds since
/// 2000-01-01.
pub fn to_timestamp(input: &str, fmt: &str) -> Result<i64, SqlError> {
    let (y, month, d, h, mi, s) = parse_formatted(input, fmt)?;
    make_timestamp(y, month as i64, d as i64, h, mi, s as f64)
}

/// Constructs a date (days since 2000-01-01) from year/month/day, validating
/// the fields as PostgreSQL `make_date` does.
pub fn make_date(y: i64, m: i64, d: i64) -> Result<i32, SqlError> {
    let range = || sql_err!("22008", "date field value out of range");
    if !(1..=12).contains(&m) {
        return Err(range());
    }
    let (mu, du) = (m as u32, d);
    if du < 1 || du as u32 > days_in_month(y, mu) {
        return Err(range());
    }
    let days = days_from_civil(y, mu, du as u32) - PG_EPOCH_DAYS;
    i32::try_from(days).map_err(|_| range())
}

/// Constructs a time-of-day (microseconds since midnight) from hour/minute and
/// a fractional second, validating fields as PostgreSQL `make_time` does.
pub fn make_time(h: i64, mi: i64, sec: f64) -> Result<i64, SqlError> {
    let range = || sql_err!("22008", "time field value out of range");
    if !(0..=23).contains(&h) || !(0..=59).contains(&mi) || !(0.0..60.0).contains(&sec) {
        return Err(range());
    }
    Ok(((h * 60 + mi) * 60) * 1_000_000 + (sec * 1_000_000.0).round() as i64)
}

/// Constructs a timestamp (microseconds since 2000-01-01) from its fields.
pub fn make_timestamp(y: i64, m: i64, d: i64, h: i64, mi: i64, sec: f64) -> Result<i64, SqlError> {
    let days = make_date(y, m, d)? as i64;
    let time_of_day = make_time(h, mi, sec)?;
    Ok(days * 86_400_000_000 + time_of_day)
}

/// Parses `YYYY-MM-DD[ |T]HH:MM[:SS[.ffffff]][Z|±HH[:MM]]` into
/// microseconds since 2000-01-01 UTC. `require_tz_shift` applies the zone
/// offset (timestamptz); plain timestamp ignores any suffix.
pub fn parse_timestamp(s: &str, apply_timezone: bool) -> Result<i64, SqlError> {
    let bad = || {
        sql_err!(
            "22007",
            "invalid input syntax for type timestamp: \"{}\"",
            s
        )
    };
    let t = s.trim();
    // Split date and time parts.
    let (date_part, rest) = match t.find([' ', 'T']) {
        Some(i) => (&t[..i], &t[i + 1..]),
        None => (t, ""),
    };
    let date_days = parse_date(date_part)? as i64;

    if rest.is_empty() {
        return Ok(date_days * 86_400 * 1_000_000);
    }

    // Trailing zone: Z, +HH, +HH:MM, -HH, -HH:MM.
    let (time_part, timezone_seconds) = if let Some(stripped) = rest.strip_suffix('Z') {
        (stripped, 0i64)
    } else if let Some(pos) = rest.rfind(['+', '-']) {
        if pos > 0 {
            let (tp, zone) = rest.split_at(pos);
            let sign: i64 = if zone.starts_with('-') { -1 } else { 1 };
            let z = &zone[1..];
            let (h, m) = match z.split_once(':') {
                Some((h, m)) => (
                    h.parse::<i64>().map_err(|_| bad())?,
                    m.parse::<i64>().map_err(|_| bad())?,
                ),
                None => (z.parse::<i64>().map_err(|_| bad())?, 0),
            };
            (tp, sign * (h * 3600 + m * 60))
        } else {
            (rest, 0)
        }
    } else {
        (rest, 0)
    };

    let mut it = time_part.splitn(3, ':');
    let h: i64 = it.next().and_then(|p| p.parse().ok()).ok_or_else(bad)?;
    let m: i64 = it.next().and_then(|p| p.parse().ok()).ok_or_else(bad)?;
    let (sec, micros) = match it.next() {
        None => (0i64, 0i64),
        Some(sec_part) => match sec_part.split_once('.') {
            None => (sec_part.parse().map_err(|_| bad())?, 0),
            Some((sp, fp)) => {
                let sec = sp.parse().map_err(|_| bad())?;
                let mut micros = 0i64;
                let mut scale = 100_000i64;
                for c in fp.chars().take(6) {
                    let d = c.to_digit(10).ok_or_else(bad)? as i64;
                    micros += d * scale;
                    scale /= 10;
                }
                (sec, micros)
            }
        },
    };
    if !(0..24).contains(&h) || !(0..60).contains(&m) || !(0..61).contains(&sec) {
        return Err(sql_err!(
            "22008",
            "date/time field value out of range: \"{}\"",
            s
        ));
    }
    let mut total =
        date_days * 86_400_000_000 + (h * 3600 + m * 60 + sec) * 1_000_000 + micros;
    if apply_timezone {
        total -= timezone_seconds * 1_000_000;
    }
    Ok(total)
}

/// Parses a `time` value (`HH:MM[:SS[.ffffff]]`) into microseconds since
/// midnight. A trailing zone is ignored (we model `time`, not `timetz`).
pub fn parse_time(s: &str) -> Result<i64, SqlError> {
    let bad = || sql_err!("22007", "invalid input syntax for type time: \"{}\"", s);
    let t = s.trim();
    // Drop any zone suffix.
    let t = match t.find(['+', 'Z']) {
        Some(i) if i > 0 => &t[..i],
        _ => t,
    };
    let t = t.trim();
    let mut it = t.splitn(3, ':');
    let h: i64 = it.next().and_then(|p| p.trim().parse().ok()).ok_or_else(bad)?;
    let m: i64 = it.next().and_then(|p| p.parse().ok()).ok_or_else(bad)?;
    let (sec, micros) = match it.next() {
        None => (0i64, 0i64),
        Some(sec_part) => match sec_part.split_once('.') {
            None => (sec_part.parse().map_err(|_| bad())?, 0),
            Some((sp, fp)) => {
                let sec = sp.parse().map_err(|_| bad())?;
                let mut micros = 0i64;
                let mut scale = 100_000i64;
                for c in fp.chars().take(6) {
                    micros += c.to_digit(10).ok_or_else(bad)? as i64 * scale;
                    scale /= 10;
                }
                (sec, micros)
            }
        },
    };
    if !(0..24).contains(&h) || !(0..60).contains(&m) || !(0..61).contains(&sec) {
        return Err(sql_err!("22008", "date/time field value out of range: \"{}\"", s));
    }
    Ok((h * 3600 + m * 60 + sec) * 1_000_000 + micros)
}

/// Formats microseconds since midnight as `HH:MM:SS[.ffffff]` (PostgreSQL
/// trims trailing zeros in the fractional part, omitting it entirely if zero).
pub fn format_time(micros: i64) -> StackStr<24> {
    use core::fmt::Write;
    let mut out = StackStr::<24>::new();
    let seconds = micros.div_euclid(1_000_000);
    let frac = micros.rem_euclid(1_000_000);
    let (h, m, s) = (seconds / 3600, (seconds % 3600) / 60, seconds % 60);
    let _ = write!(out, "{h:02}:{m:02}:{s:02}");
    if frac != 0 {
        let mut f = frac;
        let mut digits = [0u8; 6];
        for d in digits.iter_mut().rev() {
            *d = (f % 10) as u8;
            f /= 10;
        }
        let mut len = 6;
        while len > 0 && digits[len - 1] == 0 {
            len -= 1;
        }
        let _ = write!(out, ".");
        for d in &digits[..len] {
            let _ = write!(out, "{d}");
        }
    }
    out
}

/// Parses an `interval` in PostgreSQL's verbose form (`1 year 2 months`,
/// `90 minutes`, `-5 days`, `1 day 03:04:05`). Returns (months, days, micros).
pub fn parse_interval(s: &str) -> Result<super::types::Interval, SqlError> {
    use super::types::Interval;
    let bad = || sql_err!("22007", "invalid input syntax for type interval: \"{}\"", s);
    let mut months = 0i64;
    let mut days = 0i64;
    let mut micros = 0i64;
    let mut it = s.split_whitespace().peekable();
    let mut saw = false;
    while let Some(tok) = it.next() {
        if tok.contains(':') {
            // A bare clock time contributes to the microseconds field.
            let neg = tok.starts_with('-');
            let t = tok.trim_start_matches(['-', '+']);
            micros += if neg { -parse_time(t)? } else { parse_time(t)? };
            saw = true;
            continue;
        }
        // A signed number followed by a unit word.
        let n: f64 = tok.parse().map_err(|_| bad())?;
        let unit = it.next().ok_or_else(bad)?;
        let u = unit.trim_end_matches('s'); // singular/plural
        match u {
            "year" | "yr" => months += (n * 12.0) as i64,
            "month" | "mon" => months += n as i64,
            "week" | "wk" => days += (n * 7.0) as i64,
            "day" | "d" => days += n as i64,
            "hour" | "hr" | "h" => micros += (n * 3_600_000_000.0) as i64,
            "minute" | "min" | "m" => micros += (n * 60_000_000.0) as i64,
            "second" | "sec" | "s" => micros += (n * 1_000_000.0) as i64,
            "millisecond" | "msec" | "ms" => micros += (n * 1_000.0) as i64,
            "microsecond" | "usec" | "us" => micros += n as i64,
            _ => return Err(bad()),
        }
        saw = true;
    }
    if !saw {
        return Err(bad());
    }
    Ok(Interval {
        months: months as i32,
        days: days as i32,
        micros,
    })
}

/// Formats an `interval` exactly as PostgreSQL's default (postgres) style does:
/// nonzero year/month/day fields with units, then a signed `HH:MM:SS[.ffffff]`
/// time part (shown when microseconds are nonzero, or when the whole interval
/// is zero).
pub fn format_interval(interval: super::types::Interval) -> StackStr<48> {
    use core::fmt::Write;
    let mut out = StackStr::<48>::new();
    let years = interval.months / 12;
    let mons = interval.months % 12;
    let mut first = true;
    let sep = |out: &mut StackStr<48>, first: &mut bool| {
        if !*first {
            let _ = write!(out, " ");
        }
        *first = false;
    };
    let unit = |out: &mut StackStr<48>, first: &mut bool, n: i32, singular: &str| {
        if n != 0 {
            sep(out, first);
            let _ = write!(out, "{n} {singular}");
            if n != 1 {
                let _ = write!(out, "s");
            }
        }
    };
    unit(&mut out, &mut first, years, "year");
    unit(&mut out, &mut first, mons, "mon");
    unit(&mut out, &mut first, interval.days, "day");
    if interval.micros != 0 || (interval.months == 0 && interval.days == 0) {
        sep(&mut out, &mut first);
        let neg = interval.micros < 0;
        let a = interval.micros.unsigned_abs() as i64;
        let seconds = a / 1_000_000;
        let frac = a % 1_000_000;
        let (h, m, s) = (seconds / 3600, (seconds % 3600) / 60, seconds % 60);
        if neg {
            let _ = write!(out, "-");
        }
        let _ = write!(out, "{h:02}:{m:02}:{s:02}");
        if frac != 0 {
            let mut f = frac;
            let mut digits = [0u8; 6];
            for d in digits.iter_mut().rev() {
                *d = (f % 10) as u8;
                f /= 10;
            }
            let mut len = 6;
            while len > 0 && digits[len - 1] == 0 {
                len -= 1;
            }
            let _ = write!(out, ".");
            for d in &digits[..len] {
                let _ = write!(out, "{d}");
            }
        }
    }
    out
}

/// Adds an interval to a timestamp/microsecond instant: months advance the
/// calendar (clamping the day into the target month), days are 24h each, and
/// microseconds add directly.
pub fn add_interval(micros_epoch: i64, interval: super::types::Interval) -> i64 {
    let mut m = micros_epoch;
    if interval.months != 0 {
        // Break into date + time-of-day, advance the calendar month, clamp day.
        let days = m.div_euclid(DAY_US);
        let time_of_day = m.rem_euclid(DAY_US);
        let (y, month, d) = civil_from_days(days + PG_EPOCH_DAYS);
        let total = y * 12 + (month as i64 - 1) + interval.months as i64;
        let new_year = total.div_euclid(12);
        let new_month = (total.rem_euclid(12) + 1) as u32;
        let days_in_month_count = days_in_month(new_year, new_month);
        let new_day = d.min(days_in_month_count);
        let new_days = days_from_civil(new_year, new_month, new_day) - PG_EPOCH_DAYS;
        m = new_days * DAY_US + time_of_day;
    }
    m + interval.days as i64 * DAY_US + interval.micros
}

/// `interval * factor` (and `interval / factor` when `div`), matching
/// PostgreSQL's `interval_mul`/`interval_div`: a fractional number of months
/// spills into days (30-day months) and a fractional number of days spills into
/// the time field.
pub fn interval_scale(interval: super::types::Interval, factor: f64, div: bool) -> super::types::Interval {
    let f = if div { 1.0 / factor } else { factor };
    const DAYS_PER_MONTH: f64 = 30.0;
    let month_double = interval.months as f64 * f;
    let months = month_double as i32;
    let month_remainder_days = (month_double - months as f64) * DAYS_PER_MONTH;
    let day_double = interval.days as f64 * f;
    let days_whole = day_double as i32;
    let sec_remainder = (day_double - days_whole as f64 + month_remainder_days
        - month_remainder_days as i64 as f64)
        * 86_400.0;
    // Round the spilled seconds to microsecond precision.
    let sec_remainder = (sec_remainder * 1_000_000.0).round() / 1_000_000.0;
    let days = days_whole + month_remainder_days as i64 as i32;
    let micros = (interval.micros as f64 * f + sec_remainder * 1_000_000.0).round() as i64;
    super::types::Interval { months, days, micros }
}

/// `justify_hours`: carry whole days out of the time field.
pub fn justify_hours(mut interval: super::types::Interval) -> super::types::Interval {
    let wholeday = (interval.micros / DAY_US) as i32;
    interval.micros -= wholeday as i64 * DAY_US;
    interval.days += wholeday;
    if interval.days > 0 && interval.micros < 0 {
        interval.micros += DAY_US;
        interval.days -= 1;
    } else if interval.days < 0 && interval.micros > 0 {
        interval.micros -= DAY_US;
        interval.days += 1;
    }
    interval
}

/// `justify_days`: carry whole 30-day months out of the day field.
pub fn justify_days(mut interval: super::types::Interval) -> super::types::Interval {
    let wholemonth = interval.days / 30;
    interval.days -= wholemonth * 30;
    interval.months += wholemonth;
    if interval.months > 0 && interval.days < 0 {
        interval.days += 30;
        interval.months -= 1;
    } else if interval.months < 0 && interval.days > 0 {
        interval.days -= 30;
        interval.months += 1;
    }
    interval
}

/// `justify_interval`: normalize so months/days/time share a sign.
pub fn justify_interval(interval: super::types::Interval) -> super::types::Interval {
    let mut r = justify_hours(interval);
    let wholemonth = r.days / 30;
    r.days -= wholemonth * 30;
    r.months += wholemonth;
    if r.months > 0 && (r.days < 0 || (r.days == 0 && r.micros < 0)) {
        r.days += 30;
        r.months -= 1;
    } else if r.months < 0 && (r.days > 0 || (r.days == 0 && r.micros > 0)) {
        r.days -= 30;
        r.months += 1;
    }
    if r.days > 0 && r.micros < 0 {
        r.micros += DAY_US;
        r.days -= 1;
    } else if r.days < 0 && r.micros > 0 {
        r.micros -= DAY_US;
        r.days += 1;
    }
    r
}

/// `age(timestamp1, timestamp2)`: the symbolic (calendar) interval between two timestamps
/// (micros from the PostgreSQL epoch), matching PostgreSQL's `timestamp_age` —
/// field-wise subtraction with calendar borrow using the earlier date's month
/// length.
pub fn age_between(timestamp1: i64, timestamp2: i64) -> super::types::Interval {
    // Compute the positive age (larger minus smaller) with calendar borrow,
    // then negate if the arguments were in the other order — PostgreSQL's
    // `timestamp_age` normalizes the borrow to non-negative fields and recovers
    // the sign at the end.
    let neg = timestamp1 < timestamp2;
    let (hi, lo) = if neg { (timestamp2, timestamp1) } else { (timestamp1, timestamp2) };
    let (yh, moh, dh, ush) = decompose(hi);
    let (yl, mol, dl, usl) = decompose(lo);
    let mut microseconds = ush - usl;
    let mut month_day = dh as i64 - dl as i64;
    let mut month = moh as i64 - mol as i64;
    let mut year = yh - yl;
    if microseconds < 0 {
        microseconds += DAY_US;
        month_day -= 1;
    }
    while month_day < 0 {
        // Borrow a month's worth of days from the earlier date's own month.
        month_day += days_in_month(yl, mol) as i64;
        month -= 1;
    }
    while month < 0 {
        month += 12;
        year -= 1;
    }
    let interval = super::types::Interval {
        months: (year * 12 + month) as i32,
        days: month_day as i32,
        micros: microseconds,
    };
    if neg {
        super::types::Interval { months: -interval.months, days: -interval.days, micros: -interval.micros }
    } else {
        interval
    }
}

/// Splits a timestamp (micros from the PG epoch) into (year, month, day,
/// microseconds-within-day).
fn decompose(timestamp: i64) -> (i64, u32, u32, i64) {
    let days = timestamp.div_euclid(DAY_US);
    let time_of_day = timestamp.rem_euclid(DAY_US);
    let (y, m, d) = civil_from_days(days + PG_EPOCH_DAYS);
    (y, m, d, time_of_day)
}

const DAY_US: i64 = 86_400_000_000;
const DOW: [&str; 7] = ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"];
const MON: [&str; 12] = [
    "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
];

/// PostgreSQL DateStyle output format.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DateFormat {
    Iso,
    Postgres,
    Sql,
    German,
}

/// Field order. YMD collapses to MDY for *output* (PostgreSQL only distinguishes
/// DMY from the rest when rendering).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FieldOrder {
    Mdy,
    Dmy,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DateStyle {
    pub format: DateFormat,
    pub order: FieldOrder,
}

impl Default for DateStyle {
    fn default() -> Self {
        DateStyle { format: DateFormat::Iso, order: FieldOrder::Mdy }
    }
}

/// Day of week (0 = Sunday) for a day count since the PostgreSQL epoch
/// (2000-01-01, a Saturday).
pub fn day_of_week(days_since_epoch: i64) -> usize {
    // 2000-01-01 is a Saturday = index 6.
    (((days_since_epoch % 7) + 6) % 7 + 7) as usize % 7
}

pub fn format_date(days: i32) -> StackStr<16> {
    format_date_styled(days, DateStyle::default())
}

/// `with_timezone` renders a timestamptz (UTC) as PostgreSQL does.
pub fn format_timestamp(micros: i64, with_timezone: bool) -> StackStr<48> {
    format_timestamp_styled(micros, with_timezone, DateStyle::default(), crate::sql::timezone::Timezone::utc())
}

/// Date output honoring DateStyle. Matches PostgreSQL: ISO `YYYY-MM-DD`,
/// Postgres `MM-DD-YYYY`/`DD-MM-YYYY`, SQL `MM/DD/YYYY`/`DD/MM/YYYY`, German
/// `DD.MM.YYYY`.
pub fn format_date_styled(days: i32, style: DateStyle) -> StackStr<16> {
    let (y, m, d) = civil_from_days(days as i64 + PG_EPOCH_DAYS);
    let dmy = style.order == FieldOrder::Dmy;
    let mut out = StackStr::<16>::new();
    use core::fmt::Write;
    let _ = match style.format {
        DateFormat::Iso => write!(out, "{y:04}-{m:02}-{d:02}"),
        DateFormat::German => write!(out, "{d:02}.{m:02}.{y:04}"),
        DateFormat::Postgres if dmy => write!(out, "{d:02}-{m:02}-{y:04}"),
        DateFormat::Postgres => write!(out, "{m:02}-{d:02}-{y:04}"),
        DateFormat::Sql if dmy => write!(out, "{d:02}/{m:02}/{y:04}"),
        DateFormat::Sql => write!(out, "{m:02}/{d:02}/{y:04}"),
    };
    out
}

fn write_frac(out: &mut impl core::fmt::Write, frac: i64) {
    if frac == 0 {
        return;
    }
    // Trim trailing zeros as PostgreSQL does.
    let mut f = frac;
    let mut digits = 6;
    while f % 10 == 0 {
        f /= 10;
        digits -= 1;
    }
    let _ = write!(out, ".{f:0width$}", width = digits);
}

/// Timestamp output honoring DateStyle. `timezone_offset_seconds` shifts the wall clock for
/// timestamptz (0 = UTC); the zone suffix is the ISO offset in ISO style and a
/// zone abbreviation otherwise, matching PostgreSQL.
pub fn format_timestamp_styled(
    micros: i64,
    with_timezone: bool,
    style: DateStyle,
    timezone: super::timezone::Timezone,
) -> StackStr<48> {
    // The offset and abbreviation are resolved for this specific instant, so
    // DST is honored; a plain timestamp (no timezone) always renders at wall clock.
    let (timezone_offset_seconds, abbrev) = if with_timezone { timezone.resolve(micros) } else { (0, StackStr::<8>::new()) };
    let timezone_abbreviation = abbrev.as_str();
    let local = micros + timezone_offset_seconds as i64 * 1_000_000;
    let days = local.div_euclid(DAY_US);
    let in_day = local.rem_euclid(DAY_US);
    let (y, m, d) = civil_from_days(days + PG_EPOCH_DAYS);
    let seconds = in_day / 1_000_000;
    let frac = in_day % 1_000_000;
    let (h, mi, s) = (seconds / 3600, (seconds / 60) % 60, seconds % 60);
    let dmy = style.order == FieldOrder::Dmy;
    let mut out = StackStr::<48>::new();
    use core::fmt::Write;

    match style.format {
        DateFormat::Iso => {
            let _ = write!(out, "{y:04}-{m:02}-{d:02} {h:02}:{mi:02}:{s:02}");
            write_frac(&mut out, frac);
            if with_timezone {
                write_iso_offset(&mut out, timezone_offset_seconds);
            }
        }
        DateFormat::Postgres => {
            let dow = DOW[day_of_week(days)];
            let month = MON[(m - 1) as usize];
            if dmy {
                let _ = write!(out, "{dow} {d:02} {month} {h:02}:{mi:02}:{s:02}");
            } else {
                let _ = write!(out, "{dow} {month} {d:02} {h:02}:{mi:02}:{s:02}");
            }
            write_frac(&mut out, frac);
            let _ = write!(out, " {y:04}");
            if with_timezone {
                let _ = write!(out, " {timezone_abbreviation}");
            }
        }
        DateFormat::Sql | DateFormat::German => {
            let _ = if let DateFormat::German = style.format {
                write!(out, "{d:02}.{m:02}.{y:04}")
            } else if dmy {
                write!(out, "{d:02}/{m:02}/{y:04}")
            } else {
                write!(out, "{m:02}/{d:02}/{y:04}")
            };
            let _ = write!(out, " {h:02}:{mi:02}:{s:02}");
            write_frac(&mut out, frac);
            if with_timezone {
                let _ = write!(out, " {timezone_abbreviation}");
            }
        }
    }
    out
}

/// ISO-style zone suffix: `+00`, `+05:30`, `-08`, trimming trailing `:00`.
fn write_iso_offset(out: &mut impl core::fmt::Write, off_secs: i32) {
    let _ = write!(out, "{}", iso_offset_string(off_secs).as_str());
}

/// The ISO offset string for a zone offset (`+00`, `-05`, `+05:30`), trimming
/// a trailing `:00`. Also the zone abbreviation PostgreSQL shows for the
/// `Etc/GMT±N` fixed-offset zones.
pub fn iso_offset_string(off_secs: i32) -> StackStr<10> {
    use core::fmt::Write;
    let sign = if off_secs < 0 { '-' } else { '+' };
    let a = off_secs.unsigned_abs();
    let (hh, mm, ss) = (a / 3600, (a / 60) % 60, a % 60);
    let mut out = StackStr::<10>::new();
    let _ = write!(out, "{sign}{hh:02}");
    if mm != 0 || ss != 0 {
        let _ = write!(out, ":{mm:02}");
    }
    if ss != 0 {
        let _ = write!(out, ":{ss:02}");
    }
    out
}

/// Wall-clock now, as PG-epoch microseconds (UTC).
pub fn now_micros() -> i64 {
    let dur = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("system clock before 1970");
    dur.as_micros() as i64 - PG_EPOCH_SECS * 1_000_000
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sql::types::Interval;

    fn interval(months: i32, days: i32, micros: i64) -> Interval {
        Interval { months, days, micros }
    }

    // Reference values captured from PostgreSQL 18.4.
    #[test]
    fn interval_scale_matches_pg() {
        // interval '1 month' * 1.5 = 1 month 15 days (fractional month -> days).
        assert_eq!(interval_scale(interval(1, 0, 0), 1.5, false), interval(1, 15, 0));
        // interval '1 day' / 2 = 12:00:00 (fractional day -> time).
        assert_eq!(interval_scale(interval(0, 1, 0), 2.0, true), interval(0, 0, 43_200_000_000));
        // interval '10 days' / 3 = 3 days 08:00:00.
        assert_eq!(interval_scale(interval(0, 10, 0), 3.0, true), interval(0, 3, 28_800_000_000));
        // interval '2 hours' * 2.5 = 05:00:00.
        assert_eq!(interval_scale(interval(0, 0, 7_200_000_000), 2.5, false), interval(0, 0, 18_000_000_000));
    }

    #[test]
    fn justify_matches_pg() {
        // 36 hours -> 1 day 12:00:00.
        assert_eq!(justify_hours(interval(0, 0, 129_600_000_000)), interval(0, 1, 43_200_000_000));
        // 35 days -> 1 month 5 days.
        assert_eq!(justify_days(interval(0, 35, 0)), interval(1, 5, 0));
        // 1 month -1 hour -> 29 days 23:00:00.
        assert_eq!(justify_interval(interval(1, 0, -3_600_000_000)), interval(0, 29, 82_800_000_000));
    }

    #[test]
    fn age_matches_pg() {
        let timestamp = |s: &str| parse_timestamp(s, false).unwrap();
        assert_eq!(age_between(timestamp("2024-06-15"), timestamp("2020-01-10")), interval(53, 5, 0));
        // Reversed arguments negate every field.
        assert_eq!(age_between(timestamp("2020-01-10"), timestamp("2024-06-15")), interval(-53, -5, 0));
        // Day borrow uses the earlier date's own month length.
        assert_eq!(age_between(timestamp("2024-03-01"), timestamp("2024-01-31")), interval(1, 1, 0));
        assert_eq!(age_between(timestamp("2000-01-01"), timestamp("1999-02-05")), interval(10, 24, 0));
        // Time-of-day borrow into days.
        assert_eq!(
            age_between(timestamp("2024-01-01 10:00"), timestamp("2023-12-15 14:30")),
            interval(0, 16, 70_200_000_000)
        );
    }

    // Reference outputs captured from PostgreSQL 18.4 for
    // date '2024-01-15', timestamp '2024-01-15 14:30:00[.5]',
    // timestamptz '2024-01-15 14:30:00+00'.
    #[test]
    fn datestyle_output_matches_postgres() {
        let days = parse_date("2024-01-15").unwrap();
        let timestamp = parse_timestamp("2024-01-15 14:30:00", false).unwrap();
        let tsf = parse_timestamp("2024-01-15 14:30:00.5", false).unwrap();
        let mdy = |f| DateStyle { format: f, order: FieldOrder::Mdy };
        let dmy = |f| DateStyle { format: f, order: FieldOrder::Dmy };
        let cases = [
            (mdy(DateFormat::Iso), "2024-01-15", "2024-01-15 14:30:00",
             "2024-01-15 14:30:00.5", "2024-01-15 14:30:00+00"),
            (mdy(DateFormat::Postgres), "01-15-2024", "Mon Jan 15 14:30:00 2024",
             "Mon Jan 15 14:30:00.5 2024", "Mon Jan 15 14:30:00 2024 UTC"),
            (dmy(DateFormat::Postgres), "15-01-2024", "Mon 15 Jan 14:30:00 2024",
             "Mon 15 Jan 14:30:00.5 2024", "Mon 15 Jan 14:30:00 2024 UTC"),
            (mdy(DateFormat::Sql), "01/15/2024", "01/15/2024 14:30:00",
             "01/15/2024 14:30:00.5", "01/15/2024 14:30:00 UTC"),
            (dmy(DateFormat::Sql), "15/01/2024", "15/01/2024 14:30:00",
             "15/01/2024 14:30:00.5", "15/01/2024 14:30:00 UTC"),
            (mdy(DateFormat::German), "15.01.2024", "15.01.2024 14:30:00",
             "15.01.2024 14:30:00.5", "15.01.2024 14:30:00 UTC"),
        ];
        for (style, d_exp, ts_exp, tsf_exp, tstz_exp) in cases {
            assert_eq!(format_date_styled(days, style).as_str(), d_exp, "{style:?} date");
            assert_eq!(format_timestamp_styled(timestamp, false, style, crate::sql::timezone::Timezone::utc()).as_str(), ts_exp, "{style:?} timestamp");
            assert_eq!(format_timestamp_styled(tsf, false, style, crate::sql::timezone::Timezone::utc()).as_str(), tsf_exp, "{style:?} tsf");
            assert_eq!(format_timestamp_styled(timestamp, true, style, crate::sql::timezone::Timezone::utc()).as_str(), tstz_exp, "{style:?} tstz");
        }
    }

    #[test]
    fn day_of_week_matches_postgres() {
        // Sun Feb 04, Tue Mar 05, Wed Dec 25, Sun Jun 09 (2024), per PostgreSQL.
        for (s, dow) in [
            ("2024-02-04", "Sun"),
            ("2024-03-05", "Tue"),
            ("2024-12-25", "Wed"),
            ("2024-06-09", "Sun"),
        ] {
            let days = parse_date(s).unwrap() as i64;
            assert_eq!(DOW[day_of_week(days)], dow, "{s}");
        }
    }

    #[test]
    fn date_roundtrip() {
        for (s, expect) in [
            ("2000-01-01", 0),
            ("2000-01-02", 1),
            ("1999-12-31", -1),
            ("2024-02-29", 8825),
            ("1970-01-01", -(PG_EPOCH_DAYS as i32)),
        ] {
            let d = parse_date(s).unwrap();
            assert_eq!(d, expect, "{s}");
            assert_eq!(format_date(d).as_str(), s);
        }
        assert!(parse_date("2023-02-29").is_err());
        assert!(parse_date("2023-13-01").is_err());
        assert!(parse_date("not-a-date").is_err());
    }

    #[test]
    fn make_constructors_match_parsing() {
        // make_date agrees with parse_date, and validates its fields.
        assert_eq!(make_date(2024, 6, 15).unwrap(), parse_date("2024-06-15").unwrap());
        assert_eq!(make_date(2000, 1, 1).unwrap(), 0);
        assert!(make_date(2024, 13, 1).is_err());
        assert!(make_date(2024, 2, 30).is_err());
        // make_time counts microseconds since midnight.
        assert_eq!(make_time(12, 30, 0.0).unwrap(), ((12 * 60 + 30) * 60) * 1_000_000);
        assert_eq!(make_time(0, 0, 45.5).unwrap(), 45_500_000);
        assert!(make_time(24, 0, 0.0).is_err());
        assert!(make_time(0, 0, 60.0).is_err());
        // make_timestamp combines the two.
        assert_eq!(
            make_timestamp(2024, 6, 15, 12, 30, 0.0).unwrap(),
            make_date(2024, 6, 15).unwrap() as i64 * 86_400_000_000 + make_time(12, 30, 0.0).unwrap()
        );
    }

    #[test]
    fn to_date_parses_formats() {
        let d = parse_date("2024-06-15").unwrap();
        assert_eq!(to_date("2024-06-15", "YYYY-MM-DD").unwrap(), d);
        assert_eq!(to_date("15/06/2024", "DD/MM/YYYY").unwrap(), d);
        assert_eq!(to_date("06-15-2024", "MM-DD-YYYY").unwrap(), d);
        assert_eq!(to_date("240615", "YYMMDD").unwrap(), d);
        assert_eq!(to_date("2024-6-5", "YYYY-MM-DD").unwrap(), parse_date("2024-06-05").unwrap());
        assert_eq!(to_date("Jun 15 2024", "Mon DD YYYY").unwrap(), d);
        assert_eq!(
            to_timestamp("2024-06-15 12:30:45", "YYYY-MM-DD HH24:MI:SS").unwrap(),
            parse_timestamp("2024-06-15 12:30:45", false).unwrap()
        );
        assert!(to_date("2024-13-01", "YYYY-MM-DD").is_err());
    }

    #[test]
    fn timestamp_roundtrip() {
        let t = parse_timestamp("2000-01-01 00:00:00", false).unwrap();
        assert_eq!(t, 0);
        assert_eq!(format_timestamp(t, false).as_str(), "2000-01-01 00:00:00");

        let t = parse_timestamp("2024-06-15 12:34:56.789", false).unwrap();
        assert_eq!(format_timestamp(t, false).as_str(), "2024-06-15 12:34:56.789");
        assert_eq!(format_timestamp(t, true).as_str(), "2024-06-15 12:34:56.789+00");

        // Zone shifting for timestamptz.
        let utc = parse_timestamp("2024-01-01 12:00:00+00", true).unwrap();
        let plus2 = parse_timestamp("2024-01-01 14:00:00+02", true).unwrap();
        assert_eq!(utc, plus2);
        let z = parse_timestamp("2024-01-01T12:00:00Z", true).unwrap();
        assert_eq!(utc, z);
    }
}
