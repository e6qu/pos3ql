//! Time zones with DST. A [`Timezone`] resolves the UTC offset and abbreviation for
//! a specific instant, so `timestamptz` output is correct across daylight-time
//! transitions. Fixed-offset zones (UTC, `+05:30`) are the degenerate case
//! with no transitions.
//!
//! DST rules follow the POSIX form (the `Mm.w.d` transition: month, week,
//! day-of-week, local seconds-after-midnight). This is exact for current rules;
//! it does not model historical rule changes (PostgreSQL's full timezone database
//! does), which only affects timestamps from before a zone's present rule.

use core::fmt::Write;

use super::datetime::{civil_from_days, day_of_week, days_from_civil, PG_EPOCH_DAYS};
use crate::util::StackStr;

const DAY_US: i64 = 86_400_000_000;

/// One DST transition: the `week`-th `dow` (0 = Sunday) of `month`, at `seconds`
/// after local midnight. `week` 5 means "last".
#[derive(Debug, Clone, Copy)]
pub struct Trans {
    month: u32,
    week: u32,
    dow: usize,
    seconds: i64,
}

#[derive(Debug, Clone, Copy)]
pub struct Dst {
    /// Transition into daylight time, expressed in standard local time.
    start: Trans,
    /// Transition back to standard time, expressed in daylight local time.
    end: Trans,
}

/// A resolved time zone.
#[derive(Debug, Clone, Copy)]
pub struct Timezone {
    std_off: i32,
    dst_off: i32,
    std_abbrev: StackStr<8>,
    dst_abbrev: StackStr<8>,
    dst: Option<Dst>,
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
    pub fn fixed(off_secs: i32, abbrev: &str) -> Timezone {
        let mut a = StackStr::new();
        let _ = write!(a, "{abbrev}");
        Timezone { std_off: off_secs, dst_off: off_secs, std_abbrev: a, dst_abbrev: a, dst: None }
    }

    pub fn utc() -> Timezone {
        Timezone::fixed(0, "UTC")
    }

    /// The offset (seconds east of UTC) and abbreviation in effect at `utc`
    /// microseconds-since-2000.
    pub fn resolve(&self, utc: i64) -> (i32, StackStr<8>) {
        match self.dst {
            Some(d) if self.in_dst(utc, d) => (self.dst_off, self.dst_abbrev),
            _ => (self.std_off, self.std_abbrev),
        }
    }

    fn in_dst(&self, utc: i64, d: Dst) -> bool {
        // Determine the local year from the standard-time projection.
        let local = utc + self.std_off as i64 * 1_000_000;
        let (year, _, _) = civil_from_days(local.div_euclid(DAY_US) + PG_EPOCH_DAYS);
        let start = trans_instant(year, d.start, self.std_off);
        let end = trans_instant(year, d.end, self.dst_off);
        if start <= end {
            (start..end).contains(&utc) // northern hemisphere
        } else {
            utc >= start || utc < end // southern hemisphere (DST spans New Year)
        }
    }
}

/// UTC microseconds-since-2000 of a transition in `year`, given the local
/// wall-clock offset that applies just before it.
fn trans_instant(year: i64, t: Trans, off_secs: i32) -> i64 {
    let first = days_from_civil(year, t.month, 1) - PG_EPOCH_DAYS; // days since 2000
    let first_dow = day_of_week(first); // 0 = Sunday
    let mut dom = 1 + (t.dow + 7 - first_dow) % 7 + (t.week as usize - 1) * 7;
    let dim = days_in_month(year, t.month) as usize;
    if dom > dim {
        dom -= 7; // "last" (week 5) or an overshoot clamps to the final occurrence
    }
    let day = days_from_civil(year, t.month, dom as u32) - PG_EPOCH_DAYS;
    (day * DAY_US + t.seconds * 1_000_000) - off_secs as i64 * 1_000_000
}

fn days_in_month(y: i64, m: u32) -> u32 {
    match m {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        _ => {
            if (y % 4 == 0 && y % 100 != 0) || y % 400 == 0 {
                29
            } else {
                28
            }
        }
    }
}

const H: i32 = 3600;

fn build(std_off: i32, dst_off: i32, std_ab: &str, dst_ab: &str, dst: Option<Dst>) -> Timezone {
    let mut s = StackStr::new();
    let _ = write!(s, "{std_ab}");
    let mut d = StackStr::new();
    let _ = write!(d, "{dst_ab}");
    Timezone { std_off, dst_off, std_abbrev: s, dst_abbrev: d, dst }
}

/// US rule (2007+): spring 2nd Sunday March 02:00 std, fall 1st Sunday
/// November 02:00 dst.
fn us() -> Dst {
    Dst {
        start: Trans { month: 3, week: 2, dow: 0, seconds: 2 * 3600 },
        end: Trans { month: 11, week: 1, dow: 0, seconds: 2 * 3600 },
    }
}

/// EU rule: last Sunday March, last Sunday October, both at 01:00 UTC. The
/// local wall-clock time depends on the zone's standard offset.
fn eu(std_off_hours: i32) -> Dst {
    let start_local = (1 + std_off_hours) * 3600; // 01:00 UTC in std local time
    let end_local = (1 + std_off_hours + 1) * 3600; // 01:00 UTC in dst local time
    Dst {
        start: Trans { month: 3, week: 5, dow: 0, seconds: start_local as i64 },
        end: Trans { month: 10, week: 5, dow: 0, seconds: end_local as i64 },
    }
}

/// Australian rule: spring 1st Sunday October 02:00 std, fall 1st Sunday
/// April 03:00 dst (southern hemisphere — DST spans the new year).
fn au() -> Dst {
    Dst {
        start: Trans { month: 10, week: 1, dow: 0, seconds: 2 * 3600 },
        end: Trans { month: 4, week: 1, dow: 0, seconds: 3 * 3600 },
    }
}

/// New Zealand rule: last Sunday September 02:00 std, 1st Sunday April 03:00 dst.
fn nz() -> Dst {
    Dst {
        start: Trans { month: 9, week: 5, dow: 0, seconds: 2 * 3600 },
        end: Trans { month: 4, week: 1, dow: 0, seconds: 3 * 3600 },
    }
}

/// Looks up an IANA zone name (case-insensitive). Returns the resolved zone, or
/// `None` if it is not in the embedded set.
pub fn lookup(name: &str) -> Option<Timezone> {
    let n = name;
    let eq = |a: &str| n.eq_ignore_ascii_case(a);
    // North America
    if eq("America/New_York") || eq("US/Eastern") {
        return Some(build(-5 * H, -4 * H, "EST", "EDT", Some(us())));
    }
    if eq("America/Chicago") || eq("US/Central") {
        return Some(build(-6 * H, -5 * H, "CST", "CDT", Some(us())));
    }
    if eq("America/Denver") || eq("US/Mountain") {
        return Some(build(-7 * H, -6 * H, "MST", "MDT", Some(us())));
    }
    if eq("America/Los_Angeles") || eq("US/Pacific") {
        return Some(build(-8 * H, -7 * H, "PST", "PDT", Some(us())));
    }
    if eq("America/Phoenix") || eq("US/Arizona") {
        return Some(build(-7 * H, -7 * H, "MST", "MST", None));
    }
    if eq("America/Anchorage") || eq("US/Alaska") {
        return Some(build(-9 * H, -8 * H, "AKST", "AKDT", Some(us())));
    }
    if eq("America/Halifax") {
        return Some(build(-4 * H, -3 * H, "AST", "ADT", Some(us())));
    }
    if eq("America/Sao_Paulo") {
        return Some(build(-3 * H, -3 * H, "-03", "-03", None));
    }
    if eq("America/Toronto") {
        return Some(build(-5 * H, -4 * H, "EST", "EDT", Some(us())));
    }
    // Western Europe (CET/CEST, +1)
    if eq("Europe/Paris")
        || eq("Europe/Berlin")
        || eq("Europe/Madrid")
        || eq("Europe/Rome")
        || eq("Europe/Amsterdam")
        || eq("Europe/Brussels")
        || eq("Europe/Zurich")
        || eq("Europe/Warsaw")
        || eq("Europe/Prague")
        || eq("Europe/Stockholm")
        || eq("Europe/Vienna")
        || eq("Europe/Budapest")
        || eq("Europe/Oslo")
        || eq("Europe/Copenhagen")
    {
        return Some(build(H, 2 * H, "CET", "CEST", Some(eu(1))));
    }
    if eq("Europe/London") || eq("Europe/Dublin") || eq("Europe/Lisbon") {
        return Some(build(0, H, "GMT", "BST", Some(eu(0))));
    }
    // Eastern Europe (EET/EEST, +2)
    if eq("Europe/Bucharest")
        || eq("Europe/Athens")
        || eq("Europe/Helsinki")
        || eq("Europe/Kyiv")
        || eq("Europe/Kiev")
        || eq("Europe/Riga")
        || eq("Europe/Sofia")
    {
        return Some(build(2 * H, 3 * H, "EET", "EEST", Some(eu(2))));
    }
    if eq("Europe/Moscow") {
        return Some(build(3 * H, 3 * H, "MSK", "MSK", None));
    }
    // Asia (mostly no DST)
    if eq("Asia/Kolkata") || eq("Asia/Calcutta") {
        return Some(build(5 * H + 1800, 5 * H + 1800, "IST", "IST", None));
    }
    if eq("Asia/Tokyo") {
        return Some(build(9 * H, 9 * H, "JST", "JST", None));
    }
    if eq("Asia/Shanghai") || eq("Asia/Hong_Kong") || eq("Asia/Singapore") {
        return Some(build(8 * H, 8 * H, "CST", "CST", None));
    }
    if eq("Asia/Dubai") {
        return Some(build(4 * H, 4 * H, "+04", "+04", None));
    }
    if eq("Asia/Seoul") {
        return Some(build(9 * H, 9 * H, "KST", "KST", None));
    }
    // Oceania
    if eq("Australia/Sydney") || eq("Australia/Melbourne") {
        return Some(build(10 * H, 11 * H, "AEST", "AEDT", Some(au())));
    }
    if eq("Australia/Brisbane") {
        return Some(build(10 * H, 10 * H, "AEST", "AEST", None));
    }
    if eq("Pacific/Auckland") {
        return Some(build(12 * H, 13 * H, "NZST", "NZDT", Some(nz())));
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sql::datetime::days_from_civil;

    fn ts(y: i64, month: u32, d: u32, h: i64) -> i64 {
        (days_from_civil(y, month, d) - PG_EPOCH_DAYS) * DAY_US + h * 3_600_000_000
    }

    #[test]
    fn new_york_dst_transitions() {
        let ny = lookup("America/New_York").unwrap();
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
    fn europe_and_southern_hemisphere() {
        let bucharest = lookup("Europe/Bucharest").unwrap();
        assert_eq!(bucharest.resolve(ts(2021, 1, 1, 12)).0, 2 * H); // EET
        assert_eq!(bucharest.resolve(ts(2021, 7, 1, 12)).0, 3 * H); // EEST
        // Sydney: DST in the southern summer (January), standard in July.
        let sydney = lookup("Australia/Sydney").unwrap();
        assert_eq!(sydney.resolve(ts(2021, 1, 15, 3)).0, 11 * H); // AEDT
        assert_eq!(sydney.resolve(ts(2021, 7, 15, 3)).0, 10 * H); // AEST
    }

    #[test]
    fn fixed_zones() {
        assert_eq!(Timezone::utc().resolve(ts(2021, 7, 1, 0)).0, 0);
        let kolkata = lookup("Asia/Kolkata").unwrap();
        assert_eq!(kolkata.resolve(ts(2021, 7, 1, 0)).0, 5 * H + 1800);
    }
}
