//! Per-session configuration parameters (GUCs). `SET` writes them and `SHOW`
//! reads them. A value we cannot honor is rejected loudly — never silently
//! accepted-and-ignored. Parameters whose behavior is not yet implemented
//! accept only the value(s) consistent with what the engine actually does, so
//! a client that sets something we would not honor gets an error rather than a
//! false success. As behavior lands (DateStyle formatting, non-UTC time zones)
//! the accepted set widens here.

use crate::sql::eval::sqlstate;
use core::cell::{Cell, RefCell};
use core::fmt::Write;

use crate::sql_err;
use crate::storage::MAX_SEQUENCES;
use crate::util::StackStr;

use super::datetime::{DateFormat, DateStyle, FieldOrder};
use super::eval::SqlError;

std::thread_local! {
    static ACTIVE_GUC: Cell<*const GucState> = const { Cell::new(core::ptr::null()) };
    static ACTIVE_RENDER: RefCell<Option<RenderContext>> = const { RefCell::new(None) };
}

/// Keeps expression-time setting mutations scoped to the connection whose
/// statement is executing. The engine is single-threaded, and the GUC payload
/// itself is interior-mutable; the guard prevents a pointer from surviving the
/// statement on either success or error.
pub struct EvalScope {
    prior: *const GucState,
    prior_render: Option<RenderContext>,
}

impl Drop for EvalScope {
    fn drop(&mut self) {
        ACTIVE_GUC.with(|active| active.set(self.prior));
        ACTIVE_RENDER.with(|active| *active.borrow_mut() = self.prior_render);
    }
}

pub fn enter_eval_scope(guc: &GucState) -> EvalScope {
    let pointer = guc as *const GucState;
    let prior = ACTIVE_GUC.with(|active| active.replace(pointer));
    let prior_render = ACTIVE_RENDER.with(|active| active.replace(Some(guc.render())));
    EvalScope {
        prior,
        prior_render,
    }
}

pub fn active_render() -> Option<RenderContext> {
    ACTIVE_RENDER.with(|active| *active.borrow())
}

/// Whether the active session permits row-security filtering. PostgreSQL's
/// `off` value is a safety check for dump tools: it raises instead of bypassing
/// a policy that would affect the query.
pub(crate) fn active_row_security() -> bool {
    ACTIVE_GUC.with(|active| {
        let pointer = active.get();
        if pointer.is_null() {
            return true;
        }
        // SAFETY: the pointer has the same dynamic extent as `enter_eval_scope`.
        let guc = unsafe { &*pointer };
        guc.store.borrow().current.row_security.as_str() == "on"
    })
}

pub fn set_active_config(
    name: &str,
    value: Option<&str>,
    local: bool,
) -> Result<StackStr<256>, SqlError> {
    ACTIVE_GUC.with(|active| {
        let pointer = active.get();
        if pointer.is_null() {
            return Err(sql_err!(
                sqlstate::FEATURE_NOT_SUPPORTED,
                "set_config is not available outside statement execution"
            ));
        }
        // SAFETY: `enter_eval_scope` installs a live per-connection GucState
        // for exactly the dynamic extent of execute_stmt and its Drop guard
        // clears it on every exit. GucState mutation is behind RefCell.
        let guc = unsafe { &*pointer };
        let result = guc.set_config(name, value, local)?;
        let render = guc.render();
        ACTIVE_RENDER.with(|active| *active.borrow_mut() = Some(render));
        if name.eq_ignore_ascii_case("timezone") {
            crate::sql::timezone::set_session(guc.timezone());
        }
        Ok(result)
    })
}

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

/// A connection's `currval`/`lastval` state: per-sequence last-`nextval` values,
/// plus the session-wide `lastval`. It is session-scoped and *not* transactional
/// — a `nextval` in a rolled-back transaction still defines `currval`, matching
/// PostgreSQL. `Cell`s let the pure expression evaluator record advances through
/// a shared borrow. Each slot carries the sequence's `created_at` stamp so a
/// reused catalog slot cannot leak a dropped sequence's value.
pub struct SeqSession {
    /// Per-slot `(created_at, value)`; `created_at == 0` means undefined.
    currvals: [Cell<(u64, i64)>; MAX_SEQUENCES],
    /// `(defined, value)` for `lastval` — the last `nextval` of any sequence.
    lastval: Cell<(bool, i64)>,
}

impl SeqSession {
    const fn new() -> Self {
        SeqSession {
            currvals: [const { Cell::new((0u64, 0i64)) }; MAX_SEQUENCES],
            lastval: Cell::new((false, 0)),
        }
    }

    /// Records a `nextval`: defines both this sequence's `currval` and `lastval`.
    pub fn record_nextval(&self, slot: usize, created_at: u64, value: i64) {
        self.currvals[slot].set((created_at, value));
        self.lastval.set((true, value));
    }

    /// Records a `setval`: defines this sequence's `currval` only (PostgreSQL
    /// does not let `setval` define `lastval`).
    pub fn record_setval(&self, slot: usize, created_at: u64, value: i64) {
        self.currvals[slot].set((created_at, value));
    }

    /// This sequence's `currval` in this session, if `nextval`/`setval` has
    /// defined it (the stamp must still match the live sequence).
    pub fn currval(&self, slot: usize, created_at: u64) -> Option<i64> {
        let (stamp, value) = self.currvals[slot].get();
        (stamp != 0 && stamp == created_at).then_some(value)
    }

    pub fn lastval(&self) -> Option<i64> {
        let (defined, value) = self.lastval.get();
        defined.then_some(value)
    }
}

#[derive(Clone, Copy)]
struct GucValues {
    /// Effective authorization identifier. Kept in the transactional GUC
    /// snapshot so SET ROLE follows PostgreSQL's rollback/savepoint behavior.
    current_role: StackStr<64>,
    /// Transactional session authorization identifier. SET SESSION
    /// AUTHORIZATION changes this and current_role together.
    session_authorization: StackStr<64>,
    datestyle: StackStr<48>,
    timezone: StackStr<64>,
    /// Parsed current time zone, so rendering does not re-parse it.
    parsed_timezone: super::timezone::Timezone,

    client_encoding: StackStr<32>,
    application_name: StackStr<64>,
    search_path: StackStr<128>,
    default_tablespace: StackStr<64>,
    client_min_messages: MessageLevel,
    extra_float_digits: StackStr<8>,
    lock_timeout: StackStr<24>,
    /// statement_timeout in milliseconds (0 = disabled), enforced at scan
    /// boundaries during execution.
    statement_timeout: StackStr<24>,
    row_security: StackStr<4>,
    /// bytea_output = escape (false = hex, the default).
    bytea_escape: bool,
    /// Whether function bodies are checked at definition time. Function DDL is
    /// rejected before this matters, but the session value is still observable
    /// and is emitted by pg_dump.
    check_function_bodies: bool,
}

impl GucValues {
    fn new() -> Self {
        let mut values = Self {
            current_role: StackStr::from_str("postgres"),
            session_authorization: StackStr::from_str("postgres"),
            datestyle: StackStr::new(),
            timezone: StackStr::new(),
            parsed_timezone: super::timezone::Timezone::utc(),
            client_encoding: StackStr::new(),
            application_name: StackStr::new(),
            search_path: StackStr::new(),
            default_tablespace: StackStr::new(),
            client_min_messages: MessageLevel::Notice,
            extra_float_digits: StackStr::new(),
            lock_timeout: StackStr::new(),
            statement_timeout: StackStr::new(),
            row_security: StackStr::new(),
            bytea_escape: false,
            check_function_bodies: true,
        };
        let _ = write!(values.datestyle, "ISO, MDY");
        let _ = write!(values.timezone, "UTC");
        let _ = write!(values.client_encoding, "UTF8");
        let _ = write!(values.search_path, "\"$user\", public");
        let _ = write!(values.extra_float_digits, "1");
        let _ = write!(values.lock_timeout, "0");
        let _ = write!(values.statement_timeout, "0");
        let _ = write!(values.row_security, "on");
        values
    }
}

#[derive(Clone, Copy)]
struct GucSavepoint {
    current: GucValues,
    session: GucValues,
}

struct GucTransaction {
    active: bool,
    start: GucValues,
    session: GucValues,
    savepoints: [Option<GucSavepoint>; super::txn::MAX_SAVEPOINTS],
    savepoint_count: usize,
}

struct GucStore {
    current: GucValues,
    defaults: GucValues,
    transaction: GucTransaction,
}

pub struct GucState {
    store: RefCell<GucStore>,
    /// Immutable authenticated identity from the startup packet. PostgreSQL
    /// uses this identity—not a later SET ROLE or SET SESSION
    /// AUTHORIZATION—as the authority for changing session authorization.
    authenticated_user: StackStr<64>,
    /// This connection's `currval`/`lastval` state.
    seq_session: SeqSession,
}

impl Default for GucState {
    fn default() -> Self {
        Self::new()
    }
}

impl GucState {
    pub fn new() -> Self {
        let values = GucValues::new();
        let mut g = Self {
            store: RefCell::new(GucStore {
                current: values,
                defaults: values,
                transaction: GucTransaction {
                    active: false,
                    start: values,
                    session: values,
                    savepoints: [None; super::txn::MAX_SAVEPOINTS],
                    savepoint_count: 0,
                },
            }),
            authenticated_user: StackStr::new(),
            seq_session: SeqSession::new(),
        };
        let _ = write!(g.authenticated_user, "postgres");
        g
    }

    pub fn search_path(&self) -> StackStr<128> {
        self.store.borrow().current.search_path
    }

    pub fn default_tablespace(&self) -> StackStr<64> {
        self.store.borrow().current.default_tablespace
    }

    pub fn seq_session(&self) -> &SeqSession {
        &self.seq_session
    }

    pub fn session_user(&self) -> StackStr<64> {
        self.store.borrow().current.session_authorization
    }

    pub fn authenticated_user(&self) -> &str {
        self.authenticated_user.as_str()
    }

    pub fn set_session_user(&mut self, user: &str) {
        self.authenticated_user = StackStr::new();
        let _ = core::fmt::Write::write_str(&mut self.authenticated_user, user);
        let mut store = self.store.borrow_mut();
        let role = StackStr::from_str(user);
        store.current.current_role = role;
        store.current.session_authorization = role;
        store.defaults.current_role = role;
        store.defaults.session_authorization = role;
        store.transaction.start.current_role = role;
        store.transaction.start.session_authorization = role;
        store.transaction.session.current_role = role;
        store.transaction.session.session_authorization = role;
    }

    pub fn current_role(&self) -> StackStr<64> {
        self.store.borrow().current.current_role
    }

    pub fn set_role(&self, role: &str, local: bool) {
        let mut store = self.store.borrow_mut();
        let role = StackStr::from_str(role);
        store.current.current_role = role;
        if store.transaction.active && !local {
            store.transaction.session.current_role = role;
        }
    }

    pub fn reset_role(&self, local: bool) {
        let session_user = self.store.borrow().current.session_authorization;
        self.set_role(session_user.as_str(), local);
    }

    pub fn set_session_authorization(&self, role: &str, local: bool) {
        let mut store = self.store.borrow_mut();
        let role = StackStr::from_str(role);
        store.current.session_authorization = role;
        store.current.current_role = role;
        if store.transaction.active && !local {
            store.transaction.session.session_authorization = role;
            store.transaction.session.current_role = role;
        }
    }

    pub fn reset_session_authorization(&self, local: bool) {
        let authenticated_user = self.authenticated_user;
        self.set_session_authorization(authenticated_user.as_str(), local);
    }

    pub fn statement_timeout_ms(&self) -> u64 {
        parse_timeout_ms(self.store.borrow().current.statement_timeout.as_str()).unwrap_or(0)
    }

    pub fn lock_timeout_ms(&self) -> u64 {
        parse_timeout_ms(self.store.borrow().current.lock_timeout.as_str()).unwrap_or(0)
    }

    /// Applies `SET name = raw`. `raw` is the raw source text of the value
    /// (surrounding single quotes and whitespace are stripped here). Returns an
    /// error for an unknown parameter, a read-only parameter, or a value whose
    /// behavior is not implemented.
    pub fn set(&self, name: &str, raw: &str, local: bool) -> Result<(), SqlError> {
        let mut state = self.store.borrow_mut();
        let mut values = state.current;
        change_setting(&mut values, &state.defaults, name, raw)?;
        state.current = values;
        if !state.transaction.active {
            // Before the first transaction this is a startup-packet setting;
            // PostgreSQL makes it the value RESET returns to.
            state.defaults = values;
        } else if !local {
            let mut session = state.transaction.session;
            change_setting(&mut session, &state.defaults, name, raw)?;
            state.transaction.session = session;
        }
        Ok(())
    }

    pub fn reset(&self, name: &str) -> Result<(), SqlError> {
        self.set(name, "DEFAULT", false)
    }

    pub fn reset_all(&self) {
        let mut state = self.store.borrow_mut();
        let values = state.defaults;
        state.current = values;
        if state.transaction.active {
            state.transaction.session = values;
        }
    }

    pub fn begin_transaction(&self) {
        let mut state = self.store.borrow_mut();
        if state.transaction.active {
            return;
        }
        let current = state.current;
        state.transaction.active = true;
        state.transaction.start = current;
        state.transaction.session = current;
        state.transaction.savepoint_count = 0;
    }

    pub fn commit_transaction(&self) {
        let mut state = self.store.borrow_mut();
        if !state.transaction.active {
            return;
        }
        state.current = state.transaction.session;
        state.transaction.active = false;
        state.transaction.savepoint_count = 0;
    }

    pub fn rollback_transaction(&self) {
        let mut state = self.store.borrow_mut();
        if !state.transaction.active {
            return;
        }
        state.current = state.transaction.start;
        state.transaction.active = false;
        state.transaction.savepoint_count = 0;
    }

    pub fn savepoint(&self) {
        let mut state = self.store.borrow_mut();
        if !state.transaction.active {
            return;
        }
        let index = state.transaction.savepoint_count;
        debug_assert!(index < super::txn::MAX_SAVEPOINTS);
        state.transaction.savepoints[index] = Some(GucSavepoint {
            current: state.current,
            session: state.transaction.session,
        });
        state.transaction.savepoint_count += 1;
    }

    pub fn release_savepoints_from(&self, index: usize) {
        let mut state = self.store.borrow_mut();
        if state.transaction.active {
            state.transaction.savepoint_count = index;
        }
    }

    pub fn rollback_to_savepoint(&self, index: usize) {
        let mut state = self.store.borrow_mut();
        if !state.transaction.active || index >= state.transaction.savepoint_count {
            return;
        }
        let savepoint =
            state.transaction.savepoints[index].expect("GUC savepoint mirrors transaction");
        state.current = savepoint.current;
        state.transaction.session = savepoint.session;
        state.transaction.savepoint_count = index + 1;
    }

    pub fn set_config(
        &self,
        name: &str,
        value: Option<&str>,
        local: bool,
    ) -> Result<StackStr<256>, SqlError> {
        self.set(name, value.unwrap_or("DEFAULT"), local)?;
        self.get_owned(name).ok_or_else(|| {
            sql_err!(
                sqlstate::UNDEFINED_OBJECT,
                "unrecognized configuration parameter \"{}\"",
                name
            )
        })
    }
}

fn change_setting(
    values: &mut GucValues,
    defaults: &GucValues,
    name: &str,
    raw: &str,
) -> Result<(), SqlError> {
    if unquote(raw).eq_ignore_ascii_case("default") {
        reset_setting(values, defaults, name)
    } else {
        apply_setting(values, name, raw)
    }
}

fn reset_setting(values: &mut GucValues, defaults: &GucValues, name: &str) -> Result<(), SqlError> {
    if name.eq_ignore_ascii_case("datestyle") {
        values.datestyle = defaults.datestyle;
    } else if name.eq_ignore_ascii_case("timezone") {
        values.timezone = defaults.timezone;
        values.parsed_timezone = defaults.parsed_timezone;
    } else if name.eq_ignore_ascii_case("client_encoding") {
        values.client_encoding = defaults.client_encoding;
    } else if name.eq_ignore_ascii_case("application_name") {
        values.application_name = defaults.application_name;
    } else if name.eq_ignore_ascii_case("search_path") {
        values.search_path = defaults.search_path;
    } else if name.eq_ignore_ascii_case("default_tablespace") {
        values.default_tablespace = defaults.default_tablespace;
    } else if name.eq_ignore_ascii_case("client_min_messages") {
        values.client_min_messages = defaults.client_min_messages;
    } else if name.eq_ignore_ascii_case("extra_float_digits") {
        values.extra_float_digits = defaults.extra_float_digits;
    } else if name.eq_ignore_ascii_case("lock_timeout") {
        values.lock_timeout = defaults.lock_timeout;
    } else if name.eq_ignore_ascii_case("statement_timeout") {
        values.statement_timeout = defaults.statement_timeout;
    } else if name.eq_ignore_ascii_case("row_security") {
        values.row_security = defaults.row_security;
    } else if name.eq_ignore_ascii_case("bytea_output") {
        values.bytea_escape = defaults.bytea_escape;
    } else if name.eq_ignore_ascii_case("check_function_bodies") {
        values.check_function_bodies = defaults.check_function_bodies;
    } else if name.eq_ignore_ascii_case("intervalstyle") {
        // Interval rendering is fixed to PostgreSQL's default style.
    } else if name.eq_ignore_ascii_case("synchronize_seqscans") {
        // Storage scans are deterministic and never synchronize their starts.
    } else if name.eq_ignore_ascii_case("standard_conforming_strings")
        || name.eq_ignore_ascii_case("xmloption")
        || name.eq_ignore_ascii_case("default_table_access_method")
        || name.eq_ignore_ascii_case("idle_in_transaction_session_timeout")
        || name.eq_ignore_ascii_case("transaction_timeout")
    {
        // Their only accepted value is already the fixed default.
    } else if is_read_only(name) {
        return Err(sql_err!(
            sqlstate::CANT_CHANGE_RUNTIME_PARAM,
            "parameter \"{}\" cannot be changed",
            name
        ));
    } else {
        return Err(sql_err!(
            sqlstate::UNDEFINED_OBJECT,
            "unrecognized configuration parameter \"{}\"",
            name
        ));
    }
    Ok(())
}

fn apply_setting(values: &mut GucValues, name: &str, raw: &str) -> Result<(), SqlError> {
    let v = unquote(raw);
    let is_default = v.eq_ignore_ascii_case("default");

    if name.eq_ignore_ascii_case("datestyle") {
        // DateStyle is cumulative: each SET updates only the components it
        // names, keeping the rest.
        let (fmt, ord) = if is_default {
            (DateFormat::Iso, Order3::Mdy)
        } else {
            let cur = parse_full(values.datestyle.as_str());
            apply_datestyle(cur, v).ok_or_else(|| unsupported_value("DateStyle", v))?
        };
        return store(
            &mut values.datestyle,
            canonical_datestyle(fmt, ord).as_str(),
        );
    }
    if name.eq_ignore_ascii_case("timezone") {
        // UTC, fixed numeric offsets, Etc/GMT±N, and named IANA zones (with
        // DST) are honored; an unknown zone is rejected loudly.
        let timezone = if is_default {
            super::timezone::Timezone::utc()
        } else {
            parse_timezone(v).ok_or_else(|| {
                sql_err!(
                    crate::sql::eval::sqlstate::INVALID_PARAMETER_VALUE,
                    "invalid value for parameter \"TimeZone\": \"{}\"",
                    v
                )
            })?
        };
        store(&mut values.timezone, if is_default { "UTC" } else { v })?;
        values.parsed_timezone = timezone;
        return Ok(());
    }
    if name.eq_ignore_ascii_case("client_encoding") {
        // UTF8 is native; SQL_ASCII is byte-pass-through (no conversion), so
        // both are served without transcoding. Any other encoding would
        // require a conversion we do not implement.
        if is_default || is_utf8(v) {
            return store(&mut values.client_encoding, "UTF8");
        }
        if v.eq_ignore_ascii_case("sql_ascii") {
            return store(&mut values.client_encoding, "SQL_ASCII");
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
    if name.eq_ignore_ascii_case("check_function_bodies") {
        values.check_function_bodies = if is_default {
            true
        } else {
            parse_on_off(v).ok_or_else(|| unsupported_value("check_function_bodies", v))?
        };
        return Ok(());
    }
    if name.eq_ignore_ascii_case("xmloption") {
        if is_default || v.eq_ignore_ascii_case("content") {
            return Ok(());
        }
        return Err(sql_err!(
            sqlstate::FEATURE_NOT_SUPPORTED,
            "xmloption can only be content (XML document validation is not supported)"
        ));
    }
    if name.eq_ignore_ascii_case("default_tablespace") {
        return store(
            &mut values.default_tablespace,
            if is_default { "" } else { v },
        );
    }
    if name.eq_ignore_ascii_case("default_table_access_method") {
        if is_default || v.eq_ignore_ascii_case("heap") {
            return Ok(());
        }
        return Err(sql_err!(
            sqlstate::FEATURE_NOT_SUPPORTED,
            "table access method \"{}\" is not supported",
            v
        ));
    }
    if name.eq_ignore_ascii_case("intervalstyle") {
        if is_default || v.eq_ignore_ascii_case("postgres") {
            return Ok(());
        }
        return Err(sql_err!(
            sqlstate::FEATURE_NOT_SUPPORTED,
            "IntervalStyle can only be postgres (other interval renderings are not supported)"
        ));
    }
    if name.eq_ignore_ascii_case("synchronize_seqscans") {
        if is_default || v.eq_ignore_ascii_case("off") {
            return Ok(());
        }
        return Err(sql_err!(
            sqlstate::FEATURE_NOT_SUPPORTED,
            "synchronize_seqscans can only be off (synchronized scans are not supported)"
        ));
    }
    if name.eq_ignore_ascii_case("client_min_messages") {
        // Filters which NOTICE/WARNING messages reach the client. The
        // default is `notice`; an unrecognized level errors like PostgreSQL.
        values.client_min_messages = if is_default {
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
        return store(
            &mut values.application_name,
            if is_default { "" } else { v },
        );
    }
    if name.eq_ignore_ascii_case("search_path") {
        if is_default {
            return store(&mut values.search_path, "\"$user\", public");
        }
        let mut canonical = StackStr::<128>::new();
        // The raw text, not the pre-unquoted value: quoting decides how
        // elements split, and a single-quoted string is one element
        // however many commas it holds.
        canonicalize_search_path(raw, &mut canonical)?;
        return store(&mut values.search_path, canonical.as_str());
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
        values.extra_float_digits.clear();
        let _ = write!(values.extra_float_digits, "{n}");
        return Ok(());
    }
    if name.eq_ignore_ascii_case("lock_timeout") {
        if is_default {
            return store(&mut values.lock_timeout, "0");
        }
        if parse_timeout_ms(v).is_none() {
            return Err(sql_err!(
                sqlstate::INVALID_PARAMETER_VALUE,
                "invalid value for parameter \"lock_timeout\": \"{}\"",
                v
            ));
        }
        return store(&mut values.lock_timeout, v);
    }
    if name.eq_ignore_ascii_case("statement_timeout") {
        // Enforced at scan boundaries during execution.
        if is_default {
            return store(&mut values.statement_timeout, "0");
        }
        if parse_timeout_ms(v).is_none() {
            return Err(sql_err!(
                sqlstate::INVALID_PARAMETER_VALUE,
                "invalid value for parameter \"statement_timeout\": \"{}\"",
                v
            ));
        }
        return store(&mut values.statement_timeout, v);
    }
    if name.eq_ignore_ascii_case("idle_in_transaction_session_timeout")
        || name.eq_ignore_ascii_case("transaction_timeout")
    {
        // These timeout mechanisms do not run yet, so only the disabled value
        // is honored; a non-zero value would be a silent no-operator.
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
            values.bytea_escape = false;
            return Ok(());
        }
        if v.eq_ignore_ascii_case("escape") {
            values.bytea_escape = true;
            return Ok(());
        }
        return Err(sql_err!(
            sqlstate::INVALID_PARAMETER_VALUE,
            "invalid value for parameter \"bytea_output\": \"{}\"",
            v
        ));
    }
    if name.eq_ignore_ascii_case("row_security") {
        let on = if is_default {
            true
        } else {
            parse_on_off(v).ok_or_else(|| unsupported_value("row_security", v))?
        };
        return store(&mut values.row_security, if on { "on" } else { "off" });
    }
    // Read-only parameters cannot be assigned.
    if is_read_only(name) {
        return Err(sql_err!(
            crate::sql::eval::sqlstate::CANT_CHANGE_RUNTIME_PARAM,
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

impl GucState {
    /// The current value for `SHOW name`, or None if the parameter is unknown
    /// here (the caller falls back to fixed server parameters).
    pub fn get_owned(&self, name: &str) -> Option<StackStr<256>> {
        let state = self.store.borrow();
        let values = &state.current;
        if name.eq_ignore_ascii_case("datestyle") {
            Some(StackStr::from_str(values.datestyle.as_str()))
        } else if name.eq_ignore_ascii_case("timezone") {
            Some(StackStr::from_str(values.timezone.as_str()))
        } else if name.eq_ignore_ascii_case("client_encoding") {
            Some(StackStr::from_str(values.client_encoding.as_str()))
        } else if name.eq_ignore_ascii_case("application_name") {
            Some(StackStr::from_str(values.application_name.as_str()))
        } else if name.eq_ignore_ascii_case("search_path") {
            Some(StackStr::from_str(values.search_path.as_str()))
        } else if name.eq_ignore_ascii_case("client_min_messages") {
            Some(StackStr::from_str(values.client_min_messages.as_str()))
        } else if name.eq_ignore_ascii_case("extra_float_digits") {
            Some(StackStr::from_str(values.extra_float_digits.as_str()))
        } else if name.eq_ignore_ascii_case("lock_timeout") {
            Some(StackStr::from_str(values.lock_timeout.as_str()))
        } else if name.eq_ignore_ascii_case("row_security") {
            Some(StackStr::from_str(values.row_security.as_str()))
        } else if name.eq_ignore_ascii_case("statement_timeout") {
            Some(StackStr::from_str(values.statement_timeout.as_str()))
        } else if name.eq_ignore_ascii_case("idle_in_transaction_session_timeout")
            || name.eq_ignore_ascii_case("transaction_timeout")
        {
            Some(StackStr::from_str("0"))
        } else if name.eq_ignore_ascii_case("bytea_output") {
            Some(StackStr::from_str(if values.bytea_escape {
                "escape"
            } else {
                "hex"
            }))
        } else if name.eq_ignore_ascii_case("check_function_bodies") {
            Some(StackStr::from_str(if values.check_function_bodies {
                "on"
            } else {
                "off"
            }))
        } else if name.eq_ignore_ascii_case("xmloption") {
            Some(StackStr::from_str("content"))
        } else if name.eq_ignore_ascii_case("default_tablespace") {
            Some(StackStr::from_str(values.default_tablespace.as_str()))
        } else if name.eq_ignore_ascii_case("default_table_access_method") {
            Some(StackStr::from_str("heap"))
        } else if name.eq_ignore_ascii_case("intervalstyle") {
            Some(StackStr::from_str("postgres"))
        } else if name.eq_ignore_ascii_case("synchronize_seqscans") {
            Some(StackStr::from_str("off"))
        } else {
            None
        }
    }

    /// Value-rendering settings for the wire layer (DateStyle + zone).
    /// The session's resolved `TimeZone`.
    pub fn timezone(&self) -> super::timezone::Timezone {
        self.store.borrow().current.parsed_timezone
    }

    pub fn render(&self) -> RenderContext {
        let state = self.store.borrow();
        let values = &state.current;
        let (format, ord) = parse_full(values.datestyle.as_str());
        let order = if ord == Order3::Dmy {
            FieldOrder::Dmy
        } else {
            FieldOrder::Mdy
        };
        RenderContext {
            datestyle: DateStyle { format, order },
            parsed_timezone: values.parsed_timezone,
            min_message_level: values.client_min_messages,
            bytea_escape: values.bytea_escape,
        }
    }
}

/// Parses a PostgreSQL boolean GUC value (on/off/true/false/1/0), allocation-free.
fn parse_on_off(v: &str) -> Option<bool> {
    if ["on", "true", "yes", "1"]
        .iter()
        .any(|s| v.eq_ignore_ascii_case(s))
    {
        Some(true)
    } else if ["off", "false", "no", "0"]
        .iter()
        .any(|s| v.eq_ignore_ascii_case(s))
    {
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
    num_part
        .trim()
        .parse::<u64>()
        .ok()
        .and_then(|n| n.checked_mul(mult))
}

fn store<const N: usize>(dst: &mut StackStr<N>, v: &str) -> Result<(), SqlError> {
    dst.clear();
    let _ = write!(dst, "{v}");
    if dst.is_truncated() {
        return Err(sql_err!(
            sqlstate::INVALID_PARAMETER_VALUE,
            "configuration value is too long"
        ));
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
        return Some(Timezone::fixed(
            offset,
            super::datetime::iso_offset_string(offset).as_str(),
        ));
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
    v.eq_ignore_ascii_case("utf8")
        || v.eq_ignore_ascii_case("utf-8")
        || v.eq_ignore_ascii_case("unicode")
}

/// Renders a `SET search_path` value the way PostgreSQL stores it for SHOW:
/// elements comma-space separated, bare identifiers case-folded to lower, and
/// an element quoted on output when it needs quoting (`"$user"`, mixed case,
/// spaces). Elements split on commas *outside* quotes: a single-quoted
/// string is one element however many commas it contains.
fn canonicalize_search_path(v: &str, out: &mut StackStr<128>) -> Result<(), SqlError> {
    use core::fmt::Write as _;
    let mut first = true;
    let mut rest = v.trim();
    while !rest.is_empty() {
        // Take one element: up to a comma not inside '...' or "...".
        let mut depth_single = false;
        let mut depth_double = false;
        let mut split = rest.len();
        for (i, c) in rest.char_indices() {
            match c {
                '\'' if !depth_double => depth_single = !depth_single,
                '"' if !depth_single => depth_double = !depth_double,
                ',' if !depth_single && !depth_double => {
                    split = i;
                    break;
                }
                _ => {}
            }
        }
        let element = rest[..split].trim();
        rest = rest.get(split + 1..).unwrap_or("").trim_start();
        if element.is_empty() {
            continue;
        }
        let mut name = StackStr::<64>::new();
        if element.starts_with('"') {
            let raw = element.trim_matches('"');
            let mut chars = raw.chars().peekable();
            while let Some(c) = chars.next() {
                let _ = write!(name, "{c}");
                if c == '"' {
                    chars.next();
                }
            }
        } else if element.starts_with('\'') {
            let raw = element.trim_matches('\'');
            let mut chars = raw.chars().peekable();
            while let Some(c) = chars.next() {
                let _ = write!(name, "{c}");
                if c == '\'' {
                    chars.next();
                }
            }
        } else {
            for c in element.chars() {
                let _ = write!(name, "{}", c.to_ascii_lowercase());
            }
        }
        let plain = name
            .as_str()
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
            && name
                .as_str()
                .bytes()
                .next()
                .is_some_and(|b| b.is_ascii_lowercase() || b == b'_');
        if !first {
            let _ = write!(out, ", ");
        }
        first = false;
        if plain {
            let _ = write!(out, "{}", name.as_str());
        } else {
            let _ = write!(out, "\"");
            for c in name.as_str().chars() {
                if c == '"' {
                    let _ = write!(out, "\"\"");
                } else {
                    let _ = write!(out, "{c}");
                }
            }
            let _ = write!(out, "\"");
        }
        if out.is_truncated() {
            return Err(sql_err!(
                sqlstate::INVALID_PARAMETER_VALUE,
                "search_path is too long (limit 128 bytes)"
            ));
        }
    }
    Ok(())
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
