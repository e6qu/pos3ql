//! Date and time text input, ported from PostgreSQL 18.6
//! [`datetime.c`](https://github.com/postgres/postgres/blob/REL_18_6/src/backend/utils/adt/datetime.c)
//! (`ParseDateTime`, `DecodeDateTime`, `DecodeTimeOnly` and their helpers)
//! and its `date_in`, `time_in`, `timetz_in` (`date.c`), `timestamp_in` and
//! `timestamptz_in` (`timestamp.c`) callers. PostgreSQL License:
//! vendor/test/postgres-regress/COPYRIGHT.
//!
//! `ParseDateTime` splits the text into typed fields; the decoders read them
//! in context: numbers in the session DateStyle field order (MDY, DMY or YMD),
//! month and weekday names, `J` Julian days, ISO 8601 `T` forms, run-together
//! `yyyymmdd`/`hhmmss` numbers, BC/AD, AM/PM, numeric zones, zone names and
//! abbreviations, and the special words. The field masks, their order of
//! checks and the error each check raises follow PostgreSQL exactly, so the
//! same input fails with the same SQLSTATE and message.

use core::cell::Cell;

use super::datetime::{
    DATE_INFINITY, DATE_NEG_INFINITY, FieldOrder, PG_EPOCH_SECS, TIMESTAMP_INFINITY,
    TIMESTAMP_NEG_INFINITY, civil_from_days, days_from_civil,
};
use super::eval::{SqlError, sqlstate};
use super::timezone::Timezone;
use crate::sql_err;
use crate::util::StackStr;

std::thread_local! {
    /// The session's DateStyle field order, published with its TimeZone before
    /// each statement, as input reads both.
    static SESSION_FIELD_ORDER: Cell<FieldOrder> = const { Cell::new(FieldOrder::Mdy) };
}

/// Publishes the DateStyle field order that input reads numbers in.
pub fn set_session_field_order(order: FieldOrder) {
    SESSION_FIELD_ORDER.with(|value| value.set(order));
}

fn session_field_order() -> FieldOrder {
    SESSION_FIELD_ORDER.with(Cell::get)
}

const MAX_DATE_FIELDS: usize = 25;
const MAX_DATE_LEN: usize = 128;
/// `date_in`, `time_in` and `timetz_in` give `ParseDateTime` `MAXDATELEN + 1`
/// bytes of field storage; timestamp input gives `MAXDATELEN + MAXDATEFIELDS`.
/// Each field also takes a terminator byte, and longer input is malformed.
const DATE_FIELD_BYTES: usize = MAX_DATE_LEN + 1;
const TIMESTAMP_FIELD_BYTES: usize = MAX_DATE_LEN + MAX_DATE_FIELDS;

const POSTGRES_EPOCH_JDATE: i64 = 2_451_545;
const UNIX_EPOCH_JDATE: i64 = 2_440_588;
const USECS_PER_SEC: i64 = 1_000_000;
const USECS_PER_DAY: i64 = 86_400_000_000;
const MIN_TIMESTAMP: i64 = -211_813_488_000_000_000;
const END_TIMESTAMP: i64 = 9_223_371_331_200_000_000;
/// Julian days 0 (4714-11-24 BC) to `date2j(5874898, 1, 1)`.
const DATE_END_JULIAN: i64 = 2_147_483_494;
/// `MAX_TZDISP_HOUR`: numeric zones span at most ±15:59:59.
const MAX_ZONE_HOUR: i32 = 15;

// Field-type bits from PostgreSQL's datetime.h: the masks record which parts
// of the value the input has given, and a part given twice is malformed.
const MONTH: u32 = 1;
const YEAR: u32 = 2;
const DAY: u32 = 3;
const TZ: u32 = 5;
const DTZ: u32 = 6;
const DYNTZ: u32 = 7;
const AMPM: u32 = 9;
const HOUR: u32 = 10;
const MINUTE: u32 = 11;
const SECOND: u32 = 12;
const MILLISECOND: u32 = 13;
const MICROSECOND: u32 = 14;
const DOY: u32 = 15;
const DOW: u32 = 16;
const ADBC: u32 = 18;
const DTZMOD: u32 = 28;

const fn mask(bit: u32) -> u32 {
    1 << bit
}

const DATE_M: u32 = mask(YEAR) | mask(MONTH) | mask(DAY);
const TIME_M: u32 =
    mask(HOUR) | mask(MINUTE) | mask(SECOND) | mask(MILLISECOND) | mask(MICROSECOND);

/// `DTERR_*`: how decoding failed, which selects the SQLSTATE and message.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DecodeError {
    BadFormat,
    FieldOverflow,
    /// A month or day no month has: PostgreSQL hints at another DateStyle.
    MonthDayOverflow,
    ZoneDisplacement,
    /// A zone name with embedded punctuation that names no zone.
    UnknownZone {
        start: u16,
        len: u16,
    },
}

type Decode<T> = Result<T, DecodeError>;

impl DecodeError {
    /// `DateTimeParseError`.
    fn into_sql(self, fields: &Fields, input: &str, type_name: &str) -> SqlError {
        match self {
            DecodeError::BadFormat => bad_format(input, type_name),
            DecodeError::FieldOverflow => sql_err!(
                sqlstate::DATETIME_FIELD_OVERFLOW,
                "date/time field value out of range: \"{}\"",
                input
            ),
            DecodeError::MonthDayOverflow => {
                crate::sql::eval::stash_diagnostic(
                    None,
                    Some(StackStr::from_str(
                        "Perhaps you need a different \"DateStyle\" setting.",
                    )),
                );
                sql_err!(
                    sqlstate::DATETIME_FIELD_OVERFLOW,
                    "date/time field value out of range: \"{}\"",
                    input
                )
            }
            DecodeError::ZoneDisplacement => sql_err!(
                sqlstate::INVALID_TIME_ZONE_DISPLACEMENT_VALUE,
                "time zone displacement out of range: \"{}\"",
                input
            ),
            DecodeError::UnknownZone { start, len } => {
                let start = usize::from(start);
                let name = &fields.text[start..start + usize::from(len)];
                sql_err!(
                    sqlstate::INVALID_PARAMETER_VALUE,
                    "time zone \"{}\" not recognized",
                    core::str::from_utf8(name).expect("datetime fields are ASCII")
                )
            }
        }
    }
}

fn bad_format(input: &str, type_name: &str) -> SqlError {
    sql_err!(
        sqlstate::INVALID_DATETIME_FORMAT,
        "invalid input syntax for type {}: \"{}\"",
        type_name,
        input
    )
}

/// Splits and decodes `input` for `type_name`, raising PostgreSQL's error.
fn decode<T>(
    input: &str,
    capacity: usize,
    type_name: &str,
    decoder: impl FnOnce(&Fields, FieldOrder) -> Decode<T>,
) -> Result<T, SqlError> {
    let fields = parse_fields(input, capacity).ok_or_else(|| bad_format(input, type_name))?;
    decoder(&fields, session_field_order())
        .map_err(|error| error.into_sql(&fields, input, type_name))
}

/// `ParseDateTime`'s field types.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FieldKind {
    /// Digits with at most one embedded `.` (`20240105`, `040506.789`, `.5`).
    Number,
    /// Letters only (`jan`, `pm`, `est`).
    String,
    /// Digits with two delimiters, digits and text (`2024-01-05`,
    /// `5-jan-2024`), or a zone name with punctuation (`america/new_york`).
    Date,
    /// Digits with colons (`12:34:56.7`).
    Time,
    /// A sign followed by digits: a numeric zone (`-07:00`).
    Zone,
    /// A sign followed by letters (`-infinity`).
    Special,
}

/// The fields of one input, lowercased, in fixed storage.
struct Fields {
    text: [u8; TIMESTAMP_FIELD_BYTES],
    spans: [(u16, u16, FieldKind); MAX_DATE_FIELDS],
    count: usize,
}

impl Fields {
    fn field(&self, index: usize) -> (&[u8], FieldKind) {
        let (start, len, kind) = self.spans[index];
        let start = usize::from(start);
        (&self.text[start..start + usize::from(len)], kind)
    }

    fn kind(&self, index: usize) -> FieldKind {
        self.spans[index].2
    }

    fn unknown_zone(&self, index: usize) -> DecodeError {
        let (start, len, _) = self.spans[index];
        DecodeError::UnknownZone { start, len }
    }
}

fn is_space(byte: u8) -> bool {
    matches!(byte, b' ' | b'\t' | b'\n' | 0x0b | 0x0c | b'\r')
}

/// Field text being collected, with `ParseDateTime`'s storage limit.
struct FieldBuffer {
    text: [u8; TIMESTAMP_FIELD_BYTES],
    used: usize,
    capacity: usize,
}

impl FieldBuffer {
    /// Appends one lowercased byte; input too long for the storage is
    /// malformed.
    fn push(&mut self, byte: u8) -> Option<()> {
        if self.used + 1 >= self.capacity {
            return None;
        }
        self.text[self.used] = byte.to_ascii_lowercase();
        self.used += 1;
        Some(())
    }
}

/// `ParseDateTime`: splits `input` into at most 25 typed fields whose text,
/// with one terminator byte each, must fit `capacity` bytes. `None` is
/// malformed input.
fn parse_fields(input: &str, capacity: usize) -> Option<Fields> {
    let input = input.as_bytes();
    let mut buffer = FieldBuffer {
        text: [0; TIMESTAMP_FIELD_BYTES],
        used: 0,
        capacity,
    };
    let mut spans = [(0u16, 0u16, FieldKind::Number); MAX_DATE_FIELDS];
    let mut count = 0;
    let mut at = 0usize;
    let peek = |at: usize| input.get(at).copied().unwrap_or(0);
    while at < input.len() {
        if is_space(input[at]) {
            at += 1;
            continue;
        }
        if count >= MAX_DATE_FIELDS {
            return None;
        }
        let start = buffer.used;
        let first = input[at];
        let kind;
        if first.is_ascii_digit() {
            while peek(at).is_ascii_digit() {
                buffer.push(input[at])?;
                at += 1;
            }
            if peek(at) == b':' {
                kind = FieldKind::Time;
                buffer.push(b':')?;
                at += 1;
                while matches!(peek(at), b'0'..=b'9' | b':' | b'.') {
                    buffer.push(input[at])?;
                    at += 1;
                }
            } else if matches!(peek(at), b'-' | b'/' | b'.') {
                let delimiter = input[at];
                buffer.push(delimiter)?;
                at += 1;
                if peek(at).is_ascii_digit() {
                    // A `.` between two numbers is a fraction or `yyyy.ddd`;
                    // a second matching delimiter makes a three-part date.
                    let mut date = delimiter != b'.';
                    while peek(at).is_ascii_digit() {
                        buffer.push(input[at])?;
                        at += 1;
                    }
                    if peek(at) == delimiter {
                        date = true;
                        buffer.push(delimiter)?;
                        at += 1;
                        while peek(at).is_ascii_digit() || peek(at) == delimiter {
                            buffer.push(input[at])?;
                            at += 1;
                        }
                    }
                    kind = if date {
                        FieldKind::Date
                    } else {
                        FieldKind::Number
                    };
                } else {
                    kind = FieldKind::Date;
                    while peek(at).is_ascii_alphanumeric() || peek(at) == delimiter {
                        buffer.push(input[at])?;
                        at += 1;
                    }
                }
            } else {
                kind = FieldKind::Number;
            }
        } else if first == b'.' {
            buffer.push(b'.')?;
            at += 1;
            while peek(at).is_ascii_digit() {
                buffer.push(input[at])?;
                at += 1;
            }
            kind = FieldKind::Number;
        } else if first.is_ascii_alphabetic() {
            while peek(at).is_ascii_alphabetic() {
                buffer.push(input[at])?;
                at += 1;
            }
            // Letters followed by date punctuation, or by a digit or `+`
            // after a word that is no date keyword, start a date or a zone
            // name (`jan-5`, `america/new_york`, `pst8pdt`).
            let next = peek(at);
            let date = matches!(next, b'-' | b'/' | b'.')
                || ((next == b'+' || next.is_ascii_digit())
                    && keyword(&buffer.text[start..buffer.used]).is_none());
            if date {
                loop {
                    buffer.push(input[at])?;
                    at += 1;
                    let next = peek(at);
                    if !(matches!(next, b'+' | b'-' | b'/' | b'_' | b'.' | b':')
                        || next.is_ascii_alphanumeric())
                    {
                        break;
                    }
                }
                kind = FieldKind::Date;
            } else {
                kind = FieldKind::String;
            }
        } else if first == b'+' || first == b'-' {
            buffer.push(first)?;
            at += 1;
            while at < input.len() && is_space(input[at]) {
                at += 1;
            }
            if peek(at).is_ascii_digit() {
                kind = FieldKind::Zone;
                buffer.push(input[at])?;
                at += 1;
                while matches!(peek(at), b'0'..=b'9' | b':' | b'.' | b'-') {
                    buffer.push(input[at])?;
                    at += 1;
                }
            } else if peek(at).is_ascii_alphabetic() {
                kind = FieldKind::Special;
                while peek(at).is_ascii_alphabetic() {
                    buffer.push(input[at])?;
                    at += 1;
                }
            } else {
                return None;
            }
        } else if first.is_ascii_punctuation() {
            // Other punctuation only separates fields.
            at += 1;
            continue;
        } else {
            return None;
        }
        spans[count] = (start as u16, (buffer.used - start) as u16, kind);
        count += 1;
        // The field's terminator.
        buffer.used += 1;
    }
    Some(Fields {
        text: buffer.text,
        spans,
        count,
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Reserved {
    Late,
    Early,
    Epoch,
    Now,
    Today,
    Tomorrow,
    Yesterday,
    Zulu,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Meridiem {
    Am,
    Pm,
}

/// A unit word that labels the next field (`ptype` in PostgreSQL).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Prefix {
    /// `j`, `jd`, `julian`: a Julian day number follows.
    Julian,
    /// `t`: an ISO 8601 time follows.
    IsoTime,
    /// `y`, `m`, `d`, `h`, `mm`, `s` and the other units, which date input
    /// accepts as labels but no field can follow.
    Unit,
}

/// `datetktbl`: the date/time keywords.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Keyword {
    Reserved(Reserved),
    Month(i32),
    /// `bc` (true) or `ad`.
    Era(bool),
    Meridiem(Meridiem),
    Ignored,
    Prefix(Prefix),
    Weekday,
    /// `dst`: a daylight-time modifier on a preceding standard abbreviation.
    DaylightModifier,
}

const KEYWORDS: [(&str, Keyword); 72] = {
    use Keyword::*;
    [
        ("+infinity", Reserved(self::Reserved::Late)),
        ("-infinity", Reserved(self::Reserved::Early)),
        ("ad", Era(false)),
        ("allballs", Reserved(self::Reserved::Zulu)),
        ("am", Meridiem(self::Meridiem::Am)),
        ("apr", Month(4)),
        ("april", Month(4)),
        ("at", Ignored),
        ("aug", Month(8)),
        ("august", Month(8)),
        ("bc", Era(true)),
        ("d", Prefix(self::Prefix::Unit)),
        ("dec", Month(12)),
        ("december", Month(12)),
        ("dow", Prefix(self::Prefix::Unit)),
        ("doy", Prefix(self::Prefix::Unit)),
        ("dst", DaylightModifier),
        ("epoch", Reserved(self::Reserved::Epoch)),
        ("feb", Month(2)),
        ("february", Month(2)),
        ("fri", Weekday),
        ("friday", Weekday),
        ("h", Prefix(self::Prefix::Unit)),
        ("infinity", Reserved(self::Reserved::Late)),
        ("isodow", Prefix(self::Prefix::Unit)),
        ("isoyear", Prefix(self::Prefix::Unit)),
        ("j", Prefix(self::Prefix::Julian)),
        ("jan", Month(1)),
        ("january", Month(1)),
        ("jd", Prefix(self::Prefix::Julian)),
        ("jul", Month(7)),
        ("julian", Prefix(self::Prefix::Julian)),
        ("july", Month(7)),
        ("jun", Month(6)),
        ("june", Month(6)),
        ("m", Prefix(self::Prefix::Unit)),
        ("mar", Month(3)),
        ("march", Month(3)),
        ("may", Month(5)),
        ("mm", Prefix(self::Prefix::Unit)),
        ("mon", Weekday),
        ("monday", Weekday),
        ("nov", Month(11)),
        ("november", Month(11)),
        ("now", Reserved(self::Reserved::Now)),
        ("oct", Month(10)),
        ("october", Month(10)),
        ("on", Ignored),
        ("pm", Meridiem(self::Meridiem::Pm)),
        ("s", Prefix(self::Prefix::Unit)),
        ("sat", Weekday),
        ("saturday", Weekday),
        ("sep", Month(9)),
        ("sept", Month(9)),
        ("september", Month(9)),
        ("sun", Weekday),
        ("sunday", Weekday),
        ("t", Prefix(self::Prefix::IsoTime)),
        ("thu", Weekday),
        ("thur", Weekday),
        ("thurs", Weekday),
        ("thursday", Weekday),
        ("today", Reserved(self::Reserved::Today)),
        ("tomorrow", Reserved(self::Reserved::Tomorrow)),
        ("tue", Weekday),
        ("tues", Weekday),
        ("tuesday", Weekday),
        ("wed", Weekday),
        ("wednesday", Weekday),
        ("weds", Weekday),
        ("y", Prefix(self::Prefix::Unit)),
        ("yesterday", Reserved(self::Reserved::Yesterday)),
    ]
};

/// `DecodeSpecial`: the keyword a lowercased word names.
fn keyword(word: &[u8]) -> Option<Keyword> {
    KEYWORDS
        .iter()
        .find(|(name, _)| name.as_bytes() == word)
        .map(|&(_, keyword)| keyword)
}

/// What a word in the input means: a zone abbreviation (which takes
/// precedence, as `DecodeTimezoneAbbrev` runs first), a keyword, or neither.
#[allow(
    clippy::large_enum_variant,
    reason = "a dynamic zone is copied without heap allocation"
)]
enum Word {
    /// A fixed-offset abbreviation; `daylight` marks a DST one (`DTZ`).
    FixedZone {
        east: i32,
        daylight: bool,
    },
    /// An abbreviation whose offset depends on the date (`DYNTZ`).
    DynamicZone(Timezone),
    Keyword(Keyword),
    Unknown,
}

fn word(text: &[u8]) -> Word {
    let text = core::str::from_utf8(text).expect("datetime fields are ASCII");
    match super::timezone::lookup_abbreviation(text) {
        Some(zone @ Timezone::Posix(_)) => {
            let (east, _, daylight) = zone.resolve_info(0);
            Word::FixedZone { east, daylight }
        }
        Some(zone) => Word::DynamicZone(zone),
        None => keyword(text.as_bytes()).map_or(Word::Unknown, Word::Keyword),
    }
}

/// `pg_tzset`: a zone named in the input.
fn named_zone(text: &[u8]) -> Option<Timezone> {
    super::timezone::pg_tzset(core::str::from_utf8(text).expect("datetime fields are ASCII"))
}

/// The broken-down value (`struct pg_tm` and `fsec`).
#[derive(Debug, Clone, Copy, Default)]
struct Tm {
    year: i32,
    mon: i32,
    mday: i32,
    hour: i32,
    min: i32,
    sec: i32,
    yday: i32,
    /// Microseconds.
    fsec: i32,
}

/// `strtol` narrowed to `int` as PostgreSQL's `strtoint` does: leading
/// whitespace and a sign, then digits. Returns the value and the bytes
/// consumed (0 when no digits were read), or `None` when out of range.
fn strtoint(text: &[u8]) -> Option<(i32, usize)> {
    let (value, used) = strtoi64(text)?;
    Some((i32::try_from(value).ok()?, used))
}

fn strtoi64(text: &[u8]) -> Option<(i64, usize)> {
    let mut at = 0;
    while at < text.len() && is_space(text[at]) {
        at += 1;
    }
    let negative = match text.get(at) {
        Some(b'-') => {
            at += 1;
            true
        }
        Some(b'+') => {
            at += 1;
            false
        }
        _ => false,
    };
    let digits = at;
    let mut value: i64 = 0;
    let mut overflow = false;
    while at < text.len() && text[at].is_ascii_digit() {
        let digit = i64::from(text[at] - b'0');
        match value.checked_mul(10).and_then(|value| {
            if negative {
                value.checked_sub(digit)
            } else {
                value.checked_add(digit)
            }
        }) {
            Some(next) => value = next,
            None => overflow = true,
        }
        at += 1;
    }
    if at == digits {
        return Some((0, 0));
    }
    if overflow { None } else { Some((value, at)) }
}

/// `atoi` over digits: `strtol` saturates and the result narrows to `int`.
fn atoi(text: &[u8]) -> i32 {
    let mut value: i64 = 0;
    for &byte in text.iter().take_while(|byte| byte.is_ascii_digit()) {
        value = value
            .saturating_mul(10)
            .saturating_add(i64::from(byte - b'0'));
    }
    value as i32
}

/// `ParseFraction`: `.` and optional digits, nothing else.
fn parse_fraction(text: &[u8]) -> Decode<f64> {
    debug_assert_eq!(text.first(), Some(&b'.'));
    if text.len() == 1 {
        return Ok(0.0);
    }
    if !text[1..].iter().all(u8::is_ascii_digit) {
        return Err(DecodeError::BadFormat);
    }
    core::str::from_utf8(text)
        .expect("digits are ASCII")
        .parse()
        .map_err(|_| DecodeError::BadFormat)
}

/// `ParseFractionalSecond`: the fraction rounded to microseconds.
fn parse_fractional_second(text: &[u8]) -> Decode<i32> {
    Ok((parse_fraction(text)? * 1_000_000.0).round_ties_even() as i32)
}

fn is_leap(year: i64) -> bool {
    year % 4 == 0 && (year % 100 != 0 || year % 400 == 0)
}

fn days_in_month(year: i64, month: i32) -> i32 {
    const DAYS: [i32; 12] = [31, 28, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31];
    let days = DAYS[(month - 1) as usize];
    if month == 2 && is_leap(year) {
        days + 1
    } else {
        days
    }
}

/// `date2j` for a validated year, month and day.
fn date2j(year: i32, month: i32, day: i32) -> i64 {
    days_from_civil(i64::from(year), month as u32, day as u32) + UNIX_EPOCH_JDATE
}

/// `j2date`.
fn j2date(julian: i64, tm: &mut Tm) {
    let (year, month, day) = civil_from_days(julian - UNIX_EPOCH_JDATE);
    tm.year = year as i32;
    tm.mon = month as i32;
    tm.mday = day as i32;
}

/// `IS_VALID_JULIAN`: from November 4714 BC to May 5874898.
fn valid_julian(tm: &Tm) -> bool {
    (tm.year > -4713 || (tm.year == -4713 && tm.mon >= 11))
        && (tm.year < 5_874_898 || (tm.year == 5_874_898 && tm.mon < 6))
}

/// `time_overflows`: hour 24 and second 60 are accepted, up to 24:00:00.
fn time_overflows(tm: &Tm) -> bool {
    if !(0..=24).contains(&tm.hour)
        || !(0..60).contains(&tm.min)
        || !(0..=60).contains(&tm.sec)
        || !(0..=1_000_000).contains(&tm.fsec)
    {
        return true;
    }
    time_micros(tm) > USECS_PER_DAY
}

fn time_micros(tm: &Tm) -> i64 {
    ((i64::from(tm.hour) * 60 + i64::from(tm.min)) * 60 + i64::from(tm.sec)) * USECS_PER_SEC
        + i64::from(tm.fsec)
}

/// The transaction's timestamp in the session zone: its local fields and
/// the zone's offset (seconds east of UTC) then.
fn current_local() -> (Tm, i32) {
    let now = super::datetime::transaction_micros();
    let east = super::timezone::session().resolve(now).0;
    let local = now + i64::from(east) * USECS_PER_SEC;
    let mut tm = Tm::default();
    j2date(
        local.div_euclid(USECS_PER_DAY) + POSTGRES_EPOCH_JDATE,
        &mut tm,
    );
    let in_day = local.rem_euclid(USECS_PER_DAY);
    let seconds = in_day / USECS_PER_SEC;
    tm.hour = (seconds / 3600) as i32;
    tm.min = (seconds / 60 % 60) as i32;
    tm.sec = (seconds % 60) as i32;
    tm.fsec = (in_day % USECS_PER_SEC) as i32;
    (tm, east)
}

/// `DetermineTimeZoneOffset` (and, for an abbreviation zone,
/// `DetermineTimeZoneAbbrevOffset`): the offset east of UTC that `zone`
/// gives this local date and time; a date beyond the zone data reads as UTC.
fn zone_offset_at(zone: Timezone, tm: &Tm) -> i32 {
    if !valid_julian(tm) {
        return 0;
    }
    let seconds = (date2j(tm.year, tm.mon, tm.mday) - UNIX_EPOCH_JDATE) * 86_400
        + i64::from(tm.sec)
        + (i64::from(tm.min) + i64::from(tm.hour) * 60) * 60;
    let Some(local) = (seconds - PG_EPOCH_SECS).checked_mul(USECS_PER_SEC) else {
        return 0;
    };
    zone.resolve_local(local)
        .map_or(0, |utc| ((local - utc) / USECS_PER_SEC) as i32)
}

/// `DecodeTimezone`: `±hh[:mm[:ss]]` or `±hhmm`, as seconds east of UTC.
fn decode_timezone(text: &[u8]) -> Decode<i32> {
    let negative = match text.first() {
        Some(b'+') => false,
        Some(b'-') => true,
        _ => return Err(DecodeError::BadFormat),
    };
    let (mut hour, used) = strtoint(&text[1..]).ok_or(DecodeError::ZoneDisplacement)?;
    let mut rest = &text[1 + used..];
    let mut sec = 0;
    let min;
    if rest.first() == Some(&b':') {
        let (minutes, used) = strtoint(&rest[1..]).ok_or(DecodeError::ZoneDisplacement)?;
        min = minutes;
        rest = &rest[1 + used..];
        if rest.first() == Some(&b':') {
            let (seconds, used) = strtoint(&rest[1..]).ok_or(DecodeError::ZoneDisplacement)?;
            sec = seconds;
            rest = &rest[1 + used..];
        }
    } else if rest.is_empty() && text.len() > 3 {
        min = hour % 100;
        hour /= 100;
    } else {
        min = 0;
    }
    if !(0..=MAX_ZONE_HOUR).contains(&hour) || !(0..60).contains(&min) || !(0..60).contains(&sec) {
        return Err(DecodeError::ZoneDisplacement);
    }
    if !rest.is_empty() {
        return Err(DecodeError::BadFormat);
    }
    let east = (hour * 60 + min) * 60 + sec;
    Ok(if negative { -east } else { east })
}

/// `DecodeTime`: `h:m[:s[.f]]`, or `m:s.f` when a fraction follows the
/// second number. Returns the mask of the fields it set.
fn decode_time(text: &[u8], tm: &mut Tm) -> Decode<u32> {
    let (hour, used) = strtoi64(text).ok_or(DecodeError::FieldOverflow)?;
    let mut hour = hour;
    let mut rest = &text[used..];
    if rest.first() != Some(&b':') {
        return Err(DecodeError::BadFormat);
    }
    let (mut min, used) = strtoint(&rest[1..]).ok_or(DecodeError::FieldOverflow)?;
    rest = &rest[1 + used..];
    let mut sec = 0;
    let mut fsec = 0;
    match rest.first() {
        None => {}
        Some(b'.') => {
            fsec = parse_fractional_second(rest)?;
            if i32::try_from(hour).is_err() {
                return Err(DecodeError::FieldOverflow);
            }
            sec = min;
            min = hour as i32;
            hour = 0;
        }
        Some(b':') => {
            let (seconds, used) = strtoint(&rest[1..]).ok_or(DecodeError::FieldOverflow)?;
            sec = seconds;
            rest = &rest[1 + used..];
            match rest.first() {
                Some(b'.') => fsec = parse_fractional_second(rest)?,
                Some(_) => return Err(DecodeError::BadFormat),
                None => {}
            }
        }
        Some(_) => return Err(DecodeError::BadFormat),
    }
    if hour < 0
        || !(0..60).contains(&min)
        || !(0..=60).contains(&sec)
        || !(0..=1_000_000).contains(&fsec)
    {
        return Err(DecodeError::FieldOverflow);
    }
    tm.hour = i32::try_from(hour).map_err(|_| DecodeError::FieldOverflow)?;
    tm.min = min;
    tm.sec = sec;
    tm.fsec = fsec;
    Ok(TIME_M)
}

/// `DecodeNumberField`: run-together digits, `yyyymmdd` (or `yymmdd`) while
/// the date is incomplete, else `hhmmss` or `hhmm`, with an optional
/// fraction of a second. Returns the mask of the fields it set.
fn decode_number_field(
    text: &[u8],
    fmask: u32,
    tm: &mut Tm,
    is_two_digit_year: &mut bool,
) -> Decode<u32> {
    if !text
        .iter()
        .all(|&byte| byte.is_ascii_digit() || byte == b'.')
    {
        return Err(DecodeError::BadFormat);
    }
    let mut digits = text;
    if let Some(dot) = text.iter().position(|&byte| byte == b'.') {
        tm.fsec = parse_fractional_second(&text[dot..])?;
        digits = &text[..dot];
    } else if fmask & DATE_M != DATE_M && digits.len() >= 6 {
        let len = digits.len();
        tm.mday = atoi(&digits[len - 2..]);
        tm.mon = atoi(&digits[len - 4..len - 2]);
        tm.year = atoi(&digits[..len - 4]);
        if len - 4 == 2 {
            *is_two_digit_year = true;
        }
        return Ok(DATE_M);
    }
    if fmask & TIME_M != TIME_M {
        match digits.len() {
            6 => {
                tm.sec = atoi(&digits[4..]);
                tm.min = atoi(&digits[2..4]);
                tm.hour = atoi(&digits[..2]);
                return Ok(TIME_M);
            }
            4 => {
                tm.sec = 0;
                tm.min = atoi(&digits[2..]);
                tm.hour = atoi(&digits[..2]);
                return Ok(TIME_M);
            }
            _ => {}
        }
    }
    Err(DecodeError::BadFormat)
}

/// `DecodeNumber`: one number placed by what the input has given so far and
/// the DateStyle field order. Returns the mask of the fields it set.
fn decode_number(
    text: &[u8],
    have_text_month: bool,
    fmask: u32,
    tm: &mut Tm,
    is_two_digit_year: &mut bool,
    order: FieldOrder,
) -> Decode<u32> {
    let (value, used) = strtoint(text).ok_or(DecodeError::FieldOverflow)?;
    if used == 0 {
        return Err(DecodeError::BadFormat);
    }
    match text.get(used) {
        Some(b'.') => {
            // Three or more digits before the point: `2001.360`,
            // `20011225`, `040506.789`.
            if used > 2 {
                return decode_number_field(text, fmask | DATE_M, tm, is_two_digit_year);
            }
            tm.fsec = parse_fractional_second(&text[used..])?;
        }
        Some(_) => return Err(DecodeError::BadFormat),
        None => {}
    }
    let len = text.len();
    if len == 3 && fmask & DATE_M == mask(YEAR) && (1..=366).contains(&value) {
        tm.yday = value;
        return Ok(mask(DOY) | mask(MONTH) | mask(DAY));
    }
    let field = match fmask & DATE_M {
        0 => {
            // Nothing yet: a year of three or more digits, else the DateStyle
            // order's first field.
            if len >= 3 || order == FieldOrder::Ymd {
                tm.year = value;
                YEAR
            } else if order == FieldOrder::Dmy {
                tm.mday = value;
                DAY
            } else {
                tm.mon = value;
                MONTH
            }
        }
        m if m == mask(YEAR) => {
            tm.mon = value;
            MONTH
        }
        m if m == mask(MONTH) => {
            // After a month name: MON-DD-YYYY, DD-MON-YYYY and YYYY-MON-DD,
            // and a two-digit year first only in YMD order.
            if have_text_month && (len >= 3 || order == FieldOrder::Ymd) {
                tm.year = value;
                YEAR
            } else {
                tm.mday = value;
                DAY
            }
        }
        m if m == mask(YEAR) | mask(MONTH) => {
            if have_text_month && len >= 3 && *is_two_digit_year {
                // DD-MON-YYYY: the short first number was the day.
                tm.mday = tm.year;
                tm.year = value;
                *is_two_digit_year = false;
                return Ok(mask(DAY));
            }
            tm.mday = value;
            DAY
        }
        m if m == mask(DAY) => {
            tm.mon = value;
            MONTH
        }
        m if m == mask(MONTH) | mask(DAY) => {
            tm.year = value;
            YEAR
        }
        DATE_M => return decode_number_field(text, fmask, tm, is_two_digit_year),
        _ => return Err(DecodeError::BadFormat),
    };
    if field == YEAR {
        *is_two_digit_year = len <= 2;
    }
    Ok(mask(field))
}

/// `DecodeDate`: a field of numbers and a month name split by punctuation.
/// Returns the mask of the fields it set.
fn decode_date(
    text: &[u8],
    mut fmask: u32,
    is_two_digit_year: &mut bool,
    tm: &mut Tm,
    order: FieldOrder,
) -> Decode<u32> {
    let mut parts: [&[u8]; MAX_DATE_FIELDS] = [&[]; MAX_DATE_FIELDS];
    let mut count = 0;
    let mut at = 0;
    while at < text.len() && count < MAX_DATE_FIELDS {
        while at < text.len() && !text[at].is_ascii_alphanumeric() {
            at += 1;
        }
        if at == text.len() {
            return Err(DecodeError::BadFormat);
        }
        let start = at;
        if text[at].is_ascii_digit() {
            while at < text.len() && text[at].is_ascii_digit() {
                at += 1;
            }
        } else {
            while at < text.len() && text[at].is_ascii_alphabetic() {
                at += 1;
            }
        }
        parts[count] = &text[start..at];
        count += 1;
        // The byte after a part is its terminator, whatever it was.
        if at < text.len() {
            at += 1;
        }
    }
    let mut tmask = 0;
    let mut have_text_month = false;
    let mut numeric = [true; MAX_DATE_FIELDS];
    for (index, part) in parts[..count].iter().enumerate() {
        if !part[0].is_ascii_alphabetic() {
            continue;
        }
        numeric[index] = false;
        let field = match keyword(part) {
            Some(Keyword::Ignored) => continue,
            Some(Keyword::Month(month)) => {
                tm.mon = month;
                have_text_month = true;
                mask(MONTH)
            }
            _ => return Err(DecodeError::BadFormat),
        };
        if fmask & field != 0 {
            return Err(DecodeError::BadFormat);
        }
        fmask |= field;
        tmask |= field;
    }
    for (index, part) in parts[..count].iter().enumerate() {
        if !numeric[index] {
            continue;
        }
        let field = decode_number(part, have_text_month, fmask, tm, is_two_digit_year, order)?;
        if fmask & field != 0 {
            return Err(DecodeError::BadFormat);
        }
        fmask |= field;
        tmask |= field;
    }
    if fmask & !(mask(DOY) | mask(TZ)) != DATE_M {
        return Err(DecodeError::BadFormat);
    }
    Ok(tmask)
}

/// `ValidateDate`: applies BC, two-digit years and day-of-year, then checks
/// the month and day.
fn validate_date(
    fmask: u32,
    is_julian: bool,
    is_two_digit_year: bool,
    bc: bool,
    tm: &mut Tm,
) -> Decode<()> {
    if fmask & mask(YEAR) != 0 {
        if is_julian {
        } else if bc {
            if tm.year <= 0 {
                return Err(DecodeError::FieldOverflow);
            }
            tm.year = -(tm.year - 1);
        } else if is_two_digit_year {
            if tm.year < 0 {
                return Err(DecodeError::FieldOverflow);
            }
            if tm.year < 70 {
                tm.year += 2000;
            } else if tm.year < 100 {
                tm.year += 1900;
            }
        } else if tm.year <= 0 {
            return Err(DecodeError::FieldOverflow);
        }
    }
    if fmask & mask(DOY) != 0 {
        j2date(date2j(tm.year, 1, 1) + i64::from(tm.yday) - 1, tm);
    }
    if fmask & mask(MONTH) != 0 && !(1..=12).contains(&tm.mon) {
        return Err(DecodeError::MonthDayOverflow);
    }
    if fmask & mask(DAY) != 0 && !(1..=31).contains(&tm.mday) {
        return Err(DecodeError::MonthDayOverflow);
    }
    if fmask & DATE_M == DATE_M && tm.mday > days_in_month(i64::from(tm.year), tm.mon) {
        return Err(DecodeError::FieldOverflow);
    }
    Ok(())
}

/// Applies AM/PM to a validated hour.
fn apply_meridiem(meridiem: Option<Meridiem>, tm: &mut Tm) -> Decode<()> {
    if meridiem.is_some() && tm.hour > 12 {
        return Err(DecodeError::FieldOverflow);
    }
    match meridiem {
        Some(Meridiem::Am) if tm.hour == 12 => tm.hour = 0,
        Some(Meridiem::Pm) if tm.hour != 12 => tm.hour += 12,
        _ => {}
    }
    Ok(())
}

/// A Julian day number `value` with an optional fraction of a day `rest`.
fn julian_day(value: i32, rest: &[u8], tm: &mut Tm) -> Decode<u32> {
    if value < 0 {
        return Err(DecodeError::FieldOverflow);
    }
    j2date(i64::from(value), tm);
    if rest.is_empty() {
        return Ok(DATE_M);
    }
    // `dt2time` of the fraction's microseconds, truncated.
    let time = (parse_fraction(rest)? * USECS_PER_DAY as f64) as i64;
    tm.hour = (time / 3_600_000_000) as i32;
    tm.min = (time / 60_000_000 % 60) as i32;
    tm.sec = (time / USECS_PER_SEC % 60) as i32;
    tm.fsec = (time % USECS_PER_SEC) as i32;
    Ok(DATE_M | TIME_M)
}

/// The zone whose offset at the decoded local time applies: a named zone or
/// a dynamic abbreviation, or the session zone when the input names none.
/// The `dst` modifier needs a fixed standard abbreviation instead.
fn zone_to_resolve(written: Option<Timezone>, fmask: u32) -> Decode<Option<Timezone>> {
    let zone = written.or_else(|| (fmask & mask(TZ) == 0).then(super::timezone::session));
    if zone.is_some() && fmask & mask(DTZMOD) != 0 {
        return Err(DecodeError::BadFormat);
    }
    Ok(zone)
}

/// What `DecodeDateTime` decoded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DateKind {
    Date,
    Epoch,
    Late,
    Early,
}

struct DecodedDateTime {
    kind: DateKind,
    tm: Tm,
    /// The value's offset east of UTC: the zone it named, or the session
    /// zone at its local time.
    east: i32,
}

/// `DecodeDateTime`: date, timestamp and timestamptz input.
fn decode_date_time(fields: &Fields, order: FieldOrder) -> Decode<DecodedDateTime> {
    let mut fmask = 0u32;
    let mut prefix: Option<Prefix> = None;
    let mut kind = DateKind::Date;
    let mut tm = Tm::default();
    let mut meridiem = None;
    let mut have_text_month = false;
    let mut is_julian = false;
    let mut is_two_digit_year = false;
    let mut bc = false;
    let mut named: Option<Timezone> = None;
    let mut abbreviation: Option<Timezone> = None;
    let mut east = 0i32;
    for index in 0..fields.count {
        let (field, field_kind) = fields.field(index);
        let tmask = match field_kind {
            FieldKind::Date => {
                if prefix == Some(Prefix::Julian) {
                    // A Julian day with an attached zone (`J2451187-05`).
                    let (day, used) = strtoint(field).ok_or(DecodeError::FieldOverflow)?;
                    if day < 0 {
                        return Err(DecodeError::FieldOverflow);
                    }
                    j2date(i64::from(day), &mut tm);
                    is_julian = true;
                    east = decode_timezone(&field[used..])?;
                    prefix = None;
                    DATE_M | TIME_M | mask(TZ)
                } else if prefix.is_some()
                    || fmask & (mask(MONTH) | mask(DAY)) == mask(MONTH) | mask(DAY)
                {
                    // With the month and day known this is a zone name, or a
                    // run-together time with a zone (`hhmmss-zz`).
                    if field[0].is_ascii_digit() || prefix.is_some() {
                        if prefix.is_some() {
                            if prefix != Some(Prefix::IsoTime) {
                                return Err(DecodeError::BadFormat);
                            }
                            prefix = None;
                        }
                        if fmask & TIME_M == TIME_M {
                            return Err(DecodeError::BadFormat);
                        }
                        let dash = field
                            .iter()
                            .position(|&byte| byte == b'-')
                            .ok_or(DecodeError::BadFormat)?;
                        east = decode_timezone(&field[dash..])?;
                        decode_number_field(&field[..dash], fmask, &mut tm, &mut is_two_digit_year)?
                            | mask(TZ)
                    } else {
                        named = Some(named_zone(field).ok_or_else(|| fields.unknown_zone(index))?);
                        mask(TZ)
                    }
                } else {
                    decode_date(field, fmask, &mut is_two_digit_year, &mut tm, order)?
                }
            }
            FieldKind::Time => {
                if prefix.is_some() {
                    if prefix != Some(Prefix::IsoTime) {
                        return Err(DecodeError::BadFormat);
                    }
                    prefix = None;
                }
                let tmask = decode_time(field, &mut tm)?;
                if time_overflows(&tm) {
                    return Err(DecodeError::FieldOverflow);
                }
                tmask
            }
            FieldKind::Zone => {
                east = decode_timezone(field)?;
                mask(TZ)
            }
            FieldKind::Number => {
                if let Some(label) = prefix {
                    let (value, used) = strtoint(field).ok_or(DecodeError::FieldOverflow)?;
                    if field.get(used).is_some_and(|&byte| byte != b'.') {
                        return Err(DecodeError::BadFormat);
                    }
                    let tmask = match label {
                        Prefix::Julian => {
                            is_julian = true;
                            julian_day(value, &field[used..], &mut tm)?
                        }
                        Prefix::IsoTime => {
                            let tmask = decode_number_field(
                                field,
                                fmask | DATE_M,
                                &mut tm,
                                &mut is_two_digit_year,
                            )?;
                            if tmask != TIME_M {
                                return Err(DecodeError::BadFormat);
                            }
                            tmask
                        }
                        Prefix::Unit => return Err(DecodeError::BadFormat),
                    };
                    prefix = None;
                    tmask
                } else {
                    let dot = field.iter().position(|&byte| byte == b'.');
                    if dot.is_some() && fmask & DATE_M == 0 {
                        decode_date(field, fmask, &mut is_two_digit_year, &mut tm, order)?
                    } else if dot.is_some_and(|dot| dot > 2) {
                        decode_number_field(field, fmask, &mut tm, &mut is_two_digit_year)?
                    } else if field.len() >= 6 && (fmask & DATE_M == 0 || fmask & TIME_M == 0) {
                        // Six or more digits are `yymmdd`/`hhmmss` unless both
                        // a date part and a time part were given.
                        decode_number_field(field, fmask, &mut tm, &mut is_two_digit_year)?
                    } else {
                        decode_number(
                            field,
                            have_text_month,
                            fmask,
                            &mut tm,
                            &mut is_two_digit_year,
                            order,
                        )?
                    }
                }
            }
            FieldKind::String | FieldKind::Special => match word(field) {
                Word::Keyword(Keyword::Ignored) => continue,
                Word::Keyword(Keyword::Reserved(reserved)) => match reserved {
                    Reserved::Now => {
                        kind = DateKind::Date;
                        (tm, east) = current_local();
                        DATE_M | TIME_M | mask(TZ)
                    }
                    Reserved::Today | Reserved::Tomorrow | Reserved::Yesterday => {
                        kind = DateKind::Date;
                        let (today, _) = current_local();
                        let shift = match reserved {
                            Reserved::Tomorrow => 1,
                            Reserved::Yesterday => -1,
                            _ => 0,
                        };
                        j2date(date2j(today.year, today.mon, today.mday) + shift, &mut tm);
                        DATE_M
                    }
                    Reserved::Zulu => {
                        kind = DateKind::Date;
                        tm.hour = 0;
                        tm.min = 0;
                        tm.sec = 0;
                        east = 0;
                        TIME_M | mask(TZ)
                    }
                    Reserved::Epoch | Reserved::Late | Reserved::Early => {
                        kind = match reserved {
                            Reserved::Epoch => DateKind::Epoch,
                            Reserved::Late => DateKind::Late,
                            _ => DateKind::Early,
                        };
                        DATE_M | TIME_M | mask(TZ)
                    }
                },
                Word::Keyword(Keyword::Month(month)) => {
                    // A numeric month already read with no day: it was the
                    // day (`5 jan`).
                    let mut tmask = mask(MONTH);
                    if fmask & mask(MONTH) != 0
                        && !have_text_month
                        && fmask & mask(DAY) == 0
                        && (1..=31).contains(&tm.mon)
                    {
                        tm.mday = tm.mon;
                        tmask = mask(DAY);
                    }
                    have_text_month = true;
                    tm.mon = month;
                    tmask
                }
                Word::Keyword(Keyword::DaylightModifier) => {
                    east += 3600;
                    mask(DTZMOD) | mask(DTZ)
                }
                Word::FixedZone {
                    east: offset,
                    daylight,
                } => {
                    east = offset;
                    if daylight {
                        mask(DTZ) | mask(TZ)
                    } else {
                        mask(TZ)
                    }
                }
                Word::DynamicZone(zone) => {
                    abbreviation = Some(zone);
                    mask(DYNTZ) | mask(TZ)
                }
                Word::Keyword(Keyword::Meridiem(value)) => {
                    meridiem = Some(value);
                    mask(AMPM)
                }
                Word::Keyword(Keyword::Era(value)) => {
                    bc = value;
                    mask(ADBC)
                }
                Word::Keyword(Keyword::Weekday) => mask(DOW),
                Word::Keyword(Keyword::Prefix(label)) => {
                    if label == Prefix::IsoTime && fmask & DATE_M != DATE_M {
                        return Err(DecodeError::BadFormat);
                    }
                    if prefix.is_some() {
                        return Err(DecodeError::BadFormat);
                    }
                    prefix = Some(label);
                    0
                }
                Word::Unknown => {
                    named = Some(named_zone(field).ok_or(DecodeError::BadFormat)?);
                    mask(TZ)
                }
            },
        };
        if tmask & fmask != 0 {
            return Err(DecodeError::BadFormat);
        }
        fmask |= tmask;
    }
    if prefix.is_some() {
        return Err(DecodeError::BadFormat);
    }
    if kind == DateKind::Date {
        validate_date(fmask, is_julian, is_two_digit_year, bc, &mut tm)?;
        apply_meridiem(meridiem, &mut tm)?;
        if fmask & DATE_M != DATE_M {
            return Err(DecodeError::BadFormat);
        }
        if let Some(zone) = zone_to_resolve(named.or(abbreviation), fmask)? {
            east = zone_offset_at(zone, &tm);
        }
    }
    Ok(DecodedDateTime { kind, tm, east })
}

/// `DecodeTimeOnly`: time and timetz input. Returns the time and its offset
/// east of UTC, and whether the input named a zone.
fn decode_time_only(fields: &Fields, order: FieldOrder) -> Decode<(Tm, i32, bool)> {
    let count = fields.count;
    let mut fmask = 0u32;
    let mut prefix: Option<Prefix> = None;
    let mut tm = Tm::default();
    let mut meridiem = None;
    let mut is_julian = false;
    let mut is_two_digit_year = false;
    let mut bc = false;
    let mut named: Option<Timezone> = None;
    let mut abbreviation: Option<Timezone> = None;
    let mut east = 0i32;
    for index in 0..count {
        let (field, field_kind) = fields.field(index);
        let tmask = match field_kind {
            FieldKind::Date => {
                // A leading date is accepted when a time or another date-like
                // field (a zone name) follows.
                if index == 0
                    && count >= 2
                    && (fields.kind(count - 1) == FieldKind::Date
                        || fields.kind(1) == FieldKind::Time)
                {
                    decode_date(field, fmask, &mut is_two_digit_year, &mut tm, order)?
                } else if field[0].is_ascii_digit() {
                    if fmask & TIME_M == TIME_M {
                        return Err(DecodeError::BadFormat);
                    }
                    let dash = field
                        .iter()
                        .position(|&byte| byte == b'-')
                        .ok_or(DecodeError::BadFormat)?;
                    east = decode_timezone(&field[dash..])?;
                    decode_number_field(
                        &field[..dash],
                        fmask | DATE_M,
                        &mut tm,
                        &mut is_two_digit_year,
                    )? | mask(TZ)
                } else {
                    named = Some(named_zone(field).ok_or_else(|| fields.unknown_zone(index))?);
                    mask(TZ)
                }
            }
            FieldKind::Time => {
                if prefix.is_some() {
                    if prefix != Some(Prefix::IsoTime) {
                        return Err(DecodeError::BadFormat);
                    }
                    prefix = None;
                }
                decode_time(field, &mut tm)?
            }
            FieldKind::Zone => {
                east = decode_timezone(field)?;
                mask(TZ)
            }
            FieldKind::Number => {
                if let Some(label) = prefix {
                    let (value, used) = strtoint(field).ok_or(DecodeError::FieldOverflow)?;
                    if field.get(used).is_some_and(|&byte| byte != b'.') {
                        return Err(DecodeError::BadFormat);
                    }
                    let tmask = match label {
                        Prefix::Julian => {
                            is_julian = true;
                            julian_day(value, &field[used..], &mut tm)?
                        }
                        Prefix::IsoTime => {
                            let tmask = decode_number_field(
                                field,
                                fmask | DATE_M,
                                &mut tm,
                                &mut is_two_digit_year,
                            )?;
                            if tmask != TIME_M {
                                return Err(DecodeError::BadFormat);
                            }
                            tmask
                        }
                        Prefix::Unit => return Err(DecodeError::BadFormat),
                    };
                    prefix = None;
                    tmask
                } else if let Some(dot) = field.iter().position(|&byte| byte == b'.') {
                    if index == 0 && count >= 2 && fields.kind(count - 1) == FieldKind::Date {
                        decode_date(field, fmask, &mut is_two_digit_year, &mut tm, order)?
                    } else if dot > 2 {
                        decode_number_field(field, fmask | DATE_M, &mut tm, &mut is_two_digit_year)?
                    } else {
                        return Err(DecodeError::BadFormat);
                    }
                } else if field.len() > 4 {
                    decode_number_field(field, fmask | DATE_M, &mut tm, &mut is_two_digit_year)?
                } else {
                    decode_number(
                        field,
                        false,
                        fmask | DATE_M,
                        &mut tm,
                        &mut is_two_digit_year,
                        order,
                    )?
                }
            }
            FieldKind::String | FieldKind::Special => match word(field) {
                Word::Keyword(Keyword::Ignored) => continue,
                Word::Keyword(Keyword::Reserved(Reserved::Now)) => {
                    let (now, _) = current_local();
                    tm.hour = now.hour;
                    tm.min = now.min;
                    tm.sec = now.sec;
                    tm.fsec = now.fsec;
                    TIME_M
                }
                Word::Keyword(Keyword::Reserved(Reserved::Zulu)) => {
                    tm.hour = 0;
                    tm.min = 0;
                    tm.sec = 0;
                    TIME_M | mask(TZ)
                }
                Word::Keyword(Keyword::DaylightModifier) => {
                    east += 3600;
                    mask(DTZMOD) | mask(DTZ)
                }
                Word::FixedZone {
                    east: offset,
                    daylight,
                } => {
                    east = offset;
                    if daylight {
                        mask(DTZ) | mask(TZ)
                    } else {
                        mask(TZ)
                    }
                }
                Word::DynamicZone(zone) => {
                    abbreviation = Some(zone);
                    mask(DYNTZ) | mask(TZ)
                }
                Word::Keyword(Keyword::Meridiem(value)) => {
                    meridiem = Some(value);
                    mask(AMPM)
                }
                Word::Keyword(Keyword::Era(value)) => {
                    bc = value;
                    mask(ADBC)
                }
                Word::Keyword(Keyword::Prefix(label)) => {
                    if prefix.is_some() {
                        return Err(DecodeError::BadFormat);
                    }
                    prefix = Some(label);
                    0
                }
                Word::Unknown => {
                    named = Some(named_zone(field).ok_or(DecodeError::BadFormat)?);
                    mask(TZ)
                }
                Word::Keyword(Keyword::Reserved(_) | Keyword::Month(_) | Keyword::Weekday) => {
                    return Err(DecodeError::BadFormat);
                }
            },
        };
        if tmask & fmask != 0 {
            return Err(DecodeError::BadFormat);
        }
        fmask |= tmask;
    }
    if prefix.is_some() {
        return Err(DecodeError::BadFormat);
    }
    validate_date(fmask, is_julian, is_two_digit_year, bc, &mut tm)?;
    apply_meridiem(meridiem, &mut tm)?;
    if time_overflows(&tm) {
        return Err(DecodeError::FieldOverflow);
    }
    if fmask & TIME_M != TIME_M {
        return Err(DecodeError::BadFormat);
    }
    if let Some(zone) = zone_to_resolve(named.or(abbreviation), fmask)? {
        // A zone name with a single offset needs no date; otherwise the
        // offset is the one at the given date, or today's.
        let constant = named.and_then(|zone| zone.constant_offset());
        east = match constant {
            Some(offset) => offset,
            None => {
                let mut at = if fmask & DATE_M == 0 && named.is_none() {
                    current_local().0
                } else if fmask & DATE_M != DATE_M {
                    return Err(DecodeError::BadFormat);
                } else {
                    tm
                };
                at.hour = tm.hour;
                at.min = tm.min;
                at.sec = tm.sec;
                zone_offset_at(zone, &at)
            }
        };
    }
    Ok((tm, east, fmask & mask(TZ) != 0))
}

/// `date_in`: days since 2000-01-01.
pub fn parse_date(input: &str) -> Result<i32, SqlError> {
    let decoded = decode(input, DATE_FIELD_BYTES, "date", decode_date_time)?;
    let mut tm = decoded.tm;
    match decoded.kind {
        DateKind::Date => {}
        DateKind::Epoch => {
            tm.year = 1970;
            tm.mon = 1;
            tm.mday = 1;
        }
        DateKind::Late => return Ok(DATE_INFINITY),
        DateKind::Early => return Ok(DATE_NEG_INFINITY),
    }
    let out_of_range = || {
        sql_err!(
            sqlstate::DATETIME_FIELD_OVERFLOW,
            "date out of range: \"{}\"",
            input
        )
    };
    if !valid_julian(&tm) {
        return Err(out_of_range());
    }
    let julian = date2j(tm.year, tm.mon, tm.mday);
    if !(0..DATE_END_JULIAN).contains(&julian) {
        return Err(out_of_range());
    }
    Ok((julian - POSTGRES_EPOCH_JDATE) as i32)
}

/// `timestamp_in` and, with `apply_timezone`, `timestamptz_in`:
/// microseconds since 2000-01-01, UTC for timestamptz.
pub fn parse_timestamp(input: &str, apply_timezone: bool) -> Result<i64, SqlError> {
    let type_name = if apply_timezone {
        "timestamp with time zone"
    } else {
        "timestamp"
    };
    let decoded = decode(input, TIMESTAMP_FIELD_BYTES, type_name, decode_date_time)?;
    match decoded.kind {
        DateKind::Date => {}
        DateKind::Epoch => return Ok(-PG_EPOCH_SECS * USECS_PER_SEC),
        DateKind::Late => return Ok(TIMESTAMP_INFINITY),
        DateKind::Early => return Ok(TIMESTAMP_NEG_INFINITY),
    }
    let tm = decoded.tm;
    let out_of_range = || {
        sql_err!(
            sqlstate::DATETIME_FIELD_OVERFLOW,
            "timestamp out of range: \"{}\"",
            input
        )
    };
    if !valid_julian(&tm) {
        return Err(out_of_range());
    }
    let date = date2j(tm.year, tm.mon, tm.mday) - POSTGRES_EPOCH_JDATE;
    let mut value = date
        .checked_mul(USECS_PER_DAY)
        .and_then(|value| value.checked_add(time_micros(&tm)))
        .ok_or_else(out_of_range)?;
    if apply_timezone {
        value = value
            .checked_sub(i64::from(decoded.east) * USECS_PER_SEC)
            .ok_or_else(out_of_range)?;
    }
    if !(MIN_TIMESTAMP..END_TIMESTAMP).contains(&value) {
        return Err(out_of_range());
    }
    Ok(value)
}

/// A decoded `time` or `timetz` input.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TimeInput {
    /// Microseconds since midnight.
    pub micros: i64,
    /// Seconds east of UTC: the zone the input named, or the session zone's
    /// offset on the date given or today.
    pub east: i32,
    /// Whether the input named a zone.
    pub zone_written: bool,
}

fn parse_time_input(input: &str, type_name: &str) -> Result<TimeInput, SqlError> {
    let (tm, east, zone_written) = decode(input, DATE_FIELD_BYTES, type_name, decode_time_only)?;
    Ok(TimeInput {
        micros: time_micros(&tm),
        east,
        zone_written,
    })
}

/// `timetz_in`.
pub fn parse_timetz(input: &str) -> Result<TimeInput, SqlError> {
    parse_time_input(input, "time with time zone")
}

/// `time_in`: microseconds since midnight. A zone is decoded and checked,
/// then discarded, as PostgreSQL does.
pub fn parse_time(input: &str) -> Result<i64, SqlError> {
    Ok(parse_time_input(input, "time")?.micros)
}

#[cfg(all(test, not(feature = "sql-fixture-suite")))]
mod tests {
    use super::*;

    fn in_order<T>(order: FieldOrder, read: impl FnOnce() -> T) -> T {
        set_session_field_order(order);
        let value = read();
        set_session_field_order(FieldOrder::Mdy);
        value
    }

    #[test]
    fn numbers_follow_the_field_order_without_allocating() {
        super::super::timezone::set_session(Timezone::utc());
        let date = |text: &str| parse_date(text).unwrap();
        let ymd = |year, month, day| {
            (days_from_civil(year, month, day) - super::super::datetime::PG_EPOCH_DAYS) as i32
        };
        crate::mem::guard::forbid_alloc(|| {
            assert_eq!(date("05/01/2024"), ymd(2024, 5, 1));
            assert_eq!(
                in_order(FieldOrder::Dmy, || date("05/01/2024")),
                ymd(2024, 1, 5)
            );
            assert_eq!(
                in_order(FieldOrder::Ymd, || date("24/05/01")),
                ymd(2024, 5, 1)
            );
            assert_eq!(date("Jan 5 2024"), ymd(2024, 1, 5));
            assert_eq!(date("5-jan-24"), ymd(2024, 1, 5));
            assert_eq!(date("J2451187"), ymd(1999, 1, 8));
            assert_eq!(date("20240105"), ymd(2024, 1, 5));
            assert_eq!(date("2024.005"), ymd(2024, 1, 5));
            assert_eq!(date("0044-03-15 BC"), ymd(-43, 3, 15));
            assert_eq!(
                parse_timestamp("20011225T040506.789-07", true).unwrap(),
                parse_timestamp("2001-12-25 11:05:06.789", false).unwrap()
            );
            assert_eq!(
                parse_timetz("1:30 pm -0530").unwrap(),
                TimeInput {
                    micros: 48_600_000_000,
                    east: -19_800,
                    zone_written: true,
                }
            );
        });
    }

    #[test]
    fn malformed_and_out_of_range_input_keep_postgresql_errors() {
        super::super::timezone::set_session(Timezone::utc());
        let state = |result: Result<i32, SqlError>| result.unwrap_err().sqlstate;
        let _ = crate::sql::eval::take_diagnostic();
        assert_eq!(state(parse_date("13/01/2024")).as_str(), "22008");
        assert!(crate::sql::eval::take_diagnostic().is_some_and(|d| d.hint.is_some()));
        assert_eq!(state(parse_date("02/30/2024")).as_str(), "22008");
        assert!(crate::sql::eval::take_diagnostic().is_none());
        assert_eq!(state(parse_date("garbage")).as_str(), "22007");
        assert_eq!(state(parse_date("2024-01-05 é")).as_str(), "22007");
        assert_eq!(state(parse_date("5874898-01-01")).as_str(), "22008");
        let zone = parse_timestamp("2024-01-05 12:00 +16", true).unwrap_err();
        assert_eq!(zone.sqlstate.as_str(), "22009");
        let unknown = parse_timestamp("2024-01-05 12:00 Mars/Olympus", true).unwrap_err();
        assert_eq!(
            unknown.message.as_str(),
            "time zone \"mars/olympus\" not recognized"
        );
    }
}
