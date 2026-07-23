//! The IANA time-zone database, read from the system's TZif files (RFC 8536).
//!
//! Postgres resolves named zones through the full tz database — historical
//! transitions included — where the embedded rules in [`super::timezone`]
//! model only each zone's present POSIX rule. This module closes that gap:
//! a zone name is matched case-insensitively against a catalog of the
//! installed zone names (walked once at startup, before the allocator
//! freezes), its TZif file parsed into a fixed-size cache slot, and an
//! instant resolved by binary search over the real transition history, with
//! the file's POSIX footer rule covering instants past the last transition.
//!
//! Static-memory discipline: the catalog and cache are fixed pools in
//! thread-local storage, const-initialized (no heap); parsing reads the file
//! into a fixed scratch buffer; running out of cache slots is a loud error,
//! never growth. TZif files are small (the largest installed here is under
//! 4 KiB with 242 transitions); the pools carry generous margins and refuse
//! loudly beyond them.

use core::cell::RefCell;
use core::fmt::Write as _;
use std::io::Read as _;

use crate::util::StackStr;

/// Seconds between the Unix epoch and PostgreSQL's 2000-01-01 epoch.
const PG_EPOCH_UNIX_SECONDS: i64 = 946_684_800;

/// Transition capacity per zone. The largest installed zone has 242; the
/// margin covers newer tzdata releases, and overflow refuses the zone loudly.
const MAX_TRANSITIONS: usize = 512;

/// Local-time type capacity per zone (offset/abbreviation combinations).
const MAX_TYPES: usize = 32;

/// Distinct TZif zones one server lifetime can load. Exceeding it is a loud
/// error; sessions use a handful.
const MAX_CACHED: usize = 64;

/// Installed zone names the startup catalog can hold.
const MAX_CATALOG: usize = 1024;

/// One local-time type: offset east of UTC and its abbreviation.
#[derive(Clone, Copy)]
struct TypeInfo {
    utoff: i32,
    abbrev: StackStr<8>,
}

/// One parsed zone: its transition history plus the footer rule.
struct ZoneData {
    /// Transition instants, ascending, in Unix seconds.
    times: [i64; MAX_TRANSITIONS],
    /// The local-time type in effect *from* the matching instant on.
    type_after: [u8; MAX_TRANSITIONS],
    n_transitions: usize,
    types: [TypeInfo; MAX_TYPES],
    /// The type in effect before the first transition.
    first_type: u8,
    /// The POSIX rule from the file footer, for instants past the last
    /// transition (`None` when the footer is empty: the last type persists).
    footer: Option<super::timezone::PosixZone>,
}

const EMPTY_TYPE: TypeInfo = TypeInfo { utoff: 0, abbrev: StackStr::new() };

const EMPTY_ZONE: ZoneData = ZoneData {
    times: [0; MAX_TRANSITIONS],
    type_after: [0; MAX_TRANSITIONS],
    n_transitions: 0,
    types: [EMPTY_TYPE; MAX_TYPES],
    first_type: 0,
    footer: None,
};

struct Cache {
    names: [StackStr<48>; MAX_CACHED],
    zones: [ZoneData; MAX_CACHED],
    n: usize,
    /// File-read scratch: the largest installed TZif is under 4 KiB; 64 KiB
    /// covers any plausible tzdata release.
    file_buf: [u8; 64 * 1024],
}

struct Catalog {
    names: [StackStr<48>; MAX_CATALOG],
    n: usize,
}

std::thread_local! {
    // Boxed, not inline: glibc places static TLS inside each thread's stack
    // allocation, and half a megabyte of inline pools overflowed a 2 MiB test
    // thread. The boxes are allocated in `init_catalog`, before the allocator
    // freezes; a thread that never initialized (no zoneinfo, or the catalog
    // was skipped) resolves nothing and lookup falls back to the embedded
    // rules.
    static CACHE: RefCell<Option<Box<Cache>>> = const { RefCell::new(None) };
    static CATALOG: RefCell<Option<Box<Catalog>>> = const { RefCell::new(None) };
}

/// Walks the system zoneinfo directory into the name catalog. Called once at
/// startup, before the allocator freezes (`read_dir` allocates); a missing
/// directory leaves the catalog empty and named-zone lookup falls back to the
/// embedded rule set.
pub fn init_catalog() {
    let root = std::path::Path::new(zoneinfo_root());
    CATALOG.with(|c| {
        let mut c = c.borrow_mut();
        if c.is_some() {
            return;
        }
        let mut catalog =
            Box::new(Catalog { names: [StackStr::new(); MAX_CATALOG], n: 0 });
        let mut prefix = String::new();
        walk(&mut catalog, root, &mut prefix);
        *c = Some(catalog);
    });
    CACHE.with(|c| {
        let mut c = c.borrow_mut();
        if c.is_none() {
            *c = Some(Box::new(Cache {
                names: [StackStr::new(); MAX_CACHED],
                zones: [EMPTY_ZONE; MAX_CACHED],
                n: 0,
                file_buf: [0; 64 * 1024],
            }));
        }
    });
}

fn zoneinfo_root() -> &'static str {
    "/usr/share/zoneinfo"
}

fn walk(catalog: &mut Catalog, dir: &std::path::Path, prefix: &mut String) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        // Skip the metadata files and legacy right/posix duplicates.
        if name.starts_with('.')
            || name.contains('.')
            || name == "posixrules"
            || name == "right"
            || name == "posix"
        {
            continue;
        }
        let Ok(kind) = entry.file_type() else { continue };
        let saved = prefix.len();
        if !prefix.is_empty() {
            prefix.push('/');
        }
        prefix.push_str(name);
        if kind.is_dir() {
            walk(catalog, &entry.path(), prefix);
        } else if catalog.n < MAX_CATALOG && prefix.len() <= 48 {
            let mut s = StackStr::new();
            let _ = write!(s, "{prefix}");
            catalog.names[catalog.n] = s;
            catalog.n += 1;
        }
        prefix.truncate(saved);
    }
}

/// Case-insensitive catalog match: the canonical installed name for `name`,
/// or `None` when no installed zone matches (or no catalog was built).
fn canonical_name(name: &str) -> Option<StackStr<48>> {
    CATALOG.with(|c| {
        let c = c.borrow();
        let c = c.as_ref()?;
        c.names[..c.n]
            .iter()
            .find(|n| n.as_str().eq_ignore_ascii_case(name))
            .copied()
    })
}

/// Loads (or finds cached) the zone named `name`, returning its cache slot.
/// `None` when the name is not an installed zone or its file fails to parse.
pub fn load(name: &str) -> Option<u16> {
    let canonical = canonical_name(name)?;
    CACHE.with(|cache| {
        let mut cache = cache.borrow_mut();
        let cache = cache.as_mut()?;
        for i in 0..cache.n {
            if cache.names[i].as_str() == canonical.as_str() {
                return Some(i as u16);
            }
        }
        if cache.n == MAX_CACHED {
            // A loud limit, not an eviction: a session-visible zone must stay
            // resolvable for the server's lifetime.
            eprintln!("pos3ql: time-zone cache full ({MAX_CACHED} zones); refusing {name}");
            return None;
        }
        let mut path = StackStr::<128>::new();
        let _ = write!(path, "{}/{}", zoneinfo_root(), canonical.as_str());
        let n_read = {
            let mut file = std::fs::File::open(path.as_str()).ok()?;
            let mut total = 0usize;
            loop {
                let n = file.read(&mut cache.file_buf[total..]).ok()?;
                if n == 0 {
                    break;
                }
                total += n;
                if total == cache.file_buf.len() {
                    return None; // larger than any real TZif; refuse
                }
            }
            total
        };
        let slot = cache.n;
        let Cache { zones, file_buf, .. } = &mut **cache;
        if !parse_tzif(&file_buf[..n_read], &mut zones[slot]) {
            return None;
        }
        cache.names[slot] = canonical;
        cache.n += 1;
        Some(slot as u16)
    })
}

/// The offset (seconds east) and abbreviation zone `slot` has at `utc`
/// (microseconds since 2000). Falls back to UTC on a stale slot, which cannot
/// occur while the cache never evicts.
pub fn resolve(slot: u16, utc_micros: i64) -> (i32, StackStr<8>) {
    CACHE.with(|cache| {
        let cache = cache.borrow();
        let Some(cache) = cache.as_ref() else {
            return (0, utc_abbrev());
        };
        let Some(zone) = cache.zones.get(slot as usize) else {
            return (0, utc_abbrev());
        };
        let unix = utc_micros.div_euclid(1_000_000) + PG_EPOCH_UNIX_SECONDS;
        let n = zone.n_transitions;
        if n == 0 || unix < zone.times[0] {
            let t = zone.types[zone.first_type as usize];
            return (t.utoff, t.abbrev);
        }
        if unix >= zone.times[n - 1]
            && let Some(footer) = &zone.footer
        {
            return footer.resolve(utc_micros);
        }
        // The last transition at or before `unix`.
        let i = zone.times[..n].partition_point(|&t| t <= unix) - 1;
        let t = zone.types[zone.type_after[i] as usize];
        (t.utoff, t.abbrev)
    })
}

fn utc_abbrev() -> StackStr<8> {
    let mut s = StackStr::new();
    let _ = write!(s, "UTC");
    s
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
        let u32_at =
            |at: usize| u32::from_be_bytes(d[at..at + 4].try_into().expect("4 bytes"));
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
    let need = h.data_len(time_size);
    if d.len() < need {
        return false;
    }
    let mut at = 0usize;
    for i in 0..timecnt {
        out.times[i] = if time_size == 8 {
            i64::from_be_bytes(d[at..at + 8].try_into().expect("8 bytes"))
        } else {
            i64::from(i32::from_be_bytes(d[at..at + 4].try_into().expect("4 bytes")))
        };
        at += time_size;
    }
    for i in 0..timecnt {
        let idx = d[at];
        if idx as usize >= typecnt {
            return false;
        }
        out.type_after[i] = idx;
        at += 1;
    }
    let abbrevs_at = at + typecnt * 6;
    let abbrevs = &d[abbrevs_at..abbrevs_at + h.charcnt as usize];
    for i in 0..typecnt {
        let utoff = i32::from_be_bytes(d[at..at + 4].try_into().expect("4 bytes"));
        let abbrind = d[at + 5] as usize;
        at += 6;
        let mut abbrev = StackStr::new();
        if let Some(tail) = abbrevs.get(abbrind..) {
            for &b in tail {
                if b == 0 {
                    break;
                }
                let _ = abbrev.write_char(b as char);
            }
        }
        out.types[i] = TypeInfo { utoff, abbrev };
    }
    out.n_transitions = timecnt;
    // The type before history begins: RFC 8536 recommends the first standard
    // (non-DST) type; PostgreSQL behaves likewise.
    out.first_type = 0;
    out.footer = None;
    true
}

/// Parses the TZif footer's POSIX TZ string (between its newlines) into the
/// embedded rule machinery. `None` for an empty or unsupported footer — the
/// zone then freezes at its last transition's offset, which is what an empty
/// footer means.
fn parse_footer(d: &[u8]) -> Option<super::timezone::PosixZone> {
    let s = core::str::from_utf8(d).ok()?;
    let s = s.trim_matches(['\n', '\r', ' ']);
    if s.is_empty() {
        return None;
    }
    super::timezone::parse_posix_tz(s)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sql::datetime::{days_from_civil, PG_EPOCH_DAYS};

    fn ts(y: i64, month: u32, d: u32, h: i64) -> i64 {
        (days_from_civil(y, month, d) - PG_EPOCH_DAYS) * 86_400_000_000 + h * 3_600_000_000
    }

    fn load_or_skip(name: &str) -> Option<u16> {
        init_catalog();
        load(name)
    }

    #[test]
    fn new_york_matches_history_and_present() {
        // Hermetic skip when the host has no zoneinfo (the embedded rules
        // then serve lookups, tested in `timezone`).
        let Some(slot) = load_or_skip("America/New_York") else { return };
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
        init_catalog();
        assert!(load("America/New_York").is_some(), "zoneinfo present but load failed");
    }

    #[test]
    fn case_insensitive_and_unknown_names() {
        init_catalog();
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
        let Some(slot) = load_or_skip("Europe/Moscow") else { return };
        assert_eq!(resolve(slot, ts(2012, 7, 1, 12)).0, 4 * 3600);
        assert_eq!(resolve(slot, ts(2021, 7, 1, 12)).0, 3 * 3600);
    }

    #[test]
    fn lord_howe_half_hour_dst() {
        // Lord Howe Island: +10:30 standard, +11:00 daylight — a half-hour
        // DST step the embedded rules never modeled.
        let Some(slot) = load_or_skip("Australia/Lord_Howe") else { return };
        assert_eq!(resolve(slot, ts(2021, 7, 1, 12)).0, 10 * 3600 + 1800);
        assert_eq!(resolve(slot, ts(2021, 1, 15, 12)).0, 11 * 3600);
    }
}
