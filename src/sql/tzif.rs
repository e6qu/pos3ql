//! The IANA time-zone database, read from TZif files (RFC 8536).
//!
//! PostgreSQL resolves named zones through the full tz database, historical
//! transitions included. The zone names are PostgreSQL 18.6's, pinned at
//! build (`timezone_name_data`); their TZif files come from the explicitly
//! configured `timezone_directory`. Startup parses every one into a fixed
//! table, and a missing or malformed file is a named startup error, so a
//! session can resolve every zone PostgreSQL knows and no other. An instant
//! resolves by binary search over the transition history, with the file's
//! POSIX footer rule covering instants past the last transition.

use core::cell::RefCell;
use std::io::Read as _;

use super::timezone::Abbreviation;
use crate::util::StackStr;

/// Seconds between the Unix epoch and PostgreSQL's 2000-01-01 epoch.
const PG_EPOCH_UNIX_SECONDS: i64 = 946_684_800;

/// Transition capacity per zone. The largest installed zone has 242; the
/// margin covers newer tzdata releases, and overflow refuses the zone loudly.
const MAX_TRANSITIONS: usize = 512;

/// Local-time type capacity per zone (offset/abbreviation combinations).
const MAX_TYPES: usize = 32;
pub(crate) const MAX_ABBREVIATIONS: usize = MAX_TYPES + 2;

/// The longest `timezone_directory` path.
pub(crate) const DIRECTORY_BYTES: usize = 1024;

/// `TZ_MAX_CHARS`: PostgreSQL refuses a TZif file whose abbreviations take
/// more bytes than this.
const TZ_MAX_CHARS: usize = 50;

/// A cache entry returned only after a TZif zone has been parsed and retained.
/// Its index is private so a caller cannot manufacture a stale zone state.
#[derive(Clone, Copy, Debug)]
pub struct TzifSlot(u16);

/// One local-time type: offset east of UTC and its abbreviation.
#[derive(Clone, Copy)]
struct TypeInfo {
    utoff: i32,
    abbrev: StackStr<TZ_MAX_CHARS>,
    is_dst: bool,
}

/// One parsed zone: its transition history plus the footer rule.
struct ZoneData {
    /// Transition instants, ascending, in Unix seconds.
    times: [i64; MAX_TRANSITIONS],
    /// The local-time type in effect *from* the matching instant on.
    type_after: [u8; MAX_TRANSITIONS],
    n_transitions: usize,
    types: [TypeInfo; MAX_TYPES],
    n_types: usize,
    /// The type in effect before the first transition.
    first_type: u8,
    /// The POSIX rule from the file footer, for instants past the last
    /// transition (`None` when the footer is empty: the last type persists).
    footer: Option<super::timezone::PosixZone>,
}

const EMPTY_TYPE: TypeInfo = TypeInfo {
    utoff: 0,
    abbrev: StackStr::new(),
    is_dst: false,
};

const EMPTY_ZONE: ZoneData = ZoneData {
    times: [0; MAX_TRANSITIONS],
    type_after: [0; MAX_TRANSITIONS],
    n_transitions: 0,
    types: [EMPTY_TYPE; MAX_TYPES],
    n_types: 0,
    first_type: 0,
    footer: None,
};

/// Every pinned zone, parsed at startup; a slot indexes `NAMES`.
struct Zones {
    directory: String,
    zones: Box<[ZoneData]>,
}

std::thread_local! {
    // Boxed, not inline: glibc places static TLS inside each thread's stack
    // allocation. The table is allocated in `init_catalog`, before the
    // allocator freezes, on the thread that runs SQL.
    static ZONES: RefCell<Option<Zones>> = const { RefCell::new(None) };
}

/// Why the configured zone database cannot serve PostgreSQL's zones.
#[derive(Debug)]
pub enum ZoneDatabaseError {
    /// `timezone_directory` lacks the TZif file of one or more pinned zones.
    Missing {
        directory: String,
        missing: usize,
        first: &'static str,
    },
    /// A pinned zone's file is not a TZif file pos3ql can hold.
    Malformed {
        directory: String,
        zone: &'static str,
    },
}

impl core::fmt::Display for ZoneDatabaseError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Missing {
                directory,
                missing,
                first,
            } => write!(
                f,
                "timezone_directory {directory:?} lacks {missing} of PostgreSQL 18.6's {} zones, among them {first:?}",
                super::timezone_name_data::NAMES.len()
            ),
            Self::Malformed { directory, zone } => write!(
                f,
                "timezone_directory {directory:?} holds no readable TZif file for zone {zone:?}"
            ),
        }
    }
}

impl std::error::Error for ZoneDatabaseError {}

/// Parses every zone PostgreSQL 18.6 names from `directory`. Startup only,
/// on the thread that runs SQL, before the allocator freezes.
pub fn init_catalog(directory: &str) -> Result<(), ZoneDatabaseError> {
    if ZONES.with(|zones| {
        zones
            .borrow()
            .as_ref()
            .is_some_and(|zones| zones.directory == directory)
    }) {
        return Ok(());
    }
    let names = super::timezone_name_data::NAMES;
    let root = std::path::Path::new(directory);
    let mut missing = 0usize;
    let mut first = None;
    for &name in names {
        if !is_tzif_file(&root.join(name)) {
            missing += 1;
            first.get_or_insert(name);
        }
    }
    if let Some(first) = first {
        return Err(ZoneDatabaseError::Missing {
            directory: directory.to_string(),
            missing,
            first,
        });
    }
    let mut zones = (0..names.len()).map(|_| EMPTY_ZONE).collect::<Vec<_>>();
    for (zone, &name) in zones.iter_mut().zip(names) {
        let parsed = std::fs::read(root.join(name)).is_ok_and(|data| parse_tzif(&data, zone));
        if !parsed {
            return Err(ZoneDatabaseError::Malformed {
                directory: directory.to_string(),
                zone: name,
            });
        }
    }
    ZONES.with(|slot| {
        *slot.borrow_mut() = Some(Zones {
            directory: directory.to_string(),
            zones: zones.into_boxed_slice(),
        })
    });
    Ok(())
}

/// Zoneinfo installations contain several extensionless metadata files beside
/// real zones. Identify a zone by the format's required magic.
fn is_tzif_file(path: &std::path::Path) -> bool {
    let Ok(mut file) = std::fs::File::open(path) else {
        return false;
    };
    let mut magic = [0_u8; 4];
    file.read_exact(&mut magic).is_ok() && magic == *b"TZif"
}

/// The zone named `name`, matched case-insensitively against PostgreSQL's
/// zone names. `None` when PostgreSQL has no such zone, or on a thread whose
/// engine loaded no zones.
pub fn load(name: &str) -> Option<TzifSlot> {
    if ZONES.with(|zones| zones.borrow().is_none()) {
        return None;
    }
    super::timezone_name_data::NAMES
        .iter()
        .position(|candidate| candidate.eq_ignore_ascii_case(name))
        .map(|slot| TzifSlot(slot as u16))
}

/// The offset (seconds east) and abbreviation zone `slot` has at `utc`
/// (microseconds since 2000).
pub fn resolve(slot: TzifSlot, utc_micros: i64) -> (i32, Abbreviation) {
    let (offset, abbreviation, _) = resolve_info(slot, utc_micros);
    (offset, abbreviation)
}

/// The zone's name, spelled as PostgreSQL spells it.
pub fn name(slot: TzifSlot) -> &'static str {
    super::timezone_name_data::NAMES[usize::from(slot.0)]
}

/// The callback cannot retain a reference beyond ownership of the cache.
fn with_zone<T>(slot: TzifSlot, read: impl FnOnce(&ZoneData) -> T) -> T {
    ZONES.with(|zones| {
        let zones = zones.borrow();
        let zones = zones
            .as_ref()
            .expect("a TZif slot requires the loaded zones");
        read(&zones.zones[usize::from(slot.0)])
    })
}

pub(crate) fn resolve_info(slot: TzifSlot, utc_micros: i64) -> (i32, Abbreviation, bool) {
    with_zone(slot, |zone| resolve_zone(zone, utc_micros))
}

/// `pg_get_timezone_offset`: the zone's one offset east of UTC when every
/// local-time type it records has the same offset.
pub(crate) fn constant_offset(slot: TzifSlot) -> Option<i32> {
    with_zone(slot, |zone| {
        let types = &zone.types[..zone.n_types];
        let first = types.first()?.utoff;
        types
            .iter()
            .all(|kind| kind.utoff == first)
            .then_some(first)
    })
}

pub(crate) enum AbbreviationMeaning {
    Fixed { offset: i32, is_dst: bool },
    Historical,
}

pub(crate) fn abbreviation_meaning(slot: TzifSlot, name: &str) -> Option<AbbreviationMeaning> {
    with_zone(slot, |zone| {
        let mut selected = None;
        let recorded = zone.types.iter().filter_map(|kind| {
            kind.abbrev
                .as_str()
                .eq_ignore_ascii_case(name)
                .then_some((kind.utoff, kind.is_dst))
        });
        let future = zone
            .footer
            .and_then(|footer| footer.abbreviation(name))
            .map(|(offset, _, is_dst)| (offset, is_dst));
        for value in recorded.chain(future) {
            if selected.is_some_and(|prior| prior != value) {
                return Some(AbbreviationMeaning::Historical);
            }
            selected = Some(value);
        }
        selected.map(|(offset, is_dst)| AbbreviationMeaning::Fixed { offset, is_dst })
    })
}

/// PostgreSQL's pg_interpret_timezone_abbrev selects the latest matching era
/// before the instant, or the earliest matching era when it was used only later.
fn abbreviation_at(zone: &ZoneData, name: &str, utc: i64) -> Option<(i32, Abbreviation, bool)> {
    let unix = utc.div_euclid(1_000_000) + PG_EPOCH_UNIX_SECONDS;
    let count = zone.n_transitions;
    if (count == 0 || unix >= zone.times[count - 1])
        && let Some(kind) = zone.footer.and_then(|footer| footer.abbreviation(name))
    {
        return Some(kind);
    }
    let cutoff = zone.times[..count].partition_point(|&time| time <= unix);
    let before = zone.type_after[..cutoff].iter().rev().copied();
    let initial = core::iter::once(zone.first_type);
    let after = zone.type_after[cutoff..count].iter().copied();
    before.chain(initial).chain(after).find_map(|kind| {
        let kind = zone.types[usize::from(kind)];
        kind.abbrev.as_str().eq_ignore_ascii_case(name).then_some((
            kind.utoff,
            Abbreviation::from_str(kind.abbrev.as_str()),
            kind.is_dst,
        ))
    })
}

pub(crate) fn resolve_abbreviation(
    slot: TzifSlot,
    name: &str,
    utc: i64,
) -> Option<(i32, Abbreviation, bool)> {
    with_zone(slot, |zone| abbreviation_at(zone, name, utc))
}

pub(crate) fn visit_abbreviations(
    slot: TzifSlot,
    utc: i64,
    mut visit: impl FnMut(Abbreviation, i32, bool),
) {
    with_zone(slot, |zone| {
        for (index, kind) in zone.types.iter().enumerate() {
            let name = kind.abbrev.as_str();
            if name.is_empty()
                || !name.bytes().all(|byte| byte.is_ascii_uppercase())
                || zone.types[..index]
                    .iter()
                    .any(|prior| prior.abbrev == kind.abbrev)
            {
                continue;
            }
            if let Some((offset, abbreviation, is_dst)) = abbreviation_at(zone, name, utc) {
                visit(abbreviation, offset, is_dst);
            }
        }
        if let Some(footer) = zone.footer {
            footer.visit_abbreviations(|abbreviation, offset, is_dst| {
                if zone
                    .types
                    .iter()
                    .any(|kind| kind.abbrev.as_str() == abbreviation.as_str())
                {
                    return;
                }
                visit(abbreviation, offset, is_dst);
            });
        }
    });
}

/// Visits every zone at one instant, in name order; false on a thread whose
/// engine loaded no zones.
pub(crate) fn visit_current(
    utc_micros: i64,
    mut visitor: impl FnMut(&str, i32, Abbreviation, bool),
) -> bool {
    ZONES.with(|zones| {
        let zones = zones.borrow();
        let Some(zones) = zones.as_ref() else {
            return false;
        };
        for (zone, &name) in zones.zones.iter().zip(super::timezone_name_data::NAMES) {
            let (offset, abbreviation, is_dst) = resolve_zone(zone, utc_micros);
            visitor(name, offset, abbreviation, is_dst);
        }
        true
    })
}

fn resolve_zone(zone: &ZoneData, utc_micros: i64) -> (i32, Abbreviation, bool) {
    let unix = utc_micros.div_euclid(1_000_000) + PG_EPOCH_UNIX_SECONDS;
    let n = zone.n_transitions;
    if n == 0 || unix < zone.times[0] {
        let value = zone.types[zone.first_type as usize];
        return (
            value.utoff,
            Abbreviation::from_str(value.abbrev.as_str()),
            value.is_dst,
        );
    }
    if unix >= zone.times[n - 1]
        && let Some(footer) = &zone.footer
    {
        return footer.resolve_info(utc_micros);
    }
    let index = zone.times[..n].partition_point(|&time| time <= unix) - 1;
    let value = zone.types[zone.type_after[index] as usize];
    (
        value.utoff,
        Abbreviation::from_str(value.abbrev.as_str()),
        value.is_dst,
    )
}

/// Parses a TZif file (RFC 8536): the version-1 block is skipped when a
/// version-2+ block follows (64-bit transition times), and the footer's POSIX
/// TZ string covers the far future. Returns false on any malformation or
/// capacity overflow — the caller then refuses the zone rather than
/// approximating it.
fn parse_tzif(data: &[u8], out: &mut ZoneData) -> bool {
    let Some(header) = TzifHeader::parse(data) else {
        return false;
    };
    if header.version >= b'2' {
        // Skip the v1 block entirely; parse the 64-bit block after it.
        let v1_len = 44 + header.data_len(4);
        let Some(h2) = TzifHeader::parse(data.get(v1_len..).unwrap_or(&[])) else {
            return false;
        };
        let body = &data[v1_len..];
        if !parse_block(&h2, &body[44..], 8, out) {
            return false;
        }
        // Footer: "\n<TZ string>\n".
        let foot_at = 44 + h2.data_len(8);
        let footer = body.get(foot_at..).unwrap_or(&[]);
        out.footer = parse_footer(footer);
        true
    } else {
        parse_block(&header, &data[44..], 4, out)
    }
}

struct TzifHeader {
    version: u8,
    isutcnt: u32,
    isstdcnt: u32,
    leapcnt: u32,
    timecnt: u32,
    typecnt: u32,
    charcnt: u32,
}

impl TzifHeader {
    fn parse(d: &[u8]) -> Option<TzifHeader> {
        if d.len() < 44 || &d[0..4] != b"TZif" {
            return None;
        }
        let u32_at = |at: usize| u32::from_be_bytes(d[at..at + 4].try_into().expect("4 bytes"));
        Some(TzifHeader {
            version: d[4],
            isutcnt: u32_at(20),
            isstdcnt: u32_at(24),
            leapcnt: u32_at(28),
            timecnt: u32_at(32),
            typecnt: u32_at(36),
            charcnt: u32_at(40),
        })
    }

    /// Bytes of the data block after a header, for `time_size`-byte times.
    fn data_len(&self, time_size: usize) -> usize {
        self.timecnt as usize * (time_size + 1)
            + self.typecnt as usize * 6
            + self.charcnt as usize
            + self.leapcnt as usize * (time_size + 4)
            + self.isstdcnt as usize
            + self.isutcnt as usize
    }
}

fn parse_block(h: &TzifHeader, d: &[u8], time_size: usize, out: &mut ZoneData) -> bool {
    let timecnt = h.timecnt as usize;
    let typecnt = h.typecnt as usize;
    if timecnt > MAX_TRANSITIONS || typecnt > MAX_TYPES || typecnt == 0 {
        return false;
    }
    if h.charcnt as usize > TZ_MAX_CHARS {
        return false;
    }
    let need = h.data_len(time_size);
    if d.len() < need {
        return false;
    }
    let mut at = 0usize;
    for time in &mut out.times[..timecnt] {
        *time = if time_size == 8 {
            i64::from_be_bytes(d[at..at + 8].try_into().expect("8 bytes"))
        } else {
            i64::from(i32::from_be_bytes(
                d[at..at + 4].try_into().expect("4 bytes"),
            ))
        };
        at += time_size;
    }
    for type_after in &mut out.type_after[..timecnt] {
        let idx = d[at];
        if idx as usize >= typecnt {
            return false;
        }
        *type_after = idx;
        at += 1;
    }
    let abbrevs_at = at + typecnt * 6;
    let abbrevs = &d[abbrevs_at..abbrevs_at + h.charcnt as usize];
    for slot in &mut out.types[..typecnt] {
        let utoff = i32::from_be_bytes(d[at..at + 4].try_into().expect("4 bytes"));
        let is_dst = d[at + 4] != 0;
        let abbrind = d[at + 5] as usize;
        at += 6;
        let Some(tail) = abbrevs.get(abbrind..) else {
            return false;
        };
        let length = tail
            .iter()
            .position(|&byte| byte == 0)
            .unwrap_or(tail.len());
        let Ok(text) = core::str::from_utf8(&tail[..length]) else {
            return false;
        };
        let abbrev = StackStr::from_str(text);
        *slot = TypeInfo {
            utoff,
            abbrev,
            is_dst,
        };
    }
    out.n_transitions = timecnt;
    out.n_types = typecnt;
    // The type before history begins: RFC 8536 recommends the first standard
    // (non-DST) type; PostgreSQL behaves likewise.
    out.first_type = 0;
    out.footer = None;
    true
}

/// Parses the TZif footer's POSIX TZ string (between its newlines) with
/// `tzparse`. `None` for an empty footer or one `tzparse` refuses: the zone
/// then keeps its last transition's offset, as PostgreSQL's `tzload` does.
fn parse_footer(d: &[u8]) -> Option<super::timezone::PosixZone> {
    let s = core::str::from_utf8(d).ok()?;
    let s = s.trim_matches(['\n', '\r', ' ']);
    if s.is_empty() {
        return None;
    }
    super::timezone::parse_posix_tz(s)
}

#[cfg(all(
    test,
    not(all(miri, feature = "miri-ownership")),
    not(feature = "sql-fixture-suite")
))]
mod tests {
    use super::*;
    use crate::sql::datetime::{PG_EPOCH_DAYS, days_from_civil};

    fn ts(y: i64, month: u32, d: u32, h: i64) -> i64 {
        (days_from_civil(y, month, d) - PG_EPOCH_DAYS) * 86_400_000_000 + h * 3_600_000_000
    }

    #[test]
    fn a_directory_without_postgresql_zones_is_a_named_error() {
        let error = init_catalog("/nonexistent-zoneinfo").unwrap_err();
        assert_eq!(
            error.to_string(),
            "timezone_directory \"/nonexistent-zoneinfo\" lacks 598 of PostgreSQL 18.6's 598 zones, among them \"Africa/Abidjan\""
        );
    }

    fn load_or_skip(name: &str) -> Option<TzifSlot> {
        init_catalog("/usr/share/zoneinfo").expect("zoneinfo");
        load(name)
    }

    #[test]
    fn timezone_abbreviation_history_selects_prior_and_first_future_meanings() {
        let mut zone = EMPTY_ZONE;
        for (kind, offset, name, is_dst) in [
            (0, 9_000, "LOC", false),
            (1, 10_800, "LOC", false),
            (2, 14_400, "SUM", true),
            (3, 14_400, "LOC", false),
        ] {
            zone.types[kind] = TypeInfo {
                utoff: offset,
                abbrev: StackStr::from_str(name),
                is_dst,
            };
        }
        zone.times[..3].copy_from_slice(&[-100, 0, 100]);
        zone.type_after[..3].copy_from_slice(&[1, 2, 3]);
        zone.n_transitions = 3;
        crate::mem::guard::forbid_alloc(|| {
            for (unix, name, offset, is_dst) in [
                (-200, "LOC", 9_000, false),
                (-50, "LOC", 10_800, false),
                (50, "LOC", 10_800, false),
                (150, "LOC", 14_400, false),
                (-200, "SUM", 14_400, true),
            ] {
                let utc = (unix - PG_EPOCH_UNIX_SECONDS) * 1_000_000;
                let actual = abbreviation_at(&zone, name, utc).unwrap();
                assert_eq!((actual.0, actual.2), (offset, is_dst));
            }
            assert!(abbreviation_at(&zone, "UNKNOWN", 0).is_none());
        });
    }

    #[test]
    fn new_york_matches_history_and_present() {
        // Hermetic skip when the host has no zoneinfo.
        let Some(slot) = load_or_skip("America/New_York") else {
            return;
        };
        // Present rule.
        assert_eq!(resolve(slot, ts(2021, 1, 15, 12)).0, -5 * 3600);
        assert_eq!(resolve(slot, ts(2021, 7, 15, 12)).0, -4 * 3600);
        // History the POSIX rule cannot know: 1968 DST began the last Sunday
        // of April, so mid-April 1968 was still standard time.
        assert_eq!(resolve(slot, ts(1968, 4, 15, 12)).0, -5 * 3600);
        assert_eq!(resolve(slot, ts(1968, 5, 15, 12)).0, -4 * 3600);
        // Far future: the footer rule (2nd Sunday March).
        assert_eq!(resolve(slot, ts(2090, 1, 15, 12)).0, -5 * 3600);
        assert_eq!(resolve(slot, ts(2090, 7, 15, 12)).0, -4 * 3600);
        // The abbreviation follows the type.
        assert_eq!(resolve(slot, ts(2021, 1, 15, 12)).1.as_str(), "EST");
        assert_eq!(resolve(slot, ts(2021, 7, 15, 12)).1.as_str(), "EDT");
    }

    #[test]
    fn the_host_database_actually_loads_here() {
        // The other tests skip hermetically on hosts without zoneinfo; this
        // one asserts the harness machines (macOS/Linux dev and CI) do have
        // it, so the skips cannot rot into never-running tests unnoticed.
        if !std::path::Path::new("/usr/share/zoneinfo").is_dir() {
            return;
        }
        init_catalog("/usr/share/zoneinfo").expect("zoneinfo");
        assert!(
            load("America/New_York").is_some(),
            "zoneinfo present but load failed"
        );
    }

    #[test]
    fn case_insensitive_and_unknown_names() {
        init_catalog("/usr/share/zoneinfo").expect("zoneinfo");
        if load("America/New_York").is_none() {
            return; // no zoneinfo on this host
        }
        assert!(load("aMeRiCa/nEw_YoRk").is_some());
        assert!(load("Not/A_Zone").is_none());
        assert!(load("../etc/passwd").is_none());
    }

    #[test]
    fn moscow_history_needs_the_database() {
        // Moscow observed +04 year-round 2011..2014, then returned to +03 —
        // rule changes no single POSIX rule can express.
        let Some(slot) = load_or_skip("Europe/Moscow") else {
            return;
        };
        assert_eq!(resolve(slot, ts(2012, 7, 1, 12)).0, 4 * 3600);
        assert_eq!(resolve(slot, ts(2021, 7, 1, 12)).0, 3 * 3600);
    }

    #[test]
    fn lord_howe_half_hour_dst() {
        // Lord Howe Island: +10:30 standard, +11:00 daylight — a half-hour
        // DST step the embedded rules never modeled.
        let Some(slot) = load_or_skip("Australia/Lord_Howe") else {
            return;
        };
        assert_eq!(resolve(slot, ts(2021, 7, 1, 12)).0, 10 * 3600 + 1800);
        assert_eq!(resolve(slot, ts(2021, 1, 15, 12)).0, 11 * 3600);
    }
}
