//! Time zones with DST. A [`Timezone`] resolves the UTC offset and abbreviation for
//! a specific instant, so `timestamptz` output is correct across daylight-time
//! transitions. Fixed-offset zones (UTC, `+05:30`) are the degenerate case
//! with no transitions.
//!
//! Installed TZif files supply historical transitions; their POSIX footer rules
//! cover later instants. Datetime input recognizes session-zone abbreviations
//! before PostgreSQL's pinned Default abbreviation set, independently of whether
//! the host happens to install legacy files named EST, MST or HST.

use core::fmt::Write;

use super::datetime::{PG_EPOCH_DAYS, civil_from_days, day_of_week, days_from_civil};
use crate::util::StackStr;

const DAY_US: i64 = 86_400_000_000;

/// `TZ_STRLEN_MAX`: the longest zone name `pg_tzset` accepts. It also bounds
/// a POSIX zone's abbreviations, which are parts of its name.
pub const TZ_STRLEN_MAX: usize = 255;

/// A zone abbreviation, as long as a POSIX zone name can make it.
pub type Abbreviation = StackStr<TZ_STRLEN_MAX>;

/// The day of a POSIX daylight-time rule (`getrule`).
#[derive(Debug, Clone, Copy)]
enum RuleDay {
    /// `Jn`: day 1 to 365, never counting February 29.
    Julian(i64),
    /// `n`: zero-based day of the year, 0 to 365.
    DayOfYear(i64),
    /// `Mm.w.d`: the `week`-th `weekday` (0 = Sunday) of `month`; week 5
    /// is the last.
    MonthWeek { month: u32, week: u32, weekday: i64 },
}

/// A transition rule: the day, and the local time of day in seconds, which
/// may lie outside 0 to 24 hours (`M10.5.0/-1`, `M3.2.0/167`).
#[derive(Debug, Clone, Copy)]
pub struct Rule {
    day: RuleDay,
    seconds: i64,
}

#[derive(Debug, Clone, Copy)]
pub struct Dst {
    /// Transition into daylight time, in standard local time.
    start: Rule,
    /// Transition back to standard time, in daylight local time.
    end: Rule,
}

#[derive(Debug, Clone, Copy)]
enum Daylight {
    /// Standard time only.
    Never,
    Rules(Dst),
    /// Rules that never leave daylight time within a year, which
    /// PostgreSQL's `tzparse` turns into daylight time always.
    Always,
}

/// A rule-based zone: fixed offsets with an optional POSIX DST rule. Fixed
/// zones, POSIX zone names and TZif footer rules all take this form. The
/// zone keeps its name once; its abbreviations are ranges of it.
#[derive(Debug, Clone, Copy)]
pub struct PosixZone {
    std_off: i32,
    dst_off: i32,
    name: Abbreviation,
    std_abbrev: (u8, u8),
    dst_abbrev: (u8, u8),
    std_is_dst: bool,
    daylight: Daylight,
}

/// A resolved time zone: a rule-based zone, or a slot in the TZif cache — the
/// full IANA database with its historical transitions.
#[derive(Debug, Clone, Copy)]
#[allow(
    clippy::large_enum_variant,
    reason = "zones are copied without heap allocation; a POSIX zone's abbreviations are as long as its name"
)]
pub enum Timezone {
    Posix(PosixZone),
    Tzif(super::tzif::TzifSlot),
    Abbreviation {
        zone: super::tzif::TzifSlot,
        name: Abbreviation,
    },
}

#[derive(Clone, Copy)]
pub(crate) enum AbbreviationDefinition {
    Fixed { offset: i32, is_dst: bool },
    Dynamic(&'static str),
}

use core::cell::Cell;
std::thread_local! {
    /// The session's `TimeZone`, published once per statement so evaluation can
    /// resolve a value that carries no zone of its own — `'12:00'::timetz` and
    /// `timestamptz::timetz` both take it — without threading the GUC through
    /// every call. Single-threaded per connection, like the statement deadline.
    static SESSION: Cell<Option<Timezone>> = const { Cell::new(None) };
}

/// Publishes the session zone for the statement about to run.
pub fn set_session(zone: Timezone) {
    SESSION.with(|z| z.set(Some(zone)));
}

/// The session zone, or UTC before any statement has published one.
pub fn session() -> Timezone {
    SESSION.with(|z| z.get()).unwrap_or_else(Timezone::utc)
}

impl Timezone {
    /// A fixed-offset zone (no DST): UTC, `Etc/GMT±N`, bare numeric offsets.
    pub fn fixed(off_secs: i32, abbrev: &str, is_dst: bool) -> Timezone {
        let name = Abbreviation::from_str(abbrev);
        let whole = (0, name.len() as u8);
        Timezone::Posix(PosixZone {
            std_off: off_secs,
            dst_off: off_secs,
            name,
            std_abbrev: whole,
            dst_abbrev: whole,
            std_is_dst: is_dst,
            daylight: Daylight::Never,
        })
    }

    pub fn utc() -> Timezone {
        Timezone::fixed(0, "UTC", false)
    }

    /// The offset (seconds east of UTC) and abbreviation in effect at `utc`
    /// microseconds-since-2000.
    pub fn resolve(&self, utc: i64) -> (i32, Abbreviation) {
        let (offset, abbreviation, _) = self.resolve_info(utc);
        (offset, abbreviation)
    }

    pub(crate) fn resolve_info(&self, utc: i64) -> (i32, Abbreviation, bool) {
        match self {
            Timezone::Posix(z) => z.resolve_info(utc),
            Timezone::Tzif(slot) => super::tzif::resolve_info(*slot, utc),
            Timezone::Abbreviation { zone, name } => {
                // An obsolete Default alias absent from its zone uses the zone
                // itself, as PostgreSQL's dynamic-abbreviation contract specifies.
                super::tzif::resolve_abbreviation(*zone, name.as_str(), utc)
                    .unwrap_or_else(|| super::tzif::resolve_info(*zone, utc))
            }
        }
    }

    /// Converts a local wall-clock timestamp to UTC. Around a backward clock
    /// change PostgreSQL chooses the later (standard-time) interpretation; in
    /// a forward gap it keeps the pre-transition offset. Both choices are the
    /// latest UTC candidate among the offsets surrounding the local time.
    pub fn resolve_local(&self, local: i64) -> Option<i64> {
        if let Self::Abbreviation { zone, .. } = self {
            // PostgreSQL probes an abbreviation using the ordinary zone's UTC
            // interpretation, then forces the abbreviation's matching offset.
            let probe = Self::Tzif(*zone).resolve_local(local)?;
            return local.checked_sub(i64::from(self.resolve(probe).0) * 1_000_000);
        }
        let probes = [
            local.checked_sub(2 * DAY_US).unwrap_or(local),
            local,
            local.checked_add(2 * DAY_US).unwrap_or(local),
        ];
        let mut matching: Option<i64> = None;
        let mut fallback: Option<i64> = None;
        for probe in probes {
            let offset = self.resolve(probe).0;
            let candidate = local.checked_sub(i64::from(offset).checked_mul(1_000_000)?)?;
            fallback = Some(fallback.map_or(candidate, |previous| previous.max(candidate)));
            if self.resolve(candidate).0 == offset {
                matching = Some(matching.map_or(candidate, |previous| previous.max(candidate)));
            }
        }
        matching.or(fallback)
    }

    /// The zone's offset east of UTC when it has only ever had one.
    pub(crate) fn constant_offset(&self) -> Option<i32> {
        match self {
            Timezone::Posix(zone) => match zone.daylight {
                Daylight::Never => Some(zone.std_off),
                Daylight::Always => Some(zone.dst_off),
                Daylight::Rules(_) => None,
            },
            Timezone::Tzif(slot) => super::tzif::constant_offset(*slot),
            Timezone::Abbreviation { .. } => None,
        }
    }

    pub(crate) fn visit_abbreviations(self, utc: i64, visit: impl FnMut(Abbreviation, i32, bool)) {
        match self {
            Self::Posix(zone) => {
                zone.visit_abbreviations(visit);
            }
            Self::Tzif(slot) | Self::Abbreviation { zone: slot, .. } => {
                super::tzif::visit_abbreviations(slot, utc, visit);
            }
        }
    }
}

impl PosixZone {
    fn part(&self, (start, length): (u8, u8)) -> &str {
        &self.name.as_str()[usize::from(start)..usize::from(start) + usize::from(length)]
    }

    fn std_abbrev(&self) -> Abbreviation {
        Abbreviation::from_str(self.part(self.std_abbrev))
    }

    fn dst_abbrev(&self) -> Abbreviation {
        Abbreviation::from_str(self.part(self.dst_abbrev))
    }

    pub(crate) fn visit_abbreviations(self, mut visit: impl FnMut(Abbreviation, i32, bool)) {
        visit(self.std_abbrev(), self.std_off, self.std_is_dst);
        if !matches!(self.daylight, Daylight::Never)
            && self.part(self.dst_abbrev) != self.part(self.std_abbrev)
        {
            visit(self.dst_abbrev(), self.dst_off, true);
        }
    }

    pub(crate) fn abbreviation(&self, name: &str) -> Option<(i32, Abbreviation, bool)> {
        if self.part(self.std_abbrev).eq_ignore_ascii_case(name) {
            Some((self.std_off, self.std_abbrev(), self.std_is_dst))
        } else if !matches!(self.daylight, Daylight::Never)
            && self.part(self.dst_abbrev).eq_ignore_ascii_case(name)
        {
            Some((self.dst_off, self.dst_abbrev(), true))
        } else {
            None
        }
    }

    pub(crate) fn resolve_info(&self, utc: i64) -> (i32, Abbreviation, bool) {
        let daylight = match self.daylight {
            Daylight::Never => false,
            Daylight::Always => true,
            Daylight::Rules(rules) => self.in_daylight(utc, rules),
        };
        if daylight {
            (self.dst_off, self.dst_abbrev(), true)
        } else {
            (self.std_off, self.std_abbrev(), self.std_is_dst)
        }
    }

    /// The two transitions `tzparse` records for `year`, as UTC instants
    /// with whether daylight time begins there; none when the rules leave
    /// no standard time that year.
    fn year_transitions(&self, year: i64, rules: Dst) -> Option<[(i64, bool); 2]> {
        let mut start = transition_time(year, rules.start, -self.std_off);
        let mut end = transition_time(year, rules.end, -self.dst_off);
        let year_seconds = if is_leap(year) { 366 } else { 365 } * 86_400;
        let reversed = end < start;
        if reversed {
            core::mem::swap(&mut start, &mut end);
        }
        if !reversed && !(start < end && end - start < year_seconds) {
            return None;
        }
        let january = (days_from_civil(year, 1, 1) - PG_EPOCH_DAYS).saturating_mul(DAY_US);
        let at = |seconds: i64| january.saturating_add(seconds.saturating_mul(1_000_000));
        Some([(at(start), !reversed), (at(end), reversed)])
    }

    /// Whether the latest transition at or before `utc` began daylight
    /// time. The rules repeat every 400 years, as `tzparse`'s table does.
    fn in_daylight(&self, utc: i64, rules: Dst) -> bool {
        let (year, _, _) = civil_from_days(utc.div_euclid(DAY_US) + PG_EPOCH_DAYS);
        // A transition time may lie up to a week past its day, so the
        // following year's first transition can precede `utc`.
        let mut latest: Option<(i64, bool)> = None;
        for candidate in (year - 400..=year + 1).rev() {
            if let Some(transitions) = self.year_transitions(candidate, rules) {
                for (at, daylight) in transitions {
                    if at <= utc && latest.is_none_or(|(prior, _)| at > prior) {
                        latest = Some((at, daylight));
                    }
                }
            }
            if latest.is_some() && candidate < year - 1 {
                break;
            }
        }
        latest.is_some_and(|(_, daylight)| daylight)
    }
}

fn is_leap(year: i64) -> bool {
    (year % 4 == 0 && year % 100 != 0) || year % 400 == 0
}

fn days_in_month(year: i64, month: u32) -> i64 {
    match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        _ if is_leap(year) => 29,
        _ => 28,
    }
}

/// `transtime`: seconds from UTC midnight on January 1 of `year` to the
/// rule's transition, for a zone `west` seconds behind UTC before it.
fn transition_time(year: i64, rule: Rule, west: i32) -> i64 {
    let day = match rule.day {
        RuleDay::Julian(day) => day - 1 + i64::from(is_leap(year) && day >= 60),
        RuleDay::DayOfYear(day) => day,
        RuleDay::MonthWeek {
            month,
            week,
            weekday,
        } => {
            let first = days_from_civil(year, month, 1);
            let first_weekday = day_of_week(first - PG_EPOCH_DAYS) as i64;
            let mut day = (weekday - first_weekday).rem_euclid(7);
            for _ in 1..week {
                if day + 7 >= days_in_month(year, month) {
                    break;
                }
                day += 7;
            }
            first - days_from_civil(year, 1, 1) + day
        }
    };
    day * 86_400 + rule.seconds + i64::from(west)
}

/// Looks up an IANA zone name (case-insensitive) from TZif history.
pub fn lookup(name: &str) -> Option<Timezone> {
    super::tzif::load(name).map(Timezone::Tzif)
}

impl Timezone {
    /// The name `SHOW TimeZone` reports for a zone `pg_tzset` found: an
    /// installed zone's file name, else the specification upper-cased.
    pub fn name(&self) -> Abbreviation {
        match self {
            Self::Posix(zone) => zone.name,
            Self::Tzif(slot) => Abbreviation::from_str(super::tzif::name(*slot)),
            Self::Abbreviation { name, .. } => *name,
        }
    }
}

/// `pg_tzset`: an installed zone (case-insensitive, an optional leading
/// `:`), else a POSIX zone specification such as `UTC+3`, `xyz5` or
/// `EST5EDT,M3.2.0,M11.1.0`. A POSIX zone is named by its upper-cased
/// spelling, which also supplies its abbreviations.
pub fn pg_tzset(name: &str) -> Option<Timezone> {
    if name.len() > TZ_STRLEN_MAX {
        return None;
    }
    let mut upper = StackStr::<TZ_STRLEN_MAX>::new();
    for character in name.chars() {
        let _ = upper.write_char(character.to_ascii_uppercase());
    }
    if upper.as_str() == "GMT" {
        // PostgreSQL never loads a file for GMT, so it is plain UTC named GMT.
        return Some(Timezone::fixed(0, "GMT", false));
    }
    let file = upper.as_str().strip_prefix(':').unwrap_or(upper.as_str());
    if let Some(slot) = super::tzif::load(file) {
        return Some(Timezone::Tzif(slot));
    }
    if upper.as_str().starts_with(':') {
        return None;
    }
    parse_posix_tz(upper.as_str()).map(Timezone::Posix)
}

/// `pg_tzset_offset`: the zone PostgreSQL names `<-03>+03` for a fixed
/// offset `east` seconds ahead of UTC, or `None` beyond 167:59:59.
pub fn pg_tzset_offset(east: i32) -> Option<Timezone> {
    let west = -i64::from(east);
    let absolute = west.unsigned_abs();
    let mut offset = StackStr::<32>::new();
    let _ = write!(offset, "{:02}", absolute / 3600);
    if absolute % 3600 != 0 {
        let _ = write!(offset, ":{:02}", absolute % 3600 / 60);
        if absolute % 60 != 0 {
            let _ = write!(offset, ":{:02}", absolute % 60);
        }
    }
    let offset = offset.as_str();
    let name = if west > 0 {
        crate::stack_format!(80, "<-{offset}>+{offset}")
    } else {
        crate::stack_format!(80, "<+{offset}>-{offset}")
    };
    pg_tzset(name.as_str())
}

/// Input abbreviation precedence follows PostgreSQL's DecodeTimezoneAbbrev:
/// session-zone meanings, then Default. SET TimeZone uses named zones instead.
pub(crate) fn lookup_abbreviation(name: &str) -> Option<Timezone> {
    if name.is_empty() || !name.bytes().all(|byte| byte.is_ascii_alphabetic()) {
        return None;
    }
    match session() {
        Timezone::Posix(zone) => {
            if let Some((offset, abbreviation, is_dst)) = zone.abbreviation(name) {
                return Some(Timezone::fixed(offset, abbreviation.as_str(), is_dst));
            }
        }
        Timezone::Tzif(zone) | Timezone::Abbreviation { zone, .. } => {
            if let Some(meaning) = super::tzif::abbreviation_meaning(zone, name) {
                return Some(match meaning {
                    super::tzif::AbbreviationMeaning::Fixed { offset, is_dst } => {
                        Timezone::fixed(offset, name, is_dst)
                    }
                    super::tzif::AbbreviationMeaning::Historical => Timezone::Abbreviation {
                        zone,
                        name: Abbreviation::from_str(name),
                    },
                });
            }
        }
    }
    let &(abbreviation, definition) = super::timezone_abbreviation_data::DEFAULT
        .iter()
        .find(|(abbreviation, _)| abbreviation.eq_ignore_ascii_case(name))?;
    match definition {
        AbbreviationDefinition::Fixed { offset, is_dst } => {
            Some(Timezone::fixed(offset, abbreviation, is_dst))
        }
        AbbreviationDefinition::Dynamic(zone) => {
            let Timezone::Tzif(zone) = lookup(zone)? else {
                unreachable!("a named time zone retains its TZif history")
            };
            Some(Timezone::Abbreviation {
                zone,
                name: Abbreviation::from_str(abbreviation),
            })
        }
    }
}

/// `getnum`: a decimal number from `min` to `max` at `*at`.
fn number(text: &[u8], at: &mut usize, min: i64, max: i64) -> Option<i64> {
    if !text.get(*at).is_some_and(u8::is_ascii_digit) {
        return None;
    }
    let mut value = 0i64;
    while let Some(digit) = text.get(*at).filter(|byte| byte.is_ascii_digit()) {
        value = value * 10 + i64::from(digit - b'0');
        if value > max {
            return None;
        }
        *at += 1;
    }
    (value >= min).then_some(value)
}

/// `getsecs`: `hh[:mm[:ss]]`, hours to 167 (a week, less an hour) and a
/// 60th second for leap seconds.
fn seconds(text: &[u8], at: &mut usize) -> Option<i64> {
    let mut total = number(text, at, 0, 24 * 7 - 1)? * 3600;
    if text.get(*at) == Some(&b':') {
        *at += 1;
        total += number(text, at, 0, 59)? * 60;
        if text.get(*at) == Some(&b':') {
            *at += 1;
            total += number(text, at, 0, 60)?;
        }
    }
    Some(total)
}

/// `getoffset`: a signed `getsecs` value.
fn signed_seconds(text: &[u8], at: &mut usize) -> Option<i64> {
    let negative = match text.get(*at) {
        Some(b'-') => true,
        Some(b'+') => false,
        _ => return seconds(text, at),
    };
    *at += 1;
    let value = seconds(text, at)?;
    Some(if negative { -value } else { value })
}

/// `getzname` or, after `<`, `getqzname`: an abbreviation, which may be
/// empty. A quoted one ends at `>`, an unquoted one at a digit, `,`, `-`
/// or `+`.
fn zone_abbreviation(text: &str, at: &mut usize) -> Option<(u8, u8)> {
    let bytes = text.as_bytes();
    let (start, end) = if bytes.get(*at) == Some(&b'<') {
        let start = *at + 1;
        let length = bytes[start..].iter().position(|&byte| byte == b'>')?;
        *at = start + length + 1;
        (start, start + length)
    } else {
        let start = *at;
        while bytes
            .get(*at)
            .is_some_and(|&byte| !byte.is_ascii_digit() && !matches!(byte, b',' | b'-' | b'+'))
        {
            *at += 1;
        }
        (start, *at)
    };
    // Specifications are at most `TZ_STRLEN_MAX` bytes.
    Some((u8::try_from(start).ok()?, u8::try_from(end - start).ok()?))
}

/// `getrule`: `Jn`, `n` or `Mm.w.d`, then an optional `/time` that
/// defaults to 02:00:00.
fn rule(text: &[u8], at: &mut usize) -> Option<Rule> {
    let day = match text.get(*at) {
        Some(b'J') => {
            *at += 1;
            RuleDay::Julian(number(text, at, 1, 365)?)
        }
        Some(b'M') => {
            *at += 1;
            let month = number(text, at, 1, 12)? as u32;
            if text.get(*at) != Some(&b'.') {
                return None;
            }
            *at += 1;
            let week = number(text, at, 1, 5)? as u32;
            if text.get(*at) != Some(&b'.') {
                return None;
            }
            *at += 1;
            RuleDay::MonthWeek {
                month,
                week,
                weekday: number(text, at, 0, 6)?,
            }
        }
        Some(byte) if byte.is_ascii_digit() => RuleDay::DayOfYear(number(text, at, 0, 365)?),
        _ => return None,
    };
    let seconds = if text.get(*at) == Some(&b'/') {
        *at += 1;
        signed_seconds(text, at)?
    } else {
        2 * 3600
    };
    Some(Rule { day, seconds })
}

/// PostgreSQL's `tzparse` for a POSIX `TZ` string (a zone specification or
/// a TZif footer): `std offset [dst [offset] [,start[/time],end[/time]]]`.
/// Offsets count seconds *west* of UTC, so `UTC+3` is three hours behind
/// UTC; daylight time defaults to an hour ahead of standard time and to the
/// United States rules `M3.2.0,M11.1.0`.
pub fn parse_posix_tz(text: &str) -> Option<PosixZone> {
    if text.len() > TZ_STRLEN_MAX {
        return None;
    }
    let name = Abbreviation::from_str(text);
    let bytes = text.as_bytes();
    let mut at = 0usize;
    let std_abbrev = zone_abbreviation(text, &mut at)?;
    if at == bytes.len() {
        return None;
    }
    let std_west = signed_seconds(bytes, &mut at)?;
    let std_off = -(std_west as i32);
    if at == bytes.len() {
        return Some(PosixZone {
            std_off,
            dst_off: std_off,
            name,
            std_abbrev,
            dst_abbrev: std_abbrev,
            std_is_dst: false,
            daylight: Daylight::Never,
        });
    }
    let dst_abbrev = zone_abbreviation(text, &mut at)?;
    if dst_abbrev.1 == 0 {
        return None;
    }
    let dst_west = if at < bytes.len() && bytes[at] != b',' {
        signed_seconds(bytes, &mut at)?
    } else {
        std_west - 3600
    };
    const DEFAULT_RULES: &[u8] = b",M3.2.0,M11.1.0";
    let (rules, mut at) = if at == bytes.len() {
        (DEFAULT_RULES, 0)
    } else {
        (bytes, at)
    };
    if rules.get(at) != Some(&b',') {
        return None;
    }
    at += 1;
    let start = rule(rules, &mut at)?;
    if rules.get(at) != Some(&b',') {
        return None;
    }
    at += 1;
    let end = rule(rules, &mut at)?;
    if at != rules.len() {
        return None;
    }
    let mut zone = PosixZone {
        std_off,
        dst_off: -(dst_west as i32),
        name,
        std_abbrev,
        dst_abbrev,
        std_is_dst: false,
        daylight: Daylight::Rules(Dst { start, end }),
    };
    // The calendar repeats every 400 years; rules that leave no standard
    // time in any of them keep daylight time always.
    if (2000..2400).all(|year| zone.year_transitions(year, Dst { start, end }).is_none()) {
        zone.daylight = Daylight::Always;
    }
    Some(zone)
}

#[cfg(all(
    test,
    not(all(miri, feature = "miri-ownership")),
    not(feature = "sql-fixture-suite")
))]
mod tests {
    use super::*;

    const H: i32 = 3600;

    fn ts(y: i64, month: u32, d: u32, h: i64) -> i64 {
        (days_from_civil(y, month, d) - PG_EPOCH_DAYS) * DAY_US + h * 3_600_000_000
    }

    fn zone(name: &str) -> Timezone {
        super::super::tzif::init_catalog("/usr/share/zoneinfo").expect("zoneinfo");
        lookup(name).expect("installed TZif zone")
    }

    #[test]
    fn new_york_dst_transitions() {
        let ny = zone("America/New_York");
        // January: standard time, -5.
        assert_eq!(ny.resolve(ts(2021, 1, 15, 12)).0, -5 * H);
        // July: daylight time, -4.
        assert_eq!(ny.resolve(ts(2021, 7, 15, 12)).0, -4 * H);
        // Just after 2nd Sunday March (2021-03-14 07:00 UTC) is DST.
        assert_eq!(ny.resolve(ts(2021, 3, 14, 8)).0, -4 * H);
        // Just before is standard.
        assert_eq!(ny.resolve(ts(2021, 3, 14, 6)).0, -5 * H);
    }

    #[test]
    fn local_resolution_matches_postgresql_at_dst_boundaries() {
        let ny = zone("America/New_York");
        // PostgreSQL 18.4 uses the pre-transition standard offset for a
        // nonexistent spring time and the later standard occurrence for an
        // ambiguous autumn time.
        assert_eq!(
            ny.resolve_local(ts(2024, 3, 10, 2) + 30 * 60 * 1_000_000),
            Some(ts(2024, 3, 10, 7) + 30 * 60 * 1_000_000)
        );
        assert_eq!(
            ny.resolve_local(ts(2024, 11, 3, 1) + 30 * 60 * 1_000_000),
            Some(ts(2024, 11, 3, 6) + 30 * 60 * 1_000_000)
        );
    }

    #[test]
    fn europe_and_southern_hemisphere() {
        let bucharest = zone("Europe/Bucharest");
        assert_eq!(bucharest.resolve(ts(2021, 1, 1, 12)).0, 2 * H); // EET
        assert_eq!(bucharest.resolve(ts(2021, 7, 1, 12)).0, 3 * H); // EEST
        // Sydney: DST in the southern summer (January), standard in July.
        let sydney = zone("Australia/Sydney");
        assert_eq!(sydney.resolve(ts(2021, 1, 15, 3)).0, 11 * H); // AEDT
        assert_eq!(sydney.resolve(ts(2021, 7, 15, 3)).0, 10 * H); // AEST
    }

    #[test]
    fn posix_zone_specifications_follow_tzparse() {
        let offset = |spec: &str, at: i64| match pg_tzset(spec).unwrap() {
            Timezone::Posix(zone) => zone.resolve_info(at),
            _ => panic!("{spec} names no installed zone"),
        };
        // POSIX offsets count west of UTC; names are upper-cased.
        assert_eq!(pg_tzset("xyz5").unwrap().name().as_str(), "XYZ5");
        assert_eq!(offset("utc+3", ts(2024, 1, 5, 0)).0, -3 * H);
        let (east, name, dst) = offset("xyz5abc", ts(2024, 7, 5, 12));
        assert_eq!((east, name.as_str(), dst), (-4 * H, "ABC", true));
        // Julian days skip February 29; zero-based days count it.
        assert_eq!(offset("xyz5abc,J60,300", ts(2024, 3, 1, 12)).0, -4 * H);
        assert_eq!(offset("xyz5abc,J60,300", ts(2024, 2, 29, 12)).0, -5 * H);
        assert_eq!(offset("xyz5abc,J60,300", ts(2024, 10, 28, 12)).0, -5 * H);
        // Southern-hemisphere rules span the new year.
        let south = "xyz-10abc,M10.1.0,M4.1.0/3";
        assert_eq!(offset(south, ts(2024, 1, 5, 0)).0, 11 * H);
        assert_eq!(offset(south, ts(2024, 7, 5, 0)).0, 10 * H);
        // Rules that never leave daylight time keep it all year.
        assert_eq!(offset("xyz5abc,0/0,J365/25", ts(2024, 1, 5, 0)).0, -4 * H);
        for invalid in [
            "abc",
            "x",
            "xyz168",
            "xyz5:60",
            "xyz5abc,M3.2.0",
            "xyz5abc;M3.2.0,M11.1.0",
            "abc5def,M13.1.0,M1.1.0",
            "xyz5abc,J0,300",
            "<abc-3",
            ":xyz5",
        ] {
            assert!(pg_tzset(invalid).is_none(), "{invalid}");
        }
        assert!(pg_tzset(&"a".repeat(TZ_STRLEN_MAX)).is_none());
        assert_eq!(pg_tzset_offset(-3 * H).unwrap().name().as_str(), "<-03>+03");
        assert_eq!(
            pg_tzset_offset(12_615).unwrap().name().as_str(),
            "<+03:30:15>-03:30:15"
        );
        assert!(pg_tzset_offset(168 * H).is_none());
    }

    #[test]
    fn fixed_zones() {
        assert_eq!(Timezone::utc().resolve(ts(2021, 7, 1, 0)).0, 0);
        let kolkata = zone("Asia/Kolkata");
        assert_eq!(kolkata.resolve(ts(2021, 7, 1, 0)).0, 5 * H + 1800);
    }

    #[test]
    fn fixed_default_abbreviations_need_no_zone_catalog_or_runtime_allocation() {
        set_session(Timezone::utc());
        assert!(lookup("EST").is_none());
        crate::mem::guard::forbid_alloc(|| {
            for &(name, definition) in super::super::timezone_abbreviation_data::DEFAULT {
                if let AbbreviationDefinition::Fixed { offset, is_dst } = definition {
                    let zone = lookup_abbreviation(name).unwrap();
                    let (actual_offset, _, actual_dst) = zone.resolve_info(0);
                    assert_eq!((actual_offset, actual_dst), (offset, is_dst), "{name}");
                }
            }
            assert!(lookup_abbreviation("Unknown_Zone").is_none());
            assert_eq!(lookup_abbreviation("eSt").unwrap().resolve(0).0, -5 * H);
            for name in ["éééé", "Etc/éé"] {
                assert!(pg_tzset(name).is_none());
            }
        });
        assert!(lookup("EST").is_none());
    }
}
