//! Date/time storage and text I/O.
//!
//! Storage matches PostgreSQL's on-disk convention: dates are days since
//! 2000-01-01, timestamps are microseconds since 2000-01-01 00:00:00.
//! Civil-date math is Howard Hinnant's public-domain algorithms
//! (<https://howardhinnant.github.io/date_algorithms.html>). The session
//! time zone is fixed at UTC.

use crate::sql::eval::sqlstate;
use crate::sql_err;
use crate::util::StackStr;

use super::eval::SqlError;

/// Days between 1970-01-01 and 2000-01-01.
pub const PG_EPOCH_DAYS: i64 = 10_957;
/// Seconds between the unix and PostgreSQL epochs.
pub const PG_EPOCH_SECS: i64 = 946_684_800;

pub fn civil_from_days(days_since_epoch: i64) -> (i64, u32, u32) {
    let shifted = days_since_epoch + 719_468;
    let era = shifted.div_euclid(146_097);
    let day_of_era = shifted.rem_euclid(146_097);
    let year_of_era =
        (day_of_era - day_of_era / 1460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let year = year_of_era + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_index = (5 * day_of_year + 2) / 153;
    let day = (day_of_year - (153 * month_index + 2) / 5 + 1) as u32;
    let month = if month_index < 10 {
        month_index + 3
    } else {
        month_index - 9
    } as u32;
    (if month <= 2 { year + 1 } else { year }, month, day)
}

pub fn days_from_civil(year: i64, month: u32, day: u32) -> i64 {
    let year = if month <= 2 { year - 1 } else { year };
    let era = year.div_euclid(400);
    let year_of_era = year.rem_euclid(400);
    let month_index = if month > 2 { month - 3 } else { month + 9 } as i64;
    let day_of_year = (153 * month_index + 2) / 5 + day as i64 - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era * 146_097 + day_of_era - 719_468
}

fn days_in_month(year: i64, month: u32) -> u32 {
    match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 => {
            if (year % 4 == 0 && year % 100 != 0) || year % 400 == 0 {
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
            sqlstate::INVALID_DATETIME_FORMAT,
            "invalid input syntax for type date: \"{}\"",
            s
        )
    };
    let out_of_range = || {
        sql_err!(
            sqlstate::DATETIME_FIELD_OVERFLOW,
            "date/time field value out of range: \"{}\"",
            s
        )
    };
    let trimmed = s.trim();
    let mut parts = trimmed.splitn(3, '-');
    let (year, month, day) = (
        parts
            .next()
            .and_then(|p| p.parse::<i64>().ok())
            .ok_or_else(bad)?,
        parts
            .next()
            .and_then(|p| p.parse::<u32>().ok())
            .ok_or_else(bad)?,
        parts
            .next()
            .and_then(|p| p.parse::<u32>().ok())
            .ok_or_else(bad)?,
    );
    if !(1..=12).contains(&month) || day < 1 || day > days_in_month(year, month) {
        return Err(out_of_range());
    }
    let days = days_from_civil(year, month, day) - PG_EPOCH_DAYS;
    i32::try_from(days).map_err(|_| out_of_range())
}

const MONTH_ABBR: [&str; 12] = [
    "jan", "feb", "mar", "apr", "may", "jun", "jul", "aug", "sep", "oct", "nov", "dec",
];
const MONTH_FULL: [&str; 12] = [
    "january",
    "february",
    "march",
    "april",
    "may",
    "june",
    "july",
    "august",
    "september",
    "october",
    "november",
    "december",
];
const WEEKDAY_ABBR: [&str; 7] = ["sun", "mon", "tue", "wed", "thu", "fri", "sat"];
const WEEKDAY_FULL: [&str; 7] = [
    "sunday",
    "monday",
    "tuesday",
    "wednesday",
    "thursday",
    "friday",
    "saturday",
];

fn expand_short_year(value: i64, width: u32) -> i64 {
    let scale = 10i64.pow(width.max(2));
    let mut year = 2020 - 2020 % scale + value;
    if year - 2020 >= scale / 2 {
        year -= scale;
    } else if 2020 - year > scale / 2 {
        year += scale;
    }
    year
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum FormatDateConvention {
    Unset,
    Gregorian,
    IsoWeek,
    Julian,
}

impl FormatDateConvention {
    fn claim(self, next: Self) -> Result<Self, SqlError> {
        if matches!(self, Self::Unset) || self == next {
            Ok(next)
        } else {
            Err(sql_err!(
                sqlstate::INVALID_DATETIME_FORMAT,
                "invalid combination of date conventions"
            ))
        }
    }
}

/// A fully resolved date/time format-model result. Parsing produces this
/// typed state before either `to_date` or `to_timestamp` consumes it, so the
/// two entry points cannot disagree about a calendar interpretation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FormattedDateTime {
    pub year: i64,
    pub month: u32,
    pub day: u32,
    pub hour: i64,
    pub minute: i64,
    pub second: i64,
    pub microsecond: i64,
    pub timezone_offset_seconds: Option<i32>,
}

/// Parses `input` guided by a `to_date`/`to_timestamp` format string. Civil,
/// ordinal, and ISO-week dates are resolved here rather than left as partially
/// interpreted fields for callers to combine.
pub fn parse_formatted(input: &str, fmt: &str) -> Result<FormattedDateTime, SqlError> {
    let bad = || {
        sql_err!(
            sqlstate::INVALID_DATETIME_FORMAT,
            "invalid value for input string"
        )
    };
    let (mut y, mut month, mut d, mut h, mut minute, mut s, mut us) =
        (2000i64, 1u32, 1u32, 0i64, 0i64, 0i64, 0i64);
    let mut ordinal = None;
    let mut gregorian_year = None;
    let mut century = None;
    let (mut iso_year, mut iso_week, mut iso_day, mut iso_ordinal) = (None, None, None, None);
    let mut julian = None;
    let mut date_convention = FormatDateConvention::Unset;
    let (mut timezone_sign, mut timezone_hour, mut timezone_minute) = (1i32, None, None);
    let mut twelve_hour = false;
    let mut pm = None;
    let mut bc = false;
    let input_bytes = input.as_bytes();
    let format_bytes = fmt.as_bytes();
    let mut input_position = 0usize;
    let starts_with_ci = |bytes: &[u8], at: usize, word: &[u8]| -> bool {
        at + word.len() <= bytes.len() && bytes[at..at + word.len()].eq_ignore_ascii_case(word)
    };
    let exact = starts_with_ci(format_bytes, 0, b"FX");
    let mut format_index = if exact { 2 } else { 0 };
    // Reads up to `width` decimal digits (skipping leading spaces) into an int.
    let read_num = |input_position: &mut usize, width: usize| -> Option<i64> {
        while !exact && *input_position < input_bytes.len() && input_bytes[*input_position] == b' '
        {
            *input_position += 1;
        }
        let start = *input_position;
        let mut v: i64 = 0;
        while *input_position < input_bytes.len()
            && *input_position - start < width
            && input_bytes[*input_position].is_ascii_digit()
        {
            v = v * 10 + (input_bytes[*input_position] - b'0') as i64;
            *input_position += 1;
        }
        if *input_position == start {
            None
        } else {
            Some(v)
        }
    };
    while format_index < format_bytes.len() {
        let up = format_bytes[format_index].to_ascii_uppercase();
        if format_bytes[format_index] == b'"' {
            format_index += 1;
            while format_index < format_bytes.len() && format_bytes[format_index] != b'"' {
                if format_bytes[format_index] == b'\\' && format_index + 1 < format_bytes.len() {
                    format_index += 1;
                }
                if input_position >= input_bytes.len() {
                    return Err(bad());
                }
                input_position += 1;
                format_index += 1;
            }
            if format_index < format_bytes.len() {
                format_index += 1;
            }
            continue;
        }
        if starts_with_ci(format_bytes, format_index, b"FM") {
            format_index += 2;
            continue;
        }
        // Longest field codes first.
        if starts_with_ci(format_bytes, format_index, b"IYYY") {
            date_convention = date_convention.claim(FormatDateConvention::IsoWeek)?;
            iso_year = Some(read_num(&mut input_position, 4).ok_or_else(bad)?);
            format_index += 4;
        } else if starts_with_ci(format_bytes, format_index, b"IDDD") {
            date_convention = date_convention.claim(FormatDateConvention::IsoWeek)?;
            iso_ordinal = Some(read_num(&mut input_position, 3).ok_or_else(bad)?);
            format_index += 4;
        } else if starts_with_ci(format_bytes, format_index, b"HH24") {
            h = read_num(&mut input_position, 2).ok_or_else(bad)?;
            format_index += 4;
        } else if starts_with_ci(format_bytes, format_index, b"HH12") {
            h = read_num(&mut input_position, 2).ok_or_else(bad)?;
            twelve_hour = true;
            format_index += 4;
        } else if starts_with_ci(format_bytes, format_index, b"SSSSS") {
            let seconds = read_num(&mut input_position, 5).ok_or_else(bad)?;
            if !(0..86_400).contains(&seconds) {
                return Err(sql_err!(
                    sqlstate::DATETIME_FIELD_OVERFLOW,
                    "date/time field value out of range"
                ));
            }
            h = seconds / 3600;
            minute = (seconds / 60) % 60;
            s = seconds % 60;
            format_index += 5;
        } else if starts_with_ci(format_bytes, format_index, b"SSSS") {
            let seconds = read_num(&mut input_position, 5).ok_or_else(bad)?;
            if !(0..86_400).contains(&seconds) {
                return Err(sql_err!(
                    sqlstate::DATETIME_FIELD_OVERFLOW,
                    "date/time field value out of range"
                ));
            }
            h = seconds / 3600;
            minute = (seconds / 60) % 60;
            s = seconds % 60;
            format_index += 4;
        } else if starts_with_ci(format_bytes, format_index, b"YYYY") {
            date_convention = date_convention.claim(FormatDateConvention::Gregorian)?;
            gregorian_year = Some((read_num(&mut input_position, 4).ok_or_else(bad)?, 4));
            format_index += 4;
        } else if starts_with_ci(format_bytes, format_index, b"Y,YYY") {
            date_convention = date_convention.claim(FormatDateConvention::Gregorian)?;
            let leading = read_num(&mut input_position, 3).ok_or_else(bad)?;
            if input_position < input_bytes.len() && input_bytes[input_position] == b',' {
                input_position += 1;
            }
            let trailing = read_num(&mut input_position, 3).ok_or_else(bad)?;
            gregorian_year = Some((leading * 1000 + trailing, 4));
            format_index += 5;
        } else if starts_with_ci(format_bytes, format_index, b"MONTH") {
            date_convention = date_convention.claim(FormatDateConvention::Gregorian)?;
            month = read_month(input, &mut input_position, false).ok_or_else(bad)?;
            format_index += 5;
        } else if starts_with_ci(format_bytes, format_index, b"MON") {
            date_convention = date_convention.claim(FormatDateConvention::Gregorian)?;
            month = read_month(input, &mut input_position, true).ok_or_else(bad)?;
            format_index += 3;
        } else if starts_with_ci(format_bytes, format_index, b"DAY") {
            date_convention = date_convention.claim(FormatDateConvention::Gregorian)?;
            read_weekday(input, &mut input_position, false).ok_or_else(bad)?;
            format_index += 3;
        } else if starts_with_ci(format_bytes, format_index, b"YYY") {
            date_convention = date_convention.claim(FormatDateConvention::Gregorian)?;
            gregorian_year = Some((read_num(&mut input_position, 3).ok_or_else(bad)?, 3));
            format_index += 3;
        } else if starts_with_ci(format_bytes, format_index, b"DDD") {
            date_convention = date_convention.claim(FormatDateConvention::Gregorian)?;
            ordinal = Some(read_num(&mut input_position, 3).ok_or_else(bad)?);
            format_index += 3;
        } else if starts_with_ci(format_bytes, format_index, b"IYY") {
            date_convention = date_convention.claim(FormatDateConvention::IsoWeek)?;
            iso_year = Some(expand_short_year(
                read_num(&mut input_position, 3).ok_or_else(bad)?,
                3,
            ));
            format_index += 3;
        } else if up == b'H' && starts_with_ci(format_bytes, format_index, b"HH") {
            h = read_num(&mut input_position, 2).ok_or_else(bad)?;
            twelve_hour = true;
            format_index += 2;
        } else if starts_with_ci(format_bytes, format_index, b"YY") {
            date_convention = date_convention.claim(FormatDateConvention::Gregorian)?;
            let v = read_num(&mut input_position, 2).ok_or_else(bad)?;
            gregorian_year = Some((v, 2));
            format_index += 2;
        } else if starts_with_ci(format_bytes, format_index, b"IY") {
            date_convention = date_convention.claim(FormatDateConvention::IsoWeek)?;
            let v = read_num(&mut input_position, 2).ok_or_else(bad)?;
            iso_year = Some(expand_short_year(v, 2));
            format_index += 2;
        } else if starts_with_ci(format_bytes, format_index, b"IW") {
            date_convention = date_convention.claim(FormatDateConvention::IsoWeek)?;
            iso_week = Some(read_num(&mut input_position, 2).ok_or_else(bad)?);
            format_index += 2;
        } else if starts_with_ci(format_bytes, format_index, b"WW") {
            date_convention = date_convention.claim(FormatDateConvention::Gregorian)?;
            let week = read_num(&mut input_position, 2).ok_or_else(bad)?;
            if !(1..=53).contains(&week) {
                return Err(sql_err!(
                    sqlstate::DATETIME_FIELD_OVERFLOW,
                    "date/time field value out of range"
                ));
            }
            ordinal = Some((week - 1) * 7 + 1);
            format_index += 2;
        } else if starts_with_ci(format_bytes, format_index, b"MM") {
            date_convention = date_convention.claim(FormatDateConvention::Gregorian)?;
            month = read_num(&mut input_position, 2).ok_or_else(bad)? as u32;
            format_index += 2;
        } else if starts_with_ci(format_bytes, format_index, b"DD") {
            date_convention = date_convention.claim(FormatDateConvention::Gregorian)?;
            d = read_num(&mut input_position, 2).ok_or_else(bad)? as u32;
            format_index += 2;
        } else if starts_with_ci(format_bytes, format_index, b"DY") {
            date_convention = date_convention.claim(FormatDateConvention::Gregorian)?;
            read_weekday(input, &mut input_position, true).ok_or_else(bad)?;
            format_index += 2;
        } else if starts_with_ci(format_bytes, format_index, b"MI") {
            minute = read_num(&mut input_position, 2).ok_or_else(bad)?;
            format_index += 2;
        } else if starts_with_ci(format_bytes, format_index, b"SS") {
            s = read_num(&mut input_position, 2).ok_or_else(bad)?;
            format_index += 2;
        } else if starts_with_ci(format_bytes, format_index, b"MS") {
            us = read_num(&mut input_position, 3).ok_or_else(bad)? * 1000;
            format_index += 2;
        } else if starts_with_ci(format_bytes, format_index, b"US") {
            us = read_num(&mut input_position, 6).ok_or_else(bad)?;
            format_index += 2;
        } else if starts_with_ci(format_bytes, format_index, b"FF")
            && format_index + 2 < format_bytes.len()
            && matches!(format_bytes[format_index + 2], b'1'..=b'6')
        {
            let width = usize::from(format_bytes[format_index + 2] - b'0');
            us = read_num(&mut input_position, width).ok_or_else(bad)?
                * 10i64.pow((6 - width) as u32);
            format_index += 3;
        } else if starts_with_ci(format_bytes, format_index, b"RM") {
            date_convention = date_convention.claim(FormatDateConvention::Gregorian)?;
            month = read_roman_month(input, &mut input_position).ok_or_else(bad)?;
            format_index += 2;
        } else if starts_with_ci(format_bytes, format_index, b"A.M.")
            || starts_with_ci(format_bytes, format_index, b"P.M.")
        {
            pm = Some(read_meridiem(input, &mut input_position, true).ok_or_else(bad)?);
            format_index += 4;
        } else if starts_with_ci(format_bytes, format_index, b"AM")
            || starts_with_ci(format_bytes, format_index, b"PM")
        {
            pm = Some(read_meridiem(input, &mut input_position, false).ok_or_else(bad)?);
            format_index += 2;
        } else if starts_with_ci(format_bytes, format_index, b"A.D.")
            || starts_with_ci(format_bytes, format_index, b"B.C.")
        {
            date_convention = date_convention.claim(FormatDateConvention::Gregorian)?;
            bc = read_era(input, &mut input_position, true).ok_or_else(bad)?;
            format_index += 4;
        } else if starts_with_ci(format_bytes, format_index, b"AD")
            || starts_with_ci(format_bytes, format_index, b"BC")
        {
            date_convention = date_convention.claim(FormatDateConvention::Gregorian)?;
            bc = read_era(input, &mut input_position, false).ok_or_else(bad)?;
            format_index += 2;
        } else if starts_with_ci(format_bytes, format_index, b"ID") {
            date_convention = date_convention.claim(FormatDateConvention::IsoWeek)?;
            iso_day = Some(read_num(&mut input_position, 1).ok_or_else(bad)?);
            format_index += 2;
        } else if starts_with_ci(format_bytes, format_index, b"CC") {
            date_convention = date_convention.claim(FormatDateConvention::Gregorian)?;
            century = Some(read_num(&mut input_position, 2).ok_or_else(bad)?);
            format_index += 2;
        } else if starts_with_ci(format_bytes, format_index, b"TZH") {
            if input_position < input_bytes.len()
                && matches!(input_bytes[input_position], b'+' | b'-')
            {
                timezone_sign = if input_bytes[input_position] == b'-' {
                    -1
                } else {
                    1
                };
                input_position += 1;
            }
            timezone_hour = Some(read_num(&mut input_position, 2).ok_or_else(bad)? as i32);
            format_index += 3;
        } else if starts_with_ci(format_bytes, format_index, b"TZM") {
            timezone_minute = Some(read_num(&mut input_position, 2).ok_or_else(bad)? as i32);
            format_index += 3;
        } else if starts_with_ci(format_bytes, format_index, b"TZ") {
            while !exact
                && input_position < input_bytes.len()
                && input_bytes[input_position] == b' '
            {
                input_position += 1;
            }
            let start = input_position;
            while input_position < input_bytes.len()
                && input_bytes[input_position].is_ascii_alphabetic()
            {
                input_position += 1;
            }
            let name =
                core::str::from_utf8(&input_bytes[start..input_position]).map_err(|_| bad())?;
            let zone = if name.eq_ignore_ascii_case("UTC") || name.eq_ignore_ascii_case("GMT") {
                super::timezone::Timezone::utc()
            } else {
                super::timezone::lookup(name).ok_or_else(|| {
                    sql_err!(
                        sqlstate::INVALID_PARAMETER_VALUE,
                        "time zone \"{}\" not recognized",
                        name
                    )
                })?
            };
            timezone_hour = Some(zone.resolve(0).0 / 3600);
            timezone_minute = Some((zone.resolve(0).0.unsigned_abs() / 60 % 60) as i32);
            timezone_sign = zone.resolve(0).0.signum();
            format_index += 2;
        } else if up == b'J' {
            date_convention = date_convention.claim(FormatDateConvention::Julian)?;
            julian = Some(read_num(&mut input_position, 7).ok_or_else(bad)?);
            format_index += 1;
        } else if up == b'Q' {
            date_convention = date_convention.claim(FormatDateConvention::Gregorian)?;
            let quarter = read_num(&mut input_position, 1).ok_or_else(bad)?;
            if !(1..=4).contains(&quarter) {
                return Err(sql_err!(
                    sqlstate::DATETIME_FIELD_OVERFLOW,
                    "date/time field value out of range"
                ));
            }
            format_index += 1;
        } else if up == b'W' {
            date_convention = date_convention.claim(FormatDateConvention::Gregorian)?;
            let week = read_num(&mut input_position, 1).ok_or_else(bad)?;
            if !(1..=5).contains(&week) {
                return Err(sql_err!(
                    sqlstate::DATETIME_FIELD_OVERFLOW,
                    "date/time field value out of range"
                ));
            }
            ordinal = Some((week - 1) * 7 + 1);
            format_index += 1;
        } else if up == b'D' {
            date_convention = date_convention.claim(FormatDateConvention::Gregorian)?;
            let weekday = read_num(&mut input_position, 1).ok_or_else(bad)?;
            if !(1..=7).contains(&weekday) {
                return Err(sql_err!(
                    sqlstate::DATETIME_FIELD_OVERFLOW,
                    "date/time field value out of range"
                ));
            }
            format_index += 1;
        } else if up == b'Y' {
            date_convention = date_convention.claim(FormatDateConvention::Gregorian)?;
            gregorian_year = Some((read_num(&mut input_position, 1).ok_or_else(bad)?, 1));
            format_index += 1;
        } else if up == b'I' {
            date_convention = date_convention.claim(FormatDateConvention::IsoWeek)?;
            iso_year = Some(expand_short_year(
                read_num(&mut input_position, 1).ok_or_else(bad)?,
                1,
            ));
            format_index += 1;
        } else if starts_with_ci(format_bytes, format_index, b"TH") {
            if input_position + 2 > input_bytes.len()
                || !input_bytes[input_position..input_position + 2]
                    .iter()
                    .all(u8::is_ascii_alphabetic)
            {
                return Err(bad());
            }
            input_position += 2;
            format_index += 2;
        } else if starts_with_ci(format_bytes, format_index, b"SP") {
            format_index += 2;
        } else if up.is_ascii_alphabetic() {
            return Err(sql_err!(
                sqlstate::INVALID_DATETIME_FORMAT,
                "unsupported to_date/to_timestamp code"
            ));
        } else {
            if exact {
                if input_position >= input_bytes.len() {
                    return Err(bad());
                }
                input_position += 1;
            } else if input_position < input_bytes.len()
                && !input_bytes[input_position].is_ascii_alphanumeric()
            {
                input_position += 1;
            }
            format_index += 1;
        }
    }
    y = match (gregorian_year, century) {
        (Some((value, width @ (1 | 2))), Some(century)) => {
            (century - 1) * 100 + value % 10i64.pow(width)
        }
        (Some((value, width @ 1..=3)), _) => expand_short_year(value, width),
        (Some((value, _)), _) => value,
        (None, Some(century)) => (century - 1) * 100 + 1,
        (None, None) => y,
    };
    if let Some(julian_day) = julian {
        let days = julian_day - 2_440_588;
        (y, month, d) = civil_from_days(days);
    } else if let (Some(year), Some(day_of_year)) = (iso_year, iso_ordinal) {
        if !(1..=371).contains(&day_of_year) {
            return Err(sql_err!(
                sqlstate::DATETIME_FIELD_OVERFLOW,
                "date/time field value out of range"
            ));
        }
        let jan4 = days_from_civil(year, 1, 4) - PG_EPOCH_DAYS;
        let jan4_iso_day = ((day_of_week(jan4) + 6) % 7 + 1) as i64;
        let days = jan4 - (jan4_iso_day - 1) + day_of_year - 1;
        let (actual_iso_year, _, _) = {
            let actual_iso_day = ((day_of_week(days) + 6) % 7 + 1) as i64;
            civil_from_days(days + (4 - actual_iso_day) + PG_EPOCH_DAYS)
        };
        if actual_iso_year != year {
            return Err(sql_err!(
                sqlstate::DATETIME_FIELD_OVERFLOW,
                "date/time field value out of range"
            ));
        }
        (y, month, d) = civil_from_days(days + PG_EPOCH_DAYS);
    } else if let (Some(year), Some(week), Some(day)) = (iso_year, iso_week, iso_day) {
        let jan4 = days_from_civil(year, 1, 4) - PG_EPOCH_DAYS;
        let jan4_iso_day = ((day_of_week(jan4) + 6) % 7 + 1) as i64;
        if !(1..=53).contains(&week) || !(1..=7).contains(&day) {
            return Err(sql_err!(
                sqlstate::DATETIME_FIELD_OVERFLOW,
                "date/time field value out of range"
            ));
        }
        let days = jan4 - (jan4_iso_day - 1) + (week - 1) * 7 + (day - 1);
        let (actual_year, actual_week, _) = {
            let iso_day = ((day_of_week(days) + 6) % 7 + 1) as i64;
            let (actual_year, _, _) = civil_from_days(days + (4 - iso_day) + PG_EPOCH_DAYS);
            let first = days_from_civil(actual_year, 1, 4) - PG_EPOCH_DAYS;
            let first_iso_day = ((day_of_week(first) + 6) % 7 + 1) as i64;
            (
                actual_year,
                (days - (first - (first_iso_day - 1))) / 7 + 1,
                iso_day,
            )
        };
        if actual_year != year || actual_week != week {
            return Err(sql_err!(
                sqlstate::DATETIME_FIELD_OVERFLOW,
                "date/time field value out of range"
            ));
        }
        (y, month, d) = civil_from_days(days + PG_EPOCH_DAYS);
    } else if iso_year.is_some() || iso_week.is_some() || iso_day.is_some() || iso_ordinal.is_some()
    {
        return Err(sql_err!(
            sqlstate::INVALID_DATETIME_FORMAT,
            "ISO week date requires IYYY, IW, and ID"
        ));
    } else if let Some(day_of_year) = ordinal {
        let max = if days_in_month(y, 2) == 29 { 366 } else { 365 };
        if !(1..=max).contains(&(day_of_year as u32)) {
            return Err(sql_err!(
                sqlstate::DATETIME_FIELD_OVERFLOW,
                "date/time field value out of range"
            ));
        }
        let (resolved_year, resolved_month, resolved_day) =
            civil_from_days(days_from_civil(y, 1, 1) + day_of_year - 1);
        (y, month, d) = (resolved_year, resolved_month, resolved_day);
    }
    if bc {
        y = 1 - y;
    }
    if twelve_hour {
        if !(1..=12).contains(&h) {
            return Err(sql_err!(
                sqlstate::DATETIME_FIELD_OVERFLOW,
                "date/time field value out of range"
            ));
        }
        h = h % 12 + if pm.unwrap_or(false) { 12 } else { 0 };
    } else if pm.is_some() {
        return Err(sql_err!(
            sqlstate::INVALID_DATETIME_FORMAT,
            "AM/PM requires an HH12 or HH format"
        ));
    }
    if !(1..=12).contains(&month)
        || d < 1
        || d > days_in_month(y, month)
        || !(0..=23).contains(&h)
        || !(0..=59).contains(&minute)
        || !(0..=59).contains(&s)
        || !(0..1_000_000).contains(&us)
    {
        return Err(sql_err!(
            sqlstate::DATETIME_FIELD_OVERFLOW,
            "date/time field value out of range"
        ));
    }
    Ok(FormattedDateTime {
        year: y,
        month,
        day: d,
        hour: h,
        minute,
        second: s,
        microsecond: us,
        timezone_offset_seconds: match (timezone_hour, timezone_minute) {
            (None, None) => None,
            (hour, minute) => {
                let hour = hour.unwrap_or(0);
                let minute = minute.unwrap_or(0);
                if hour.unsigned_abs() > 15 || !(0..=59).contains(&minute) {
                    return Err(sql_err!(
                        sqlstate::DATETIME_FIELD_OVERFLOW,
                        "time zone displacement out of range"
                    ));
                }
                Some(timezone_sign * (hour.unsigned_abs() as i32 * 3600 + minute * 60))
            }
        },
    })
}

/// Reads a month name (abbreviated when `abbr`, else full) at `*input_position`, returning
/// the 1-based month.
fn read_month(input: &str, input_position: &mut usize, abbr: bool) -> Option<u32> {
    let bytes = input.as_bytes();
    while *input_position < bytes.len() && bytes[*input_position] == b' ' {
        *input_position += 1;
    }
    let table: &[&str] = if abbr { &MONTH_ABBR } else { &MONTH_FULL };
    for (i, name) in table.iter().enumerate() {
        let name_bytes = name.as_bytes();
        if *input_position + name_bytes.len() <= bytes.len()
            && bytes[*input_position..*input_position + name_bytes.len()]
                .eq_ignore_ascii_case(name_bytes)
        {
            *input_position += name_bytes.len();
            return Some(i as u32 + 1);
        }
    }
    // `MON` also accepts the full name; `MONTH` also accepts the abbreviation.
    let other: &[&str] = if abbr { &MONTH_FULL } else { &MONTH_ABBR };
    for (i, name) in other.iter().enumerate() {
        let name_bytes = name.as_bytes();
        if *input_position + name_bytes.len() <= bytes.len()
            && bytes[*input_position..*input_position + name_bytes.len()]
                .eq_ignore_ascii_case(name_bytes)
        {
            *input_position += name_bytes.len();
            return Some(i as u32 + 1);
        }
    }
    None
}

fn read_weekday(input: &str, input_position: &mut usize, abbr: bool) -> Option<()> {
    let bytes = input.as_bytes();
    while *input_position < bytes.len() && bytes[*input_position] == b' ' {
        *input_position += 1;
    }
    let table: &[&str] = if abbr { &WEEKDAY_ABBR } else { &WEEKDAY_FULL };
    for name in table {
        let end = *input_position + name.len();
        if end <= bytes.len() && bytes[*input_position..end].eq_ignore_ascii_case(name.as_bytes()) {
            *input_position = end;
            return Some(());
        }
    }
    None
}

fn read_roman_month(input: &str, input_position: &mut usize) -> Option<u32> {
    const ROMAN: [&str; 12] = [
        "XII", "XI", "IX", "VIII", "VII", "VI", "IV", "III", "II", "I", "V", "X",
    ];
    let bytes = input.as_bytes();
    while *input_position < bytes.len() && bytes[*input_position] == b' ' {
        *input_position += 1;
    }
    for token in ROMAN {
        let end = *input_position + token.len();
        if end <= bytes.len() && bytes[*input_position..end].eq_ignore_ascii_case(token.as_bytes())
        {
            *input_position = end;
            return Some(match token {
                "I" => 1,
                "II" => 2,
                "III" => 3,
                "IV" => 4,
                "V" => 5,
                "VI" => 6,
                "VII" => 7,
                "VIII" => 8,
                "IX" => 9,
                "X" => 10,
                "XI" => 11,
                "XII" => 12,
                _ => unreachable!(),
            });
        }
    }
    None
}

fn read_meridiem(input: &str, input_position: &mut usize, dotted: bool) -> Option<bool> {
    let bytes = input.as_bytes();
    while *input_position < bytes.len() && bytes[*input_position] == b' ' {
        *input_position += 1;
    }
    let am = if dotted {
        b"A.M.".as_slice()
    } else {
        b"AM".as_slice()
    };
    let pm = if dotted {
        b"P.M.".as_slice()
    } else {
        b"PM".as_slice()
    };
    for (token, is_pm) in [(am, false), (pm, true)] {
        let end = *input_position + token.len();
        if end <= bytes.len() && bytes[*input_position..end].eq_ignore_ascii_case(token) {
            *input_position = end;
            return Some(is_pm);
        }
    }
    None
}

fn read_era(input: &str, input_position: &mut usize, dotted: bool) -> Option<bool> {
    let bytes = input.as_bytes();
    while *input_position < bytes.len() && bytes[*input_position] == b' ' {
        *input_position += 1;
    }
    let ad = if dotted {
        b"A.D.".as_slice()
    } else {
        b"AD".as_slice()
    };
    let bc = if dotted {
        b"B.C.".as_slice()
    } else {
        b"BC".as_slice()
    };
    for (token, is_bc) in [(ad, false), (bc, true)] {
        let end = *input_position + token.len();
        if end <= bytes.len() && bytes[*input_position..end].eq_ignore_ascii_case(token) {
            *input_position = end;
            return Some(is_bc);
        }
    }
    None
}

/// `to_date`: parses a formatted date into days since 2000-01-01.
pub fn to_date(input: &str, fmt: &str) -> Result<i32, SqlError> {
    let value = parse_formatted(input, fmt)?;
    make_date(value.year, value.month as i64, value.day as i64)
}

/// `to_timestamp`: parses a formatted timestamp into microseconds since
/// 2000-01-01.
pub fn to_timestamp(input: &str, fmt: &str) -> Result<i64, SqlError> {
    let value = parse_formatted(input, fmt)?;
    let local = make_timestamp(
        value.year,
        value.month as i64,
        value.day as i64,
        value.hour,
        value.minute,
        value.second as f64 + value.microsecond as f64 / 1_000_000.0,
    )?;
    let offset = value
        .timezone_offset_seconds
        .unwrap_or_else(|| super::timezone::session().resolve(local).0);
    Ok(local - i64::from(offset) * 1_000_000)
}

/// Constructs a date (days since 2000-01-01) from year/month/day, validating
/// the fields as PostgreSQL `make_date` does.
pub fn make_date(year: i64, month: i64, day: i64) -> Result<i32, SqlError> {
    let range = || {
        sql_err!(
            sqlstate::DATETIME_FIELD_OVERFLOW,
            "date field value out of range"
        )
    };
    if !(1..=12).contains(&month) {
        return Err(range());
    }
    let month_u32 = month as u32;
    if day < 1 || day as u32 > days_in_month(year, month_u32) {
        return Err(range());
    }
    let days = days_from_civil(year, month_u32, day as u32) - PG_EPOCH_DAYS;
    i32::try_from(days).map_err(|_| range())
}

/// Constructs a time-of-day (microseconds since midnight) from hour/minute and
/// a fractional second, validating fields as PostgreSQL `make_time` does.
pub fn make_time(hour: i64, minute: i64, sec: f64) -> Result<i64, SqlError> {
    let range = || {
        sql_err!(
            sqlstate::DATETIME_FIELD_OVERFLOW,
            "time field value out of range"
        )
    };
    if !(0..=23).contains(&hour) || !(0..=59).contains(&minute) || !(0.0..60.0).contains(&sec) {
        return Err(range());
    }
    Ok(((hour * 60 + minute) * 60) * 1_000_000 + (sec * 1_000_000.0).round() as i64)
}

/// Constructs a timestamp (microseconds since 2000-01-01) from its fields.
pub fn make_timestamp(
    year: i64,
    month: i64,
    day: i64,
    hour: i64,
    minute: i64,
    sec: f64,
) -> Result<i64, SqlError> {
    let days = make_date(year, month, day)? as i64;
    let time_of_day = make_time(hour, minute, sec)?;
    Ok(days * 86_400_000_000 + time_of_day)
}

/// Parses `YYYY-MM-DD[ |T]HH:MM[:SS[.ffffff]][Z|±HH[:MM]]` into
/// microseconds since 2000-01-01 UTC. `require_tz_shift` applies the zone
/// offset (timestamptz); plain timestamp ignores any suffix.
pub fn parse_timestamp(s: &str, apply_timezone: bool) -> Result<i64, SqlError> {
    // The type this input is for names the error, so `timestamptz` reports
    // `timestamp with time zone`, as PostgreSQL does.
    let type_name = if apply_timezone {
        "timestamp with time zone"
    } else {
        "timestamp"
    };
    let bad = || {
        sql_err!(
            sqlstate::INVALID_DATETIME_FORMAT,
            "invalid input syntax for type {}: \"{}\"",
            type_name,
            s
        )
    };
    let t = s.trim();
    // PostgreSQL's era marker applies to the date even when it follows a
    // timestamp time component. Consume it before the ordinary zone parser,
    // where `BC` would otherwise look like a named zone.
    let (t, bc) = match t.rsplit_once(' ') {
        Some((head, era)) if era.eq_ignore_ascii_case("BC") => (head.trim_end(), true),
        Some((head, era)) if era.eq_ignore_ascii_case("AD") => (head.trim_end(), false),
        _ => (t, false),
    };
    // Split date and time parts.
    let (date_part, rest) = match t.find([' ', 'T']) {
        Some(i) => (&t[..i], &t[i + 1..]),
        None => (t, ""),
    };
    // A syntactically bad date part is this timestamp's malformed input, not a
    // `date` error surfaced verbatim — so a 22007 from `parse_date` is remapped
    // to the timestamp type, while its 22008 out-of-range error is kept, since
    // an impossible date is an impossible timestamp too.
    let mut date_days =
        parse_date(date_part).map_err(|e| if e.sqlstate == "22007" { bad() } else { e })? as i64;
    if bc {
        let (year, month, day) = civil_from_days(date_days + PG_EPOCH_DAYS);
        date_days = days_from_civil(1 - year, month, day) - PG_EPOCH_DAYS;
    }

    if rest.is_empty() {
        return Ok(date_days * 86_400 * 1_000_000);
    }

    // Trailing *named* zone (`... Europe/Moscow`, `... EST`, `... UTC`): the
    // name resolves at the timestamp's own instant, so a historical rule
    // applies to a historical timestamp. Detected before the numeric-offset
    // split, since a name never contains the digits an offset must.
    let mut named_zone: Option<super::timezone::Timezone> = None;
    let rest = if let Some((head, name)) = rest.rsplit_once(' ') {
        let name = name.trim();
        let name_like = !name.is_empty()
            && name
                .chars()
                .all(|c| c.is_ascii_alphabetic() || c == '/' || c == '_')
            && name.chars().any(|c| c.is_ascii_alphabetic());
        if name_like {
            if name.eq_ignore_ascii_case("utc") || name.eq_ignore_ascii_case("gmt") {
                named_zone = Some(super::timezone::Timezone::utc());
            } else {
                named_zone = Some(super::timezone::lookup(name).ok_or_else(|| {
                    // PostgreSQL reports the name case-folded.
                    let mut folded = crate::util::StackStr::<64>::new();
                    use core::fmt::Write as _;
                    for c in name.chars() {
                        let _ = folded.write_char(c.to_ascii_lowercase());
                    }
                    sql_err!(
                        sqlstate::INVALID_PARAMETER_VALUE,
                        "time zone \"{}\" not recognized",
                        folded.as_str()
                    )
                })?);
            }
            head.trim_end()
        } else {
            rest
        }
    } else {
        rest
    };
    // Trailing zone: Z, +HH, +HH:MM, -HH, -HH:MM. Whether an offset was
    // written matters: a bare timestamptz literal is interpreted in the
    // *session* zone, as PostgreSQL reads it.
    let (time_part, timezone_seconds, explicit_offset) =
        if let Some(stripped) = rest.strip_suffix('Z') {
            (stripped, 0i64, true)
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
                (tp, sign * (h * 3600 + m * 60), true)
            } else {
                (rest, 0, false)
            }
        } else {
            (rest, 0, false)
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
            sqlstate::DATETIME_FIELD_OVERFLOW,
            "date/time field value out of range: \"{}\"",
            s
        ));
    }
    let mut total = date_days * 86_400_000_000 + (h * 3600 + m * 60 + sec) * 1_000_000 + micros;
    if apply_timezone {
        if let Some(zone) = named_zone {
            // The offset in effect at the given wall time (resolved with the
            // wall instant standing in for UTC — exact away from the sub-hour
            // transition windows, as the AT TIME ZONE conversion has it).
            let (offset_seconds, _) = zone.resolve(total);
            total -= i64::from(offset_seconds) * 1_000_000;
        } else if explicit_offset {
            total -= timezone_seconds * 1_000_000;
        } else {
            // No zone written: the wall time reads in the session zone.
            let (offset_seconds, _) = super::timezone::session().resolve(total);
            total -= i64::from(offset_seconds) * 1_000_000;
        }
    }
    Ok(total)
}

/// Parses a time of day, returning the microseconds since midnight and the
/// zone offset (seconds east) when the text carried one. Both `time` and
/// `timetz` come through here: PostgreSQL accepts a zone on either and simply
/// ignores it for the zoneless type, so `'12:00:00-05'::time` is `12:00:00`.
pub fn parse_timetz(s: &str) -> Result<(i64, Option<i32>), SqlError> {
    parse_time_parts(s, "time with time zone")
}

/// Microseconds since midnight, discarding any zone the text carried.
pub fn parse_time(s: &str) -> Result<i64, SqlError> {
    Ok(parse_time_parts(s, "time")?.0)
}

fn parse_time_parts(s: &str, type_name: &str) -> Result<(i64, Option<i32>), SqlError> {
    let bad = || {
        sql_err!(
            sqlstate::INVALID_DATETIME_FORMAT,
            "invalid input syntax for type {}: \"{}\"",
            type_name,
            s
        )
    };
    let t = s.trim();
    // Split off a trailing zone: `Z`, a named `UTC`, or `±HH[:MM[:SS]]`. The
    // sign has to be past the start so a lone offset is not read as a time.
    let (t, zone) = if let Some(stripped) = t.strip_suffix('Z').or_else(|| t.strip_suffix('z')) {
        (stripped, Some(0))
    } else if let Some((head, name)) = t.rsplit_once(' ') {
        let name = name.trim();
        // `UTC`/`GMT` are the zone names PostgreSQL accepts here that are not
        // in the region table; anything else goes through the usual lookup.
        if name.eq_ignore_ascii_case("utc") || name.eq_ignore_ascii_case("gmt") {
            (head, Some(0))
        } else {
            match super::timezone::lookup(name) {
                Some(z) => (head, Some(z.resolve(now_micros()).0)),
                None => return Err(bad()),
            }
        }
    } else {
        match t.rfind(['+', '-']) {
            Some(i) if i > 0 => {
                let (head, zone) = t.split_at(i);
                (head, Some(parse_zone_offset(zone).ok_or_else(bad)?))
            }
            _ => (t, None),
        }
    };
    let t = t.trim();
    let mut it = t.splitn(3, ':');
    let h: i64 = it
        .next()
        .and_then(|p| p.trim().parse().ok())
        .ok_or_else(bad)?;
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
    // 24:00:00 is the one hour-24 time PostgreSQL accepts.
    let hour_ok = (0..24).contains(&h) || (h == 24 && m == 0 && sec == 0 && micros == 0);
    if !hour_ok || !(0..60).contains(&m) || !(0..61).contains(&sec) {
        return Err(sql_err!(
            sqlstate::DATETIME_FIELD_OVERFLOW,
            "date/time field value out of range: \"{}\"",
            s
        ));
    }
    Ok(((h * 3600 + m * 60 + sec) * 1_000_000 + micros, zone))
}

/// `±HH`, `±HH:MM` or `±HH:MM:SS` as seconds east of UTC.
fn parse_zone_offset(zone: &str) -> Option<i32> {
    let sign = match zone.as_bytes().first()? {
        b'+' => 1i32,
        b'-' => -1,
        _ => return None,
    };
    let mut parts = zone[1..].split(':');
    let h: i32 = parts.next()?.parse().ok()?;
    let m: i32 = match parts.next() {
        Some(p) => p.parse().ok()?,
        None => 0,
    };
    let sec: i32 = match parts.next() {
        Some(p) => p.parse().ok()?,
        None => 0,
    };
    if parts.next().is_some()
        || !(0..=15).contains(&h)
        || !(0..60).contains(&m)
        || !(0..60).contains(&sec)
    {
        return None;
    }
    Some(sign * (h * 3600 + m * 60 + sec))
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
    let bad = || {
        sql_err!(
            sqlstate::INVALID_DATETIME_FORMAT,
            "invalid input syntax for type interval: \"{}\"",
            s
        )
    };
    let mut months = 0i64;
    let mut days = 0i64;
    let mut micros = 0i64;
    let overflow = || {
        sql_err!(
            sqlstate::INTERVAL_FIELD_OVERFLOW,
            "interval field value out of range: \"{}\"",
            s
        )
    };
    let add_micros = |total: &mut i64, value: f64| -> Result<(), SqlError> {
        if !value.is_finite() || value < i64::MIN as f64 || value > i64::MAX as f64 {
            return Err(overflow());
        }
        *total = total
            .checked_add(value.round() as i64)
            .ok_or_else(overflow)?;
        Ok(())
    };
    let add_days =
        |day_total: &mut i64, micro_total: &mut i64, value: f64| -> Result<(), SqlError> {
            if !value.is_finite() || value < i64::MIN as f64 || value > i64::MAX as f64 {
                return Err(overflow());
            }
            let whole = value.trunc();
            *day_total = day_total.checked_add(whole as i64).ok_or_else(overflow)?;
            add_micros(micro_total, (value - whole) * DAY_US as f64)
        };
    let add_months = |month_total: &mut i64,
                      day_total: &mut i64,
                      micro_total: &mut i64,
                      value: f64|
     -> Result<(), SqlError> {
        if !value.is_finite() || value < i64::MIN as f64 || value > i64::MAX as f64 {
            return Err(overflow());
        }
        let whole = value.trunc();
        *month_total = month_total.checked_add(whole as i64).ok_or_else(overflow)?;
        add_days(day_total, micro_total, (value - whole) * 30.0)
    };
    let mut it = s.split_whitespace().peekable();
    let mut saw = false;
    while let Some(tok) = it.next() {
        if tok.contains(':') {
            // Interval hours are a duration and are not restricted to a
            // clock's 0..24 range.
            let neg = tok.starts_with('-');
            let t = tok.trim_start_matches(['-', '+']);
            let mut parts = t.splitn(3, ':');
            let hour: i64 = parts
                .next()
                .and_then(|part| part.parse().ok())
                .ok_or_else(bad)?;
            let minute: i64 = parts
                .next()
                .and_then(|part| part.parse().ok())
                .ok_or_else(bad)?;
            let second: f64 = parts
                .next()
                .and_then(|part| part.parse().ok())
                .ok_or_else(bad)?;
            if !(0..60).contains(&minute) || !(0.0..60.0).contains(&second) {
                return Err(overflow());
            }
            let clock = (hour as f64 * 3600.0 + minute as f64 * 60.0 + second) * 1_000_000.0;
            if !clock.is_finite() || clock > i64::MAX as f64 {
                return Err(overflow());
            }
            let clock = clock.round() as i64;
            micros = micros
                .checked_add(if neg { -clock } else { clock })
                .ok_or_else(overflow)?;
            saw = true;
            continue;
        }
        // A signed number, optionally followed by a unit word. A number with
        // no unit is seconds, as PostgreSQL reads a bare `INTERVAL '90'`.
        let n: f64 = tok.parse().map_err(|_| bad())?;
        let Some(unit) = it.next() else {
            add_micros(&mut micros, n * 1_000_000.0)?;
            saw = true;
            continue;
        };
        let u = unit.trim_end_matches('s'); // singular/plural
        match u {
            "year" | "yr" => add_months(&mut months, &mut days, &mut micros, n * 12.0)?,
            "month" | "mon" => add_months(&mut months, &mut days, &mut micros, n)?,
            "week" | "wk" => add_days(&mut days, &mut micros, n * 7.0)?,
            "day" | "d" => add_days(&mut days, &mut micros, n)?,
            "hour" | "hr" | "h" => add_micros(&mut micros, n * 3_600_000_000.0)?,
            "minute" | "min" | "m" => add_micros(&mut micros, n * 60_000_000.0)?,
            "second" | "sec" | "s" => add_micros(&mut micros, n * 1_000_000.0)?,
            "millisecond" | "msec" | "ms" => add_micros(&mut micros, n * 1_000.0)?,
            "microsecond" | "usec" | "us" => add_micros(&mut micros, n)?,
            _ => return Err(bad()),
        }
        saw = true;
    }
    if !saw {
        return Err(bad());
    }
    Ok(Interval {
        months: months.try_into().map_err(|_| overflow())?,
        days: days.try_into().map_err(|_| overflow())?,
        micros,
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IntervalStyle {
    Postgres,
    PostgresVerbose,
    SqlStandard,
    Iso8601,
}

impl IntervalStyle {
    pub fn parse(value: &str) -> Option<Self> {
        Some(if value.eq_ignore_ascii_case("postgres") {
            Self::Postgres
        } else if value.eq_ignore_ascii_case("postgres_verbose") {
            Self::PostgresVerbose
        } else if value.eq_ignore_ascii_case("sql_standard") {
            Self::SqlStandard
        } else if value.eq_ignore_ascii_case("iso_8601") {
            Self::Iso8601
        } else {
            return None;
        })
    }

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Postgres => "postgres",
            Self::PostgresVerbose => "postgres_verbose",
            Self::SqlStandard => "sql_standard",
            Self::Iso8601 => "iso_8601",
        }
    }
}

pub fn format_interval(interval: super::types::Interval) -> StackStr<96> {
    format_interval_styled(interval, IntervalStyle::Postgres)
}

pub fn format_interval_styled(
    interval: super::types::Interval,
    style: IntervalStyle,
) -> StackStr<96> {
    match style {
        IntervalStyle::Postgres => format_interval_postgres(interval),
        IntervalStyle::PostgresVerbose => format_interval_verbose(interval),
        IntervalStyle::SqlStandard => format_interval_sql_standard(interval),
        IntervalStyle::Iso8601 => format_interval_iso8601(interval),
    }
}

/// PostgreSQL's default style: named calendar fields and a clock field.
fn format_interval_postgres(interval: super::types::Interval) -> StackStr<96> {
    use core::fmt::Write;
    let mut out = StackStr::<96>::new();
    let years = interval.months / 12;
    let mons = interval.months % 12;
    let mut first = true;
    // PostgreSQL gives a positive field an explicit `+` only when the field
    // printed just before it was negative — `-1 mons +5 days`, but a run of
    // all-positive fields stays bare. `prev_neg` tracks that immediately
    // preceding sign.
    let mut prev_neg = false;
    let sep = |out: &mut StackStr<96>, first: &mut bool| {
        if !*first {
            let _ = write!(out, " ");
        }
        *first = false;
    };
    let unit =
        |out: &mut StackStr<96>, first: &mut bool, prev_neg: &mut bool, n: i32, singular: &str| {
            if n != 0 {
                sep(out, first);
                if n > 0 && *prev_neg {
                    let _ = write!(out, "+");
                }
                let _ = write!(out, "{n} {singular}");
                if n != 1 {
                    let _ = write!(out, "s");
                }
                *prev_neg = n < 0;
            }
        };
    unit(&mut out, &mut first, &mut prev_neg, years, "year");
    unit(&mut out, &mut first, &mut prev_neg, mons, "mon");
    unit(&mut out, &mut first, &mut prev_neg, interval.days, "day");
    if interval.micros != 0 || (interval.months == 0 && interval.days == 0) {
        // The clock takes the same rule: `+` when it is positive and the field
        // before it was negative, `-` when it is itself negative.
        sep(&mut out, &mut first);
        let neg = interval.micros < 0;
        let a = interval.micros.unsigned_abs();
        let seconds = a / 1_000_000;
        let frac = a % 1_000_000;
        let (h, m, s) = (seconds / 3600, (seconds % 3600) / 60, seconds % 60);
        if neg {
            let _ = write!(out, "-");
        } else if prev_neg {
            let _ = write!(out, "+");
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

fn write_fraction(out: &mut StackStr<96>, fraction: u64) {
    use core::fmt::Write;
    if fraction == 0 {
        return;
    }
    let mut value = fraction;
    let mut digits = [0_u8; 6];
    for digit in digits.iter_mut().rev() {
        *digit = (value % 10) as u8;
        value /= 10;
    }
    let mut length = digits.len();
    while digits[length - 1] == 0 {
        length -= 1;
    }
    let _ = out.write_char('.');
    for digit in &digits[..length] {
        let _ = write!(out, "{digit}");
    }
}

fn format_interval_verbose(interval: super::types::Interval) -> StackStr<96> {
    use core::fmt::Write;
    let before = interval.months < 0
        || interval.months == 0 && interval.days < 0
        || interval.months == 0 && interval.days == 0 && interval.micros < 0;
    let direction = if before { -1_i64 } else { 1_i64 };
    let months = i64::from(interval.months) * direction;
    let days = i64::from(interval.days) * direction;
    let micros = i128::from(interval.micros) * i128::from(direction);
    let mut out = StackStr::<96>::from_str("@ ");
    let mut wrote = false;
    let mut unit = |value: i64, singular: &str| {
        if value == 0 {
            return;
        }
        if wrote {
            let _ = out.write_char(' ');
        }
        let _ = write!(out, "{value} {singular}");
        if value.unsigned_abs() != 1 {
            let _ = out.write_char('s');
        }
        wrote = true;
    };
    unit(months / 12, "year");
    unit(months % 12, "mon");
    unit(days, "day");
    let negative_time = micros < 0;
    let absolute = micros.unsigned_abs();
    let seconds = absolute / 1_000_000;
    let fraction = (absolute % 1_000_000) as u64;
    let hours = seconds / 3600;
    let minutes = seconds % 3600 / 60;
    let second = seconds % 60;
    let sign = if negative_time { -1 } else { 1 };
    unit((hours as i64) * sign, "hour");
    unit((minutes as i64) * sign, "min");
    if second != 0 || fraction != 0 {
        if wrote {
            let _ = out.write_char(' ');
        }
        if negative_time {
            let _ = out.write_char('-');
        }
        let _ = write!(out, "{second}");
        write_fraction(&mut out, fraction);
        let _ = out.write_str(if second == 1 && fraction == 0 {
            " sec"
        } else {
            " secs"
        });
        wrote = true;
    }
    if !wrote {
        let _ = out.write_char('0');
    }
    if before {
        let _ = out.write_str(" ago");
    }
    out
}

fn interval_signs(interval: super::types::Interval) -> (bool, bool) {
    let negative = interval.months < 0 || interval.days < 0 || interval.micros < 0;
    let positive = interval.months > 0 || interval.days > 0 || interval.micros > 0;
    (negative, positive)
}

enum SqlClockSign {
    Own,
    Explicit,
    SharedWithDay,
}

fn write_sql_clock(out: &mut StackStr<96>, micros: i64, sign: SqlClockSign) {
    use core::fmt::Write;
    match sign {
        SqlClockSign::Own if micros < 0 => {
            let _ = out.write_char('-');
        }
        SqlClockSign::Explicit => {
            let _ = out.write_char(if micros < 0 { '-' } else { '+' });
        }
        SqlClockSign::Own | SqlClockSign::SharedWithDay => {}
    }
    let value = micros.unsigned_abs();
    let seconds = value / 1_000_000;
    let fraction = value % 1_000_000;
    let _ = write!(
        out,
        "{}:{:02}:{:02}",
        seconds / 3600,
        seconds % 3600 / 60,
        seconds % 60
    );
    write_fraction(out, fraction);
}

fn format_interval_sql_standard(interval: super::types::Interval) -> StackStr<96> {
    use core::fmt::Write;
    if interval.months == 0 && interval.days == 0 && interval.micros == 0 {
        return StackStr::from_str("0");
    }
    let (negative, positive) = interval_signs(interval);
    let separate_groups = negative && positive
        || interval.months != 0 && (interval.days != 0 || interval.micros != 0);
    let mut out = StackStr::<96>::new();
    if separate_groups {
        let month_sign = if interval.months < 0 { '-' } else { '+' };
        let months = i64::from(interval.months).unsigned_abs();
        let _ = write!(out, "{month_sign}{}-{}", months / 12, months % 12);
        let day_sign = if interval.days < 0 { '-' } else { '+' };
        let _ = write!(
            out,
            " {day_sign}{} ",
            i64::from(interval.days).unsigned_abs()
        );
        write_sql_clock(&mut out, interval.micros, SqlClockSign::Explicit);
    } else if interval.months != 0 {
        if interval.months < 0 {
            let _ = out.write_char('-');
        }
        let months = i64::from(interval.months).unsigned_abs();
        let _ = write!(out, "{}-{}", months / 12, months % 12);
    } else if interval.days != 0 {
        let _ = write!(out, "{} ", interval.days);
        write_sql_clock(&mut out, interval.micros, SqlClockSign::SharedWithDay);
    } else {
        write_sql_clock(&mut out, interval.micros, SqlClockSign::Own);
    }
    out
}

fn format_interval_iso8601(interval: super::types::Interval) -> StackStr<96> {
    use core::fmt::Write;
    let mut out = StackStr::<96>::from_str("P");
    let years = interval.months / 12;
    let months = interval.months % 12;
    if years != 0 {
        let _ = write!(out, "{years}Y");
    }
    if months != 0 {
        let _ = write!(out, "{months}M");
    }
    if interval.days != 0 {
        let _ = write!(out, "{}D", interval.days);
    }
    if interval.micros != 0 {
        let _ = out.write_char('T');
        let negative = interval.micros < 0;
        let absolute = interval.micros.unsigned_abs();
        let seconds = absolute / 1_000_000;
        let fraction = absolute % 1_000_000;
        let hours = seconds / 3600;
        let minutes = seconds % 3600 / 60;
        let second = seconds % 60;
        let sign = if negative { "-" } else { "" };
        if hours != 0 {
            let _ = write!(out, "{sign}{hours}H");
        }
        if minutes != 0 {
            let _ = write!(out, "{sign}{minutes}M");
        }
        if second != 0 || fraction != 0 {
            let _ = write!(out, "{sign}{second}");
            write_fraction(&mut out, fraction);
            let _ = out.write_char('S');
        }
    } else if interval.months == 0 && interval.days == 0 {
        let _ = out.write_str("T0S");
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
pub fn interval_scale(
    interval: super::types::Interval,
    factor: f64,
    div: bool,
) -> super::types::Interval {
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
    super::types::Interval {
        months,
        days,
        micros,
    }
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
    let (hi, lo) = if neg {
        (timestamp2, timestamp1)
    } else {
        (timestamp1, timestamp2)
    };
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
        super::types::Interval {
            months: -interval.months,
            days: -interval.days,
            micros: -interval.micros,
        }
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
        DateStyle {
            format: DateFormat::Iso,
            order: FieldOrder::Mdy,
        }
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
    format_timestamp_styled(
        micros,
        with_timezone,
        DateStyle::default(),
        crate::sql::timezone::Timezone::utc(),
    )
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
    let (timezone_offset_seconds, abbrev) = if with_timezone {
        timezone.resolve(micros)
    } else {
        (0, StackStr::<8>::new())
    };
    let timezone_abbreviation = abbrev.as_str();
    let local = micros + timezone_offset_seconds as i64 * 1_000_000;
    let days = local.div_euclid(DAY_US);
    let in_day = local.rem_euclid(DAY_US);
    let (y, m, d) = civil_from_days(days + PG_EPOCH_DAYS);
    let seconds = in_day / 1_000_000;
    let frac = in_day % 1_000_000;
    let (h, minute, s) = (seconds / 3600, (seconds / 60) % 60, seconds % 60);
    let dmy = style.order == FieldOrder::Dmy;
    let mut out = StackStr::<48>::new();
    use core::fmt::Write;

    match style.format {
        DateFormat::Iso => {
            let _ = write!(out, "{y:04}-{m:02}-{d:02} {h:02}:{minute:02}:{s:02}");
            write_frac(&mut out, frac);
            if with_timezone {
                write_iso_offset(&mut out, timezone_offset_seconds);
            }
        }
        DateFormat::Postgres => {
            let dow = DOW[day_of_week(days)];
            let month = MON[(m - 1) as usize];
            if dmy {
                let _ = write!(out, "{dow} {d:02} {month} {h:02}:{minute:02}:{s:02}");
            } else {
                let _ = write!(out, "{dow} {month} {d:02} {h:02}:{minute:02}:{s:02}");
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
            let _ = write!(out, " {h:02}:{minute:02}:{s:02}");
            write_frac(&mut out, frac);
            if with_timezone {
                let _ = write!(out, " {timezone_abbreviation}");
            }
        }
    }
    out
}

/// ISO 8601 timestamp for JSON output — the form `to_json` / `row_to_json` /
/// `to_jsonb` render date-time types in: a `T` between date and time, and, for
/// `timestamptz`, a full `+HH:MM` offset (never trimmed to `+HH`). The value is
/// UTC (as the whole `Datum` Display path renders timestamps), so the offset is
/// `+00:00` — the session-zone shift PostgreSQL also applies here is the same
/// limitation the plain `::text` render carries.
pub fn format_timestamp_json(micros: i64, with_timezone: bool) -> StackStr<48> {
    let days = micros.div_euclid(DAY_US);
    let in_day = micros.rem_euclid(DAY_US);
    let (y, m, d) = civil_from_days(days + PG_EPOCH_DAYS);
    let seconds = in_day / 1_000_000;
    let frac = in_day % 1_000_000;
    let (h, minute, s) = (seconds / 3600, (seconds / 60) % 60, seconds % 60);
    let mut out = StackStr::<48>::new();
    use core::fmt::Write;
    let _ = write!(out, "{y:04}-{m:02}-{d:02}T{h:02}:{minute:02}:{s:02}");
    write_frac(&mut out, frac);
    if with_timezone {
        let _ = out.write_str("+00:00");
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
use core::cell::Cell;
std::thread_local! {
    /// When the running statement started, and when its transaction did.
    /// PostgreSQL freezes `now()`/`current_timestamp` at transaction start and
    /// `statement_timestamp` at statement start, leaving only
    /// `clock_timestamp` reading the clock; taking the clock afresh per call
    /// instead lets two `now()`s in one statement differ. Single-threaded per
    /// connection, like the statement deadline and the session zone.
    static STATEMENT_START: Cell<i64> = const { Cell::new(0) };
    static TRANSACTION_START: Cell<i64> = const { Cell::new(0) };
}

/// Marks the start of a statement.
pub fn begin_statement() {
    STATEMENT_START.with(|t| t.set(now_micros()));
}

/// Marks the start of a transaction, which always begins at some statement —
/// so it anchors to that statement's clock rather than taking its own reading.
/// A lone statement is its own implicit transaction, and PostgreSQL has
/// `now() = statement_timestamp()` there; a second reading would not.
pub fn begin_transaction() {
    TRANSACTION_START.with(|t| t.set(statement_micros()));
}

/// Releases the transaction anchor at commit or rollback; the next statement
/// takes a fresh one.
pub fn end_transaction() {
    TRANSACTION_START.with(|t| t.set(0));
}

/// `statement_timestamp()`: fixed for the running statement.
pub fn statement_micros() -> i64 {
    let at = STATEMENT_START.with(|t| t.get());
    if at == 0 { now_micros() } else { at }
}

/// `now()` / `current_timestamp` / `transaction_timestamp()`: fixed for the
/// running transaction.
pub fn transaction_micros() -> i64 {
    let at = TRANSACTION_START.with(|t| t.get());
    if at == 0 { statement_micros() } else { at }
}

/// `clock_timestamp()`: the actual wall clock, read afresh each call.
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
        Interval {
            months,
            days,
            micros,
        }
    }

    // Reference values captured from PostgreSQL 18.4.
    #[test]
    fn interval_scale_matches_pg() {
        // interval '1 month' * 1.5 = 1 month 15 days (fractional month -> days).
        assert_eq!(
            interval_scale(interval(1, 0, 0), 1.5, false),
            interval(1, 15, 0)
        );
        // interval '1 day' / 2 = 12:00:00 (fractional day -> time).
        assert_eq!(
            interval_scale(interval(0, 1, 0), 2.0, true),
            interval(0, 0, 43_200_000_000)
        );
        // interval '10 days' / 3 = 3 days 08:00:00.
        assert_eq!(
            interval_scale(interval(0, 10, 0), 3.0, true),
            interval(0, 3, 28_800_000_000)
        );
        // interval '2 hours' * 2.5 = 05:00:00.
        assert_eq!(
            interval_scale(interval(0, 0, 7_200_000_000), 2.5, false),
            interval(0, 0, 18_000_000_000)
        );
    }

    #[test]
    fn justify_matches_pg() {
        // 36 hours -> 1 day 12:00:00.
        assert_eq!(
            justify_hours(interval(0, 0, 129_600_000_000)),
            interval(0, 1, 43_200_000_000)
        );
        // 35 days -> 1 month 5 days.
        assert_eq!(justify_days(interval(0, 35, 0)), interval(1, 5, 0));
        // 1 month -1 hour -> 29 days 23:00:00.
        assert_eq!(
            justify_interval(interval(1, 0, -3_600_000_000)),
            interval(0, 29, 82_800_000_000)
        );
    }

    #[test]
    fn age_matches_pg() {
        let timestamp = |s: &str| parse_timestamp(s, false).unwrap();
        assert_eq!(
            age_between(timestamp("2024-06-15"), timestamp("2020-01-10")),
            interval(53, 5, 0)
        );
        // Reversed arguments negate every field.
        assert_eq!(
            age_between(timestamp("2020-01-10"), timestamp("2024-06-15")),
            interval(-53, -5, 0)
        );
        // Day borrow uses the earlier date's own month length.
        assert_eq!(
            age_between(timestamp("2024-03-01"), timestamp("2024-01-31")),
            interval(1, 1, 0)
        );
        assert_eq!(
            age_between(timestamp("2000-01-01"), timestamp("1999-02-05")),
            interval(10, 24, 0)
        );
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
        let mdy = |f| DateStyle {
            format: f,
            order: FieldOrder::Mdy,
        };
        let dmy = |f| DateStyle {
            format: f,
            order: FieldOrder::Dmy,
        };
        let cases = [
            (
                mdy(DateFormat::Iso),
                "2024-01-15",
                "2024-01-15 14:30:00",
                "2024-01-15 14:30:00.5",
                "2024-01-15 14:30:00+00",
            ),
            (
                mdy(DateFormat::Postgres),
                "01-15-2024",
                "Mon Jan 15 14:30:00 2024",
                "Mon Jan 15 14:30:00.5 2024",
                "Mon Jan 15 14:30:00 2024 UTC",
            ),
            (
                dmy(DateFormat::Postgres),
                "15-01-2024",
                "Mon 15 Jan 14:30:00 2024",
                "Mon 15 Jan 14:30:00.5 2024",
                "Mon 15 Jan 14:30:00 2024 UTC",
            ),
            (
                mdy(DateFormat::Sql),
                "01/15/2024",
                "01/15/2024 14:30:00",
                "01/15/2024 14:30:00.5",
                "01/15/2024 14:30:00 UTC",
            ),
            (
                dmy(DateFormat::Sql),
                "15/01/2024",
                "15/01/2024 14:30:00",
                "15/01/2024 14:30:00.5",
                "15/01/2024 14:30:00 UTC",
            ),
            (
                mdy(DateFormat::German),
                "15.01.2024",
                "15.01.2024 14:30:00",
                "15.01.2024 14:30:00.5",
                "15.01.2024 14:30:00 UTC",
            ),
        ];
        for (style, d_exp, ts_exp, tsf_exp, tstz_exp) in cases {
            assert_eq!(
                format_date_styled(days, style).as_str(),
                d_exp,
                "{style:?} date"
            );
            assert_eq!(
                format_timestamp_styled(
                    timestamp,
                    false,
                    style,
                    crate::sql::timezone::Timezone::utc()
                )
                .as_str(),
                ts_exp,
                "{style:?} timestamp"
            );
            assert_eq!(
                format_timestamp_styled(tsf, false, style, crate::sql::timezone::Timezone::utc())
                    .as_str(),
                tsf_exp,
                "{style:?} tsf"
            );
            assert_eq!(
                format_timestamp_styled(
                    timestamp,
                    true,
                    style,
                    crate::sql::timezone::Timezone::utc()
                )
                .as_str(),
                tstz_exp,
                "{style:?} tstz"
            );
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
        assert_eq!(
            make_date(2024, 6, 15).unwrap(),
            parse_date("2024-06-15").unwrap()
        );
        assert_eq!(make_date(2000, 1, 1).unwrap(), 0);
        assert!(make_date(2024, 13, 1).is_err());
        assert!(make_date(2024, 2, 30).is_err());
        // make_time counts microseconds since midnight.
        assert_eq!(
            make_time(12, 30, 0.0).unwrap(),
            ((12 * 60 + 30) * 60) * 1_000_000
        );
        assert_eq!(make_time(0, 0, 45.5).unwrap(), 45_500_000);
        assert!(make_time(24, 0, 0.0).is_err());
        assert!(make_time(0, 0, 60.0).is_err());
        // make_timestamp combines the two.
        assert_eq!(
            make_timestamp(2024, 6, 15, 12, 30, 0.0).unwrap(),
            make_date(2024, 6, 15).unwrap() as i64 * 86_400_000_000
                + make_time(12, 30, 0.0).unwrap()
        );
    }

    #[test]
    fn to_date_parses_formats() {
        let d = parse_date("2024-06-15").unwrap();
        assert_eq!(to_date("2024-06-15", "YYYY-MM-DD").unwrap(), d);
        assert_eq!(to_date("15/06/2024", "DD/MM/YYYY").unwrap(), d);
        assert_eq!(to_date("06-15-2024", "MM-DD-YYYY").unwrap(), d);
        assert_eq!(to_date("240615", "YYMMDD").unwrap(), d);
        assert_eq!(
            to_date("2024-6-5", "YYYY-MM-DD").unwrap(),
            parse_date("2024-06-05").unwrap()
        );
        assert_eq!(to_date("Jun 15 2024", "Mon DD YYYY").unwrap(), d);
        assert_eq!(
            to_timestamp("2024-06-15 12:30:45", "YYYY-MM-DD HH24:MI:SS").unwrap(),
            parse_timestamp("2024-06-15 12:30:45", false).unwrap()
        );
        assert!(to_date("2024-13-01", "YYYY-MM-DD").is_err());
    }

    #[test]
    fn format_model_calendar_inputs_are_resolved_once() {
        assert_eq!(
            parse_timestamp("0001-01-01 BC", false).unwrap(),
            make_timestamp(0, 1, 1, 0, 0, 0.0).unwrap()
        );
        let leap = make_date(2024, 2, 29).unwrap();
        assert_eq!(to_date("2024-060", "YYYY-DDD").unwrap(), leap);
        assert_eq!(
            to_date("2020-53-5", "IYYY-IW-ID").unwrap(),
            make_date(2021, 1, 1).unwrap()
        );
        assert_eq!(
            to_date("XII-31-2024", "RM-DD-YYYY").unwrap(),
            make_date(2024, 12, 31).unwrap()
        );
        assert_eq!(
            to_date("0001 BC", "YYYY BC").unwrap(),
            make_date(0, 1, 1).unwrap()
        );
        assert_eq!(
            to_timestamp(
                "2021-01-01 01:02:03.456789 PM",
                "YYYY-MM-DD HH12:MI:SS.US AM"
            )
            .unwrap(),
            make_timestamp(2021, 1, 1, 13, 2, 3.456789).unwrap()
        );
        assert_eq!(
            to_timestamp("46923", "SSSS").unwrap(),
            make_timestamp(2000, 1, 1, 13, 2, 3.0).unwrap()
        );
        assert_eq!(
            to_date("2460370", "J").unwrap(),
            make_date(2024, 2, 29).unwrap()
        );
        assert_eq!(
            to_date("2024-060", "IYYY-IDDD").unwrap(),
            make_date(2024, 2, 29).unwrap()
        );
        let zoned = to_timestamp(
            "2024-02-29 23:07:05.123456 -05:30",
            "YYYY-MM-DD HH24:MI:SS.FF6 TZH:TZM",
        )
        .unwrap();
        assert_eq!(zoned, make_timestamp(2024, 3, 1, 4, 37, 5.123456).unwrap());
        assert!(to_date("2024  02 29", "FXYYYY MM DD").is_err());
        assert_eq!(to_date("5", "Y").unwrap(), make_date(2005, 1, 1).unwrap());
        assert_eq!(
            to_date("999", "YYY").unwrap(),
            make_date(1999, 1, 1).unwrap()
        );
        assert_eq!(
            to_date("22-24-2nd-Monday", "CC-YY-Wth-DAY").unwrap(),
            make_date(2124, 1, 8).unwrap()
        );
        assert_eq!(to_date("21", "CC").unwrap(), make_date(2001, 1, 1).unwrap());
        assert!(to_date("2024-060", "YYYY-IDDD").is_err());
        assert!(to_date("2460370-2024", "J-YYYY").is_err());
    }

    #[test]
    fn timestamp_roundtrip() {
        let t = parse_timestamp("2000-01-01 00:00:00", false).unwrap();
        assert_eq!(t, 0);
        assert_eq!(format_timestamp(t, false).as_str(), "2000-01-01 00:00:00");

        let t = parse_timestamp("2024-06-15 12:34:56.789", false).unwrap();
        assert_eq!(
            format_timestamp(t, false).as_str(),
            "2024-06-15 12:34:56.789"
        );
        assert_eq!(
            format_timestamp(t, true).as_str(),
            "2024-06-15 12:34:56.789+00"
        );

        // Zone shifting for timestamptz.
        let utc = parse_timestamp("2024-01-01 12:00:00+00", true).unwrap();
        let plus2 = parse_timestamp("2024-01-01 14:00:00+02", true).unwrap();
        assert_eq!(utc, plus2);
        let z = parse_timestamp("2024-01-01T12:00:00Z", true).unwrap();
        assert_eq!(utc, z);
    }
}
