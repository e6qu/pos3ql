//! Per-session configuration parameters (GUCs). `SET` writes them and `SHOW`
//! reads them. A value we cannot honor is rejected loudly — never silently
//! accepted-and-ignored. Parameters whose behavior is not yet implemented
//! accept only the value(s) consistent with what the engine actually does, so
//! a client that sets something we would not honor gets an error rather than a
//! false success. As behavior lands (DateStyle formatting, non-UTC time zones)
//! the accepted set widens here.

use crate::sql::eval::sqlstate;
use core::fmt::Write;

use crate::sql_err;
use crate::util::StackStr;

use super::datetime::{DateFormat, DateStyle, FieldOrder};
use super::eval::SqlError;

/// The `client_min_messages` severity threshold, ordered as PostgreSQL orders
/// it: a message is delivered to the client only when its own severity is at or
/// above this level. Declaration order is the rank (low to high).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum MessageLevel {
    Debug5,
    Debug4,
    Debug3,
    Debug2,
    Debug1,
    Log,
    Notice,
    Warning,
    Error,
}

impl MessageLevel {
    /// Whether a message of severity `msg` is shown at this threshold.
    pub fn allows(self, msg: MessageLevel) -> bool {
        msg >= self
    }

    pub fn as_str(self) -> &'static str {
        match self {
            MessageLevel::Debug5 => "debug5",
            MessageLevel::Debug4 => "debug4",
            MessageLevel::Debug3 => "debug3",
            MessageLevel::Debug2 => "debug2",
            MessageLevel::Debug1 => "debug1",
            MessageLevel::Log => "log",
            MessageLevel::Notice => "notice",
            MessageLevel::Warning => "warning",
            MessageLevel::Error => "error",
        }
    }

    fn parse(s: &str) -> Option<MessageLevel> {
        // `debug` with no digit is an accepted alias for debug2 in PostgreSQL.
        for lvl in [
            MessageLevel::Debug5,
            MessageLevel::Debug4,
            MessageLevel::Debug3,
            MessageLevel::Debug2,
            MessageLevel::Debug1,
            MessageLevel::Log,
            MessageLevel::Notice,
            MessageLevel::Warning,
            MessageLevel::Error,
        ] {
            if s.eq_ignore_ascii_case(lvl.as_str()) {
                return Some(lvl);
            }
        }
        if s.eq_ignore_ascii_case("debug") {
            return Some(MessageLevel::Debug2);
        }
        None
    }
}

/// Value-rendering and message-filtering settings derived from the session
/// GUCs, handed to the wire layer so DateStyle, TimeZone, and
/// client_min_messages affect output.
#[derive(Debug, Clone, Copy)]
pub struct RenderContext {
    pub datestyle: DateStyle,
    /// The session time zone; resolves offset + abbreviation per timestamp so
    /// DST is honored.
    pub parsed_timezone: super::timezone::Timezone,
    /// The client_min_messages threshold: NOTICE/WARNING below it are dropped.
    pub min_message_level: MessageLevel,
    /// `bytea_output = escape`: text-format bytea renders in the escape
    /// format (printable ASCII verbatim, `\\` for backslash, `\nnn` octal)
    /// instead of `\x` hex.
    pub bytea_escape: bool,
}

impl Default for RenderContext {
    fn default() -> Self {
        RenderContext {
            datestyle: DateStyle::default(),
            parsed_timezone: super::timezone::Timezone::utc(),
            min_message_level: MessageLevel::Notice,
            bytea_escape: false,
        }
    }
}

pub struct GucState {
    datestyle: StackStr<48>,
    timezone: StackStr<64>,
    /// Parsed current time zone, so rendering does not re-parse it.
    parsed_timezone: super::timezone::Timezone,

    client_encoding: StackStr<32>,
    application_name: StackStr<64>,
    search_path: StackStr<128>,
    client_min_messages: MessageLevel,
    extra_float_digits: StackStr<8>,
    lock_timeout: StackStr<24>,
    /// statement_timeout in milliseconds (0 = disabled), enforced at scan
    /// boundaries during execution.
    statement_timeout: StackStr<24>,
    row_security: StackStr<4>,
    /// bytea_output = escape (false = hex, the default).
    bytea_escape: bool,
}

impl Default for GucState {
    fn default() -> Self {
        Self::new()
    }
}

impl GucState {
    pub fn new() -> Self {
        let mut g = Self {
            datestyle: StackStr::new(),
            timezone: StackStr::new(),
            parsed_timezone: super::timezone::Timezone::utc(),
            client_encoding: StackStr::new(),
            application_name: StackStr::new(),
            search_path: StackStr::new(),
            client_min_messages: MessageLevel::Notice,
            extra_float_digits: StackStr::new(),
            lock_timeout: StackStr::new(),
            statement_timeout: StackStr::new(),
            row_security: StackStr::new(),
            bytea_escape: false,
        };
        let _ = write!(g.datestyle, "ISO, MDY");
        let _ = write!(g.timezone, "UTC");
        let _ = write!(g.client_encoding, "UTF8");
        let _ = write!(g.search_path, "\"$user\", public");
        let _ = write!(g.extra_float_digits, "1");
        let _ = write!(g.lock_timeout, "0");
        let _ = write!(g.statement_timeout, "0");
        let _ = write!(g.row_security, "on");
        g
    }

    /// The current statement_timeout in milliseconds (0 = disabled).
    pub fn statement_timeout_ms(&self) -> u64 {
        parse_timeout_ms(self.statement_timeout.as_str()).unwrap_or(0)
    }

    /// Applies `SET name = raw`. `raw` is the raw source text of the value
    /// (surrounding single quotes and whitespace are stripped here). Returns an
    /// error for an unknown parameter, a read-only parameter, or a value whose
    /// behavior is not implemented.
    pub fn set(&mut self, name: &str, raw: &str) -> Result<(), SqlError> {
        let v = unquote(raw);
        let is_default = v.eq_ignore_ascii_case("default");

        if name.eq_ignore_ascii_case("datestyle") {
            // DateStyle is cumulative: each SET updates only the components it
            // names, keeping the rest.
            let (fmt, ord) = if is_default {
                (DateFormat::Iso, Order3::Mdy)
            } else {
                let cur = parse_full(self.datestyle.as_str());
                apply_datestyle(cur, v).ok_or_else(|| unsupported_value("DateStyle", v))?
            };
            return store(&mut self.datestyle, canonical_datestyle(fmt, ord).as_str());
        }
        if name.eq_ignore_ascii_case("timezone") {
            // UTC, fixed numeric offsets, Etc/GMT±N, and named IANA zones (with
            // DST) are honored; an unknown zone is rejected loudly.
            let timezone = if is_default {
                super::timezone::Timezone::utc()
            } else {
                parse_timezone(v).ok_or_else(|| unsupported_value("TimeZone", v))?
            };
            store(&mut self.timezone, if is_default { "UTC" } else { v })?;
            self.parsed_timezone = timezone;
            return Ok(());
        }
        if name.eq_ignore_ascii_case("client_encoding") {
            // UTF8 is native; SQL_ASCII is byte-pass-through (no conversion), so
            // both are served without transcoding. Any other encoding would
            // require a conversion we do not implement.
            if is_default || is_utf8(v) {
                return store(&mut self.client_encoding, "UTF8");
            }
            if v.eq_ignore_ascii_case("sql_ascii") {
                return store(&mut self.client_encoding, "SQL_ASCII");
            }
            return Err(sql_err!(
                sqlstate::FEATURE_NOT_SUPPORTED,
                "conversion between {} and UTF8 is not supported",
                v
            ));
        }
        if name.eq_ignore_ascii_case("standard_conforming_strings") {
            if is_default || v.eq_ignore_ascii_case("on") {
                return Ok(());
            }
            return Err(sql_err!(
                sqlstate::FEATURE_NOT_SUPPORTED,
                "standard_conforming_strings can only be on (strings always conform)"
            ));
        }
        if name.eq_ignore_ascii_case("client_min_messages") {
            // Filters which NOTICE/WARNING messages reach the client. The
            // default is `notice`; an unrecognized level errors like PostgreSQL.
            self.client_min_messages = if is_default {
                MessageLevel::Notice
            } else {
                MessageLevel::parse(v).ok_or_else(|| {
                    sql_err!(
                        sqlstate::INVALID_PARAMETER_VALUE,
                        "invalid value for parameter \"client_min_messages\": \"{}\"",
                        v
                    )
                })?
            };
            return Ok(());
        }
        if name.eq_ignore_ascii_case("application_name") {
            return store(&mut self.application_name, if is_default { "" } else { v });
        }
        if name.eq_ignore_ascii_case("search_path") {
            // Name resolution searches pg_catalog + public; a search_path that
            // still reaches public is honored, anything else is not yet.
            if is_default || mentions_public(v) {
                return store(&mut self.search_path, if is_default { "\"$user\", public" } else { v });
            }
            return Err(sql_err!(
                sqlstate::FEATURE_NOT_SUPPORTED,
                "search_path without \"public\" is not supported yet"
            ));
        }
        if name.eq_ignore_ascii_case("extra_float_digits") {
            // Floats already render at their shortest exact round-trip form —
            // the extra_float_digits >= 0 behavior — so the value is validated
            // to PostgreSQL's range and retained for SHOW.
            let n: i32 = if is_default {
                1
            } else {
                v.parse().map_err(|_| {
                    sql_err!(
                        sqlstate::INVALID_PARAMETER_VALUE,
                        "invalid value for parameter \"extra_float_digits\": \"{}\"",
                        v
                    )
                })?
            };
            if !(-15..=3).contains(&n) {
                return Err(sql_err!(
                    sqlstate::INVALID_PARAMETER_VALUE,
                    "{} is outside the valid range for parameter \"extra_float_digits\" (-15 .. 3)",
                    n
                ));
            }
            self.extra_float_digits.clear();
            let _ = write!(self.extra_float_digits, "{n}");
            return Ok(());
        }
        if name.eq_ignore_ascii_case("lock_timeout") {
            // A write conflict fails fast (40001) rather than waiting on a
            // lock, so there is never a lock wait for lock_timeout to bound —
            // any value is trivially satisfied.
            return store(&mut self.lock_timeout, if is_default { "0" } else { v });
        }
        if name.eq_ignore_ascii_case("statement_timeout") {
            // Enforced at scan boundaries during execution.
            if is_default {
                return store(&mut self.statement_timeout, "0");
            }
            if parse_timeout_ms(v).is_none() {
                return Err(sql_err!(
                    sqlstate::INVALID_PARAMETER_VALUE,
                    "invalid value for parameter \"statement_timeout\": \"{}\"",
                    v
                ));
            }
            return store(&mut self.statement_timeout, v);
        }
        if name.eq_ignore_ascii_case("idle_in_transaction_session_timeout") {
            // No idle-in-transaction reaper yet, so only the disabled value is
            // honored; a non-zero value would be a silent no-operator.
            if is_default || v == "0" {
                return Ok(());
            }
            return Err(sql_err!(
                sqlstate::FEATURE_NOT_SUPPORTED,
                "{} is not enforced yet; only 0 (disabled) is accepted",
                name
            ));
        }
        if name.eq_ignore_ascii_case("bytea_output") {
            if is_default || v.eq_ignore_ascii_case("hex") {
                self.bytea_escape = false;
                return Ok(());
            }
            if v.eq_ignore_ascii_case("escape") {
                self.bytea_escape = true;
                return Ok(());
            }
            return Err(sql_err!(
                sqlstate::INVALID_PARAMETER_VALUE,
                "invalid value for parameter \"bytea_output\": \"{}\"",
                v
            ));
        }
        if name.eq_ignore_ascii_case("row_security") {
            // No row-level-security policies exist, so `on` and `offset` select
            // the same rows; the value is validated and retained for SHOW.
            let on = if is_default {
                true
            } else {
                parse_on_off(v).ok_or_else(|| unsupported_value("row_security", v))?
            };
            return store(&mut self.row_security, if on { "on" } else { "off" });
        }
        // Read-only parameters cannot be assigned.
        if is_read_only(name) {
            return Err(sql_err!(
                "55P02",
                "parameter \"{}\" cannot be changed",
                name
            ));
        }
        Err(sql_err!(
            sqlstate::UNDEFINED_OBJECT,
            "unrecognized configuration parameter \"{}\"",
            name
        ))
    }

    /// The current value for `SHOW name`, or None if the parameter is unknown
    /// here (the caller falls back to fixed server parameters).
    pub fn get(&self, name: &str) -> Option<&str> {
        if name.eq_ignore_ascii_case("datestyle") {
            Some(self.datestyle.as_str())
        } else if name.eq_ignore_ascii_case("timezone") {
            Some(self.timezone.as_str())
        } else if name.eq_ignore_ascii_case("client_encoding") {
            Some(self.client_encoding.as_str())
        } else if name.eq_ignore_ascii_case("application_name") {
            Some(self.application_name.as_str())
        } else if name.eq_ignore_ascii_case("search_path") {
            Some(self.search_path.as_str())
        } else if name.eq_ignore_ascii_case("client_min_messages") {
            Some(self.client_min_messages.as_str())
        } else if name.eq_ignore_ascii_case("extra_float_digits") {
            Some(self.extra_float_digits.as_str())
        } else if name.eq_ignore_ascii_case("lock_timeout") {
            Some(self.lock_timeout.as_str())
        } else if name.eq_ignore_ascii_case("row_security") {
            Some(self.row_security.as_str())
        } else if name.eq_ignore_ascii_case("statement_timeout") {
            Some(self.statement_timeout.as_str())
        } else if name.eq_ignore_ascii_case("idle_in_transaction_session_timeout") {
            Some("0")
        } else if name.eq_ignore_ascii_case("bytea_output") {
            Some(if self.bytea_escape { "escape" } else { "hex" })
        } else {
            None
        }
    }

    /// Value-rendering settings for the wire layer (DateStyle + zone).
    /// The session's resolved `TimeZone`.
    pub fn timezone(&self) -> super::timezone::Timezone {
        self.parsed_timezone
    }

    pub fn render(&self) -> RenderContext {
        let (format, ord) = parse_full(self.datestyle.as_str());
        let order = if ord == Order3::Dmy { FieldOrder::Dmy } else { FieldOrder::Mdy };
        RenderContext {
            datestyle: DateStyle { format, order },
            parsed_timezone: self.parsed_timezone,
            min_message_level: self.client_min_messages,
            bytea_escape: self.bytea_escape,
        }
    }
}

/// Parses a PostgreSQL boolean GUC value (on/offset/true/false/1/0), allocation-free.
fn parse_on_off(v: &str) -> Option<bool> {
    if ["on", "true", "yes", "1"].iter().any(|s| v.eq_ignore_ascii_case(s)) {
        Some(true)
    } else if ["off", "false", "no", "0"].iter().any(|s| v.eq_ignore_ascii_case(s)) {
        Some(false)
    } else {
        None
    }
}

/// Parses a `statement_timeout` value into milliseconds: a bare integer is
/// milliseconds, or a `ms`/`s`/`min`/`h`/`d` unit suffix scales it (matching
/// PostgreSQL). Returns None for a malformed value.
fn parse_timeout_ms(v: &str) -> Option<u64> {
    let t = v.trim().trim_matches('\'').trim();
    let (num_part, mult) = if let Some(n) = t.strip_suffix("ms") {
        (n, 1u64)
    } else if let Some(n) = t.strip_suffix("min") {
        (n, 60_000)
    } else if let Some(n) = t.strip_suffix('s') {
        (n, 1000)
    } else if let Some(n) = t.strip_suffix('h') {
        (n, 3_600_000)
    } else if let Some(n) = t.strip_suffix('d') {
        (n, 86_400_000)
    } else {
        (t, 1)
    };
    num_part.trim().parse::<u64>().ok().and_then(|n| n.checked_mul(mult))
}

fn store<const N: usize>(dst: &mut StackStr<N>, v: &str) -> Result<(), SqlError> {
    dst.clear();
    let _ = write!(dst, "{v}");
    if dst.is_truncated() {
        return Err(sql_err!(sqlstate::INVALID_PARAMETER_VALUE, "configuration value is too long"));
    }
    Ok(())
}

/// Strips one layer of surrounding single quotes (and doubled `''` escapes)
/// and trims whitespace, turning raw source text into the value.
fn unquote(raw: &str) -> &str {
    let t = raw.trim();
    if t.len() >= 2 && t.starts_with('\'') && t.ends_with('\'') {
        &t[1..t.len() - 1]
    } else {
        t
    }
}

/// Field order as PostgreSQL tracks it (YMD is preserved for SHOW even though
/// output renders it like MDY).
#[derive(Clone, Copy, PartialEq, Eq)]
enum Order3 {
    Mdy,
    Dmy,
    Ymd,
}

/// Parses a stored canonical DateStyle string (always well-formed).
fn parse_full(s: &str) -> (DateFormat, Order3) {
    apply_datestyle((DateFormat::Iso, Order3::Mdy), s).unwrap_or((DateFormat::Iso, Order3::Mdy))
}

/// Applies a `SET datestyle` value cumulatively onto `current`, returning the
/// new (format, order) or None if a token is unrecognized. Selecting German
/// without naming an order sets DMY, as PostgreSQL does.
fn apply_datestyle(current: (DateFormat, Order3), v: &str) -> Option<(DateFormat, Order3)> {
    let (mut fmt, mut ord) = current;
    let (mut mentioned_order, mut mentioned_german) = (false, false);
    let mut mentioned_any = false;
    for tok in v.split([',', ' ']) {
        let t = tok.trim();
        if t.is_empty() {
            continue;
        }
        mentioned_any = true;
        if t.eq_ignore_ascii_case("iso") {
            fmt = DateFormat::Iso;
        } else if t.eq_ignore_ascii_case("postgres") {
            fmt = DateFormat::Postgres;
        } else if t.eq_ignore_ascii_case("sql") {
            fmt = DateFormat::Sql;
        } else if t.eq_ignore_ascii_case("german") {
            fmt = DateFormat::German;
            mentioned_german = true;
        } else if t.eq_ignore_ascii_case("mdy") {
            ord = Order3::Mdy;
            mentioned_order = true;
        } else if t.eq_ignore_ascii_case("dmy") {
            ord = Order3::Dmy;
            mentioned_order = true;
        } else if t.eq_ignore_ascii_case("ymd") {
            ord = Order3::Ymd;
            mentioned_order = true;
        } else {
            return None;
        }
    }
    if !mentioned_any {
        return None;
    }
    if mentioned_german && !mentioned_order {
        ord = Order3::Dmy;
    }
    Some((fmt, ord))
}

fn canonical_datestyle(fmt: DateFormat, ord: Order3) -> StackStr<24> {
    let f = match fmt {
        DateFormat::Iso => "ISO",
        DateFormat::Postgres => "Postgres",
        DateFormat::Sql => "SQL",
        DateFormat::German => "German",
    };
    let o = match ord {
        Order3::Mdy => "MDY",
        Order3::Dmy => "DMY",
        Order3::Ymd => "YMD",
    };
    let mut s = StackStr::new();
    let _ = write!(s, "{f}, {o}");
    s
}

fn is_utc(v: &str) -> bool {
    v.eq_ignore_ascii_case("utc")
        || v.eq_ignore_ascii_case("gmt")
        || v.eq_ignore_ascii_case("etc/utc")
        || v.eq_ignore_ascii_case("universal")
}

/// Parses a time-zone value to (offset east of UTC in seconds, non-ISO
/// abbreviation), or None for a named/DST zone we do not model. Matches
/// PostgreSQL's inverted sign conventions: `Etc/GMT+5` is UTC-5 and a bare
/// `+05:30` is UTC-5:30.
pub fn parse_timezone(v: &str) -> Option<super::timezone::Timezone> {
    use super::timezone::Timezone;
    let t = v.trim();
    if is_utc(t) || t.eq_ignore_ascii_case("z") || t.eq_ignore_ascii_case("zulu") {
        return Some(Timezone::utc());
    }
    // A named IANA zone (with DST) from the embedded set.
    if let Some(timezone) = super::timezone::lookup(t) {
        return Some(timezone);
    }
    // Etc/GMT±N and GMT±N: the sign is inverted; the abbreviation PostgreSQL
    // shows is the resulting ISO offset (e.g. Etc/GMT+5 -> "-05").
    let etc = if t.len() >= 7 && t[..7].eq_ignore_ascii_case("etc/gmt") {
        Some(&t[7..])
    } else if t.len() >= 3 && t[..3].eq_ignore_ascii_case("gmt") {
        Some(&t[3..])
    } else {
        None
    };
    if let Some(rest) = etc {
        if rest.is_empty() {
            return Some(Timezone::utc());
        }
        let offset = -parse_hms(rest)?;
        return Some(Timezone::fixed(offset, super::datetime::iso_offset_string(offset).as_str()));
    }
    // Bare numeric offset: POSIX inverted sign, no abbreviation shown.
    if t.starts_with('+') || t.starts_with('-') {
        return Some(Timezone::fixed(-parse_hms(t)?, ""));
    }
    None
}

/// Parses `±HH[:MM[:SS]]` to signed seconds (sign as written, not inverted).
fn parse_hms(s: &str) -> Option<i32> {
    let (sign, rest) = match s.strip_prefix('-') {
        Some(r) => (-1, r),
        None => (1, s.strip_prefix('+').unwrap_or(s)),
    };
    let mut parts = rest.split(':');
    let hh: i32 = parts.next()?.parse().ok()?;
    let mm: i32 = match parts.next() {
        Some(x) => x.parse().ok()?,
        None => 0,
    };
    let ss: i32 = match parts.next() {
        Some(x) => x.parse().ok()?,
        None => 0,
    };
    if parts.next().is_some()
        || !(0..=24).contains(&hh)
        || !(0..60).contains(&mm)
        || !(0..60).contains(&ss)
    {
        return None;
    }
    Some(sign * (hh * 3600 + mm * 60 + ss))
}

fn is_utf8(v: &str) -> bool {
    v.eq_ignore_ascii_case("utf8") || v.eq_ignore_ascii_case("utf-8") || v.eq_ignore_ascii_case("unicode")
}

fn mentions_public(v: &str) -> bool {
    v.split(',').any(|p| p.trim().eq_ignore_ascii_case("public"))
}

fn is_read_only(name: &str) -> bool {
    const READ_ONLY: &[&str] = &[
        "server_version",
        "server_version_num",
        "server_encoding",
        "is_superuser",
        "integer_datetimes",
        "in_hot_standby",
        "max_connections",
    ];
    READ_ONLY.iter().any(|r| name.eq_ignore_ascii_case(r))
}

fn unsupported_value(param: &str, v: &str) -> SqlError {
    sql_err!(
        sqlstate::FEATURE_NOT_SUPPORTED,
        "{} \"{}\" is not supported yet (only the default is implemented)",
        param,
        v
    )
}
