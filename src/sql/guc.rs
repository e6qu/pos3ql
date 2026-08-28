//! Per-session configuration parameters (GUCs). `SET` writes them and `SHOW`
//! reads them. A value we cannot honor is rejected loudly, so a client never
//! receives false success for an ignored setting.

use crate::sql::eval::sqlstate;
use core::cell::{Cell, RefCell};
use core::fmt::Write;

use crate::sql_err;
use crate::storage::MAX_SEQUENCES;
use crate::util::StackStr;

use super::ast::{TransactionCharacteristics, TransactionIsolation};
use super::datetime::{DateFormat, DateStyle, FieldOrder, IntervalStyle};
use super::eval::SqlError;

#[derive(Clone, Copy)]
struct PrngState {
    s0: u64,
    s1: u64,
    initialized: bool,
}

impl PrngState {
    const UNINITIALIZED: Self = Self {
        s0: 0,
        s1: 0,
        initialized: false,
    };

    fn splitmix64(seed: &mut u64) -> u64 {
        *seed = seed.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut value = *seed;
        value = (value ^ (value >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        value = (value ^ (value >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        value ^ (value >> 31)
    }

    fn seed_u64(&mut self, mut seed: u64) {
        self.s0 = Self::splitmix64(&mut seed);
        self.s1 = Self::splitmix64(&mut seed);
        if self.s0 == 0 && self.s1 == 0 {
            self.s0 = 0x5851_F42D_4C95_7F2D;
            self.s1 = 0x1405_7B7E_F767_814F;
        }
        self.initialized = true;
    }

    fn seed_f64(&mut self, seed: f64) {
        let integer = (((1_u64 << 52) - 1) as f64 * seed) as i64;
        self.seed_u64(integer as u64);
    }

    fn next_u64(&mut self) -> u64 {
        let s0 = self.s0;
        let mixed = self.s1 ^ s0;
        let value = s0.wrapping_mul(5).rotate_left(7).wrapping_mul(9);
        self.s0 = s0.rotate_left(24) ^ mixed ^ mixed.wrapping_shl(16);
        self.s1 = mixed.rotate_left(37);
        value
    }

    fn next_f64(&mut self) -> f64 {
        ((self.next_u64() >> 12) as f64) * 2_f64.powi(-52)
    }
}

std::thread_local! {
    static ACTIVE_GUC: Cell<*const GucState> = const { Cell::new(core::ptr::null()) };
    static ACTIVE_TXN: Cell<*mut super::txn::TxnState> = const { Cell::new(core::ptr::null_mut()) };
    static ACTIVE_RENDER: RefCell<Option<RenderContext>> = const { RefCell::new(None) };
}

/// Keeps expression-time setting mutations scoped to the connection whose
/// statement is executing. The engine is single-threaded, and the GUC payload
/// itself is interior-mutable; the guard prevents a pointer from surviving the
/// statement on either success or error.
pub struct EvalScope {
    prior: *const GucState,
    prior_txn: *mut super::txn::TxnState,
    prior_render: Option<RenderContext>,
}

impl Drop for EvalScope {
    fn drop(&mut self) {
        ACTIVE_GUC.with(|active| active.set(self.prior));
        ACTIVE_TXN.with(|active| active.set(self.prior_txn));
        ACTIVE_RENDER.with(|active| *active.borrow_mut() = self.prior_render);
    }
}

pub fn enter_eval_scope(guc: &GucState, txn: &mut super::txn::TxnState) -> EvalScope {
    let pointer = guc as *const GucState;
    let prior = ACTIVE_GUC.with(|active| active.replace(pointer));
    let prior_txn = ACTIVE_TXN.with(|active| active.replace(txn as *mut _));
    let prior_render = ACTIVE_RENDER.with(|active| active.replace(Some(guc.render())));
    EvalScope {
        prior,
        prior_txn,
        prior_render,
    }
}

pub fn active_render() -> Option<RenderContext> {
    ACTIVE_RENDER.with(|active| *active.borrow())
}

pub(crate) fn active_random() -> Result<f64, SqlError> {
    ACTIVE_GUC.with(|active| {
        let pointer = active.get();
        if pointer.is_null() {
            return Err(sql_err!(
                sqlstate::FEATURE_NOT_SUPPORTED,
                "random is unavailable outside statement execution"
            ));
        }
        // SAFETY: EvalScope owns the pointer's dynamic extent.
        unsafe { &*pointer }.random()
    })
}

pub(crate) fn set_active_random_seed(seed: f64) -> Result<(), SqlError> {
    ACTIVE_GUC.with(|active| {
        let pointer = active.get();
        if pointer.is_null() {
            return Err(sql_err!(
                sqlstate::FEATURE_NOT_SUPPORTED,
                "setseed is unavailable outside statement execution"
            ));
        }
        // SAFETY: EvalScope owns the pointer's dynamic extent.
        unsafe { &*pointer }.set_random_seed(seed)
    })
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
        if let Some(characteristics) =
            guc.current_transaction_setting(name, value.unwrap_or("DEFAULT"))
        {
            let characteristics = characteristics?;
            let txn_pointer = ACTIVE_TXN.with(Cell::get);
            if txn_pointer.is_null() {
                return Err(sql_err!(
                    sqlstate::FEATURE_NOT_SUPPORTED,
                    "transaction configuration is unavailable outside statement execution"
                ));
            }
            // SAFETY: EvalScope installs the live transaction beside the GUC
            // and clears both pointers on every exit.
            let txn = unsafe { &mut *txn_pointer };
            txn.apply_characteristics(characteristics)?;
            let result =
                transaction_setting_owned(txn, name).expect("recognized transaction setting");
            let reset = guc
                .transaction_reset_owned(name)
                .expect("recognized transaction setting has a default");
            crate::sql::eval::funcs::system::update_session_setting(name, result, reset, "session");
            return Ok(result);
        }
        let result = guc.set_config(name, value, local)?;
        publish_active_setting(guc, name);
        Ok(result)
    })
}

pub(crate) fn publish_active_setting(guc: &GucState, name: &str) {
    let txn_pointer = ACTIVE_TXN.with(Cell::get);
    if !txn_pointer.is_null() {
        // SAFETY: EvalScope owns the pointer's dynamic extent.
        let txn = unsafe { &*txn_pointer };
        if let Some(value) = transaction_setting_owned(txn, name) {
            let reset = guc.transaction_reset_owned(name).unwrap_or(value);
            crate::sql::eval::funcs::system::update_session_setting(name, value, reset, "session");
            return;
        }
    }
    if let Some(value) = guc.get_owned(name) {
        let reset = guc.reset_owned(name).unwrap_or(value);
        crate::sql::eval::funcs::system::update_session_setting(
            name,
            value,
            reset,
            guc.source(name),
        );
    }
    ACTIVE_RENDER.with(|active| *active.borrow_mut() = Some(guc.render()));
    if name.eq_ignore_ascii_case("timezone") {
        crate::sql::timezone::set_session(guc.timezone());
    }
}

fn transaction_setting_owned(txn: &super::txn::TxnState, name: &str) -> Option<StackStr<256>> {
    if name.eq_ignore_ascii_case("transaction_isolation") {
        Some(StackStr::from_str(txn.isolation.as_str()))
    } else if name.eq_ignore_ascii_case("transaction_read_only") {
        Some(StackStr::from_str(if txn.read_only { "on" } else { "off" }))
    } else if name.eq_ignore_ascii_case("transaction_deferrable") {
        Some(StackStr::from_str(if txn.deferrable {
            "on"
        } else {
            "off"
        }))
    } else {
        None
    }
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
    pub intervalstyle: IntervalStyle,
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
            intervalstyle: IntervalStyle::Postgres,
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

    pub fn discard(&self) {
        for value in &self.currvals {
            value.set((0, 0));
        }
        self.lastval.set((false, 0));
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
    intervalstyle: IntervalStyle,
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
    default_transaction_isolation: TransactionIsolation,
    default_transaction_read_only: bool,
    default_transaction_deferrable: bool,
}

impl GucValues {
    fn new() -> Self {
        let mut values = Self {
            current_role: StackStr::from_str("postgres"),
            session_authorization: StackStr::from_str("postgres"),
            datestyle: StackStr::new(),
            intervalstyle: IntervalStyle::Postgres,
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
            default_transaction_isolation: TransactionIsolation::ReadCommitted,
            default_transaction_read_only: false,
            default_transaction_deferrable: false,
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

const GUC_DATESTYLE: u32 = 1 << 0;
const GUC_INTERVALSTYLE: u32 = 1 << 1;
const GUC_TIMEZONE: u32 = 1 << 2;
const GUC_CLIENT_ENCODING: u32 = 1 << 3;
const GUC_APPLICATION_NAME: u32 = 1 << 4;
const GUC_SEARCH_PATH: u32 = 1 << 5;
const GUC_DEFAULT_TABLESPACE: u32 = 1 << 6;
const GUC_CLIENT_MIN_MESSAGES: u32 = 1 << 7;
const GUC_EXTRA_FLOAT_DIGITS: u32 = 1 << 8;
const GUC_LOCK_TIMEOUT: u32 = 1 << 9;
const GUC_STATEMENT_TIMEOUT: u32 = 1 << 10;
const GUC_ROW_SECURITY: u32 = 1 << 11;
const GUC_BYTEA_OUTPUT: u32 = 1 << 12;
const GUC_CHECK_FUNCTION_BODIES: u32 = 1 << 13;
const GUC_DEFAULT_TRANSACTION_ISOLATION: u32 = 1 << 14;
const GUC_DEFAULT_TRANSACTION_READ_ONLY: u32 = 1 << 15;
const GUC_DEFAULT_TRANSACTION_DEFERRABLE: u32 = 1 << 16;
const GUC_STANDARD_CONFORMING_STRINGS: u32 = 1 << 17;
const GUC_XML_OPTION: u32 = 1 << 18;
const GUC_DEFAULT_TABLE_ACCESS_METHOD: u32 = 1 << 19;
const GUC_SYNCHRONIZE_SEQSCANS: u32 = 1 << 20;
const GUC_IDLE_IN_TRANSACTION_SESSION_TIMEOUT: u32 = 1 << 21;
const GUC_TRANSACTION_TIMEOUT: u32 = 1 << 22;
const GUC_ALL: u32 = (1 << 23) - 1;

fn guc_bit(name: &str) -> u32 {
    if name.eq_ignore_ascii_case("datestyle") {
        GUC_DATESTYLE
    } else if name.eq_ignore_ascii_case("intervalstyle") {
        GUC_INTERVALSTYLE
    } else if name.eq_ignore_ascii_case("timezone") {
        GUC_TIMEZONE
    } else if name.eq_ignore_ascii_case("client_encoding") {
        GUC_CLIENT_ENCODING
    } else if name.eq_ignore_ascii_case("application_name") {
        GUC_APPLICATION_NAME
    } else if name.eq_ignore_ascii_case("search_path") {
        GUC_SEARCH_PATH
    } else if name.eq_ignore_ascii_case("default_tablespace") {
        GUC_DEFAULT_TABLESPACE
    } else if name.eq_ignore_ascii_case("client_min_messages") {
        GUC_CLIENT_MIN_MESSAGES
    } else if name.eq_ignore_ascii_case("extra_float_digits") {
        GUC_EXTRA_FLOAT_DIGITS
    } else if name.eq_ignore_ascii_case("lock_timeout") {
        GUC_LOCK_TIMEOUT
    } else if name.eq_ignore_ascii_case("statement_timeout") {
        GUC_STATEMENT_TIMEOUT
    } else if name.eq_ignore_ascii_case("row_security") {
        GUC_ROW_SECURITY
    } else if name.eq_ignore_ascii_case("bytea_output") {
        GUC_BYTEA_OUTPUT
    } else if name.eq_ignore_ascii_case("check_function_bodies") {
        GUC_CHECK_FUNCTION_BODIES
    } else if name.eq_ignore_ascii_case("default_transaction_isolation") {
        GUC_DEFAULT_TRANSACTION_ISOLATION
    } else if name.eq_ignore_ascii_case("default_transaction_read_only") {
        GUC_DEFAULT_TRANSACTION_READ_ONLY
    } else if name.eq_ignore_ascii_case("default_transaction_deferrable") {
        GUC_DEFAULT_TRANSACTION_DEFERRABLE
    } else if name.eq_ignore_ascii_case("standard_conforming_strings") {
        GUC_STANDARD_CONFORMING_STRINGS
    } else if name.eq_ignore_ascii_case("xmloption") {
        GUC_XML_OPTION
    } else if name.eq_ignore_ascii_case("default_table_access_method") {
        GUC_DEFAULT_TABLE_ACCESS_METHOD
    } else if name.eq_ignore_ascii_case("synchronize_seqscans") {
        GUC_SYNCHRONIZE_SEQSCANS
    } else if name.eq_ignore_ascii_case("idle_in_transaction_session_timeout") {
        GUC_IDLE_IN_TRANSACTION_SESSION_TIMEOUT
    } else if name.eq_ignore_ascii_case("transaction_timeout") {
        GUC_TRANSACTION_TIMEOUT
    } else {
        0
    }
}

fn copy_guc_values(target: &mut GucValues, source: &GucValues, mask: u32) {
    macro_rules! copy {
        ($bit:ident, $field:ident) => {
            if mask & $bit != 0 {
                target.$field = source.$field;
            }
        };
    }
    copy!(GUC_DATESTYLE, datestyle);
    copy!(GUC_INTERVALSTYLE, intervalstyle);
    if mask & GUC_TIMEZONE != 0 {
        target.timezone = source.timezone;
        target.parsed_timezone = source.parsed_timezone;
    }
    copy!(GUC_CLIENT_ENCODING, client_encoding);
    copy!(GUC_APPLICATION_NAME, application_name);
    copy!(GUC_SEARCH_PATH, search_path);
    copy!(GUC_DEFAULT_TABLESPACE, default_tablespace);
    copy!(GUC_CLIENT_MIN_MESSAGES, client_min_messages);
    copy!(GUC_EXTRA_FLOAT_DIGITS, extra_float_digits);
    copy!(GUC_LOCK_TIMEOUT, lock_timeout);
    copy!(GUC_STATEMENT_TIMEOUT, statement_timeout);
    copy!(GUC_ROW_SECURITY, row_security);
    copy!(GUC_BYTEA_OUTPUT, bytea_escape);
    copy!(GUC_CHECK_FUNCTION_BODIES, check_function_bodies);
    copy!(
        GUC_DEFAULT_TRANSACTION_ISOLATION,
        default_transaction_isolation
    );
    copy!(
        GUC_DEFAULT_TRANSACTION_READ_ONLY,
        default_transaction_read_only
    );
    copy!(
        GUC_DEFAULT_TRANSACTION_DEFERRABLE,
        default_transaction_deferrable
    );
}

fn finish_setting_change(
    state: &mut GucStore,
    values: GucValues,
    bit: u32,
    local: bool,
    resetting: bool,
) {
    state.current = values;
    if !state.transaction.active {
        state.defaults = values;
        state.connection_overrides |= bit;
        state.database_overrides &= !bit;
        state.role_overrides &= !bit;
        state.database_role_overrides &= !bit;
        state.client_overrides |= bit;
    } else if !local {
        copy_guc_values(&mut state.transaction.session, &values, bit);
        if resetting {
            state.session_overrides &= !bit;
            state.transaction.session_overrides &= !bit;
        } else {
            state.session_overrides |= bit;
            state.transaction.session_overrides |= bit;
        }
    }
}

#[derive(Clone, Copy)]
struct GucSavepoint {
    current: GucValues,
    session: GucValues,
    current_overrides: u32,
    session_overrides: u32,
}

struct GucTransaction {
    active: bool,
    start: GucValues,
    session: GucValues,
    start_overrides: u32,
    session_overrides: u32,
    savepoints: [Option<GucSavepoint>; super::txn::MAX_SAVEPOINTS],
    savepoint_count: usize,
}

struct GucStore {
    current: GucValues,
    defaults: GucValues,
    cluster_defaults: GucValues,
    cluster_overrides: u32,
    connection_overrides: u32,
    database_overrides: u32,
    role_overrides: u32,
    database_role_overrides: u32,
    client_overrides: u32,
    session_overrides: u32,
    transaction: GucTransaction,
}

#[derive(Clone, Copy)]
pub(crate) enum ConnectionDefaultSource {
    Database,
    Role,
    DatabaseRole,
}

pub struct GucState {
    store: RefCell<GucStore>,
    /// Immutable authenticated identity from the startup packet. PostgreSQL
    /// uses this identity—not a later SET ROLE or SET SESSION
    /// AUTHORIZATION—as the authority for changing session authorization.
    authenticated_user: StackStr<64>,
    /// This connection's `currval`/`lastval` state.
    seq_session: SeqSession,
    random: Cell<PrngState>,
}

pub(crate) struct RoutineConfigScope {
    guc: *const GucState,
    prior: GucValues,
    session: GucValues,
    names: [crate::storage::SqlName; crate::storage::MAX_ROUTINE_CONFIGS],
    count: usize,
}

impl Drop for RoutineConfigScope {
    fn drop(&mut self) {
        // SAFETY: routine scopes are created only beneath the statement's
        // EvalScope and are dropped before that scope releases the GucState.
        let guc = unsafe { &*self.guc };
        let mut state = guc.store.borrow_mut();
        let changed = state.transaction.session;
        let mut restored = self.prior;
        merge_session_changes(&mut restored, &self.session, &changed);
        state.current = restored;
        drop(state);
        for name in &self.names[..self.count] {
            if let Some(value) = guc.get_owned(name.as_str()) {
                let reset = guc.reset_owned(name.as_str()).unwrap_or(value);
                crate::sql::eval::funcs::system::update_session_setting(
                    name.as_str(),
                    value,
                    reset,
                    guc.source(name.as_str()),
                );
            }
        }
        ACTIVE_RENDER.with(|active| *active.borrow_mut() = Some(guc.render()));
        crate::sql::timezone::set_session(guc.timezone());
    }
}

fn merge_session_changes(target: &mut GucValues, before: &GucValues, after: &GucValues) {
    macro_rules! changed {
        ($field:ident) => {
            if before.$field != after.$field {
                target.$field = after.$field;
            }
        };
    }
    changed!(current_role);
    changed!(session_authorization);
    changed!(datestyle);
    changed!(intervalstyle);
    if before.timezone != after.timezone {
        target.timezone = after.timezone;
        target.parsed_timezone = after.parsed_timezone;
    }
    changed!(client_encoding);
    changed!(application_name);
    changed!(search_path);
    changed!(default_tablespace);
    changed!(client_min_messages);
    changed!(extra_float_digits);
    changed!(lock_timeout);
    changed!(statement_timeout);
    changed!(row_security);
    changed!(bytea_escape);
    changed!(check_function_bodies);
    changed!(default_transaction_isolation);
    changed!(default_transaction_read_only);
    changed!(default_transaction_deferrable);
}

impl Default for GucState {
    fn default() -> Self {
        Self::new()
    }
}

impl GucState {
    pub(crate) fn source(&self, name: &str) -> &'static str {
        let state = self.store.borrow();
        let bit = guc_bit(name);
        if state.session_overrides & bit != 0 {
            "session"
        } else if state.client_overrides & bit != 0 {
            "client"
        } else if state.database_role_overrides & bit != 0 {
            "database user"
        } else if state.role_overrides & bit != 0 {
            "user"
        } else if state.database_overrides & bit != 0 {
            "database"
        } else if state.cluster_overrides & bit != 0 {
            "configuration file"
        } else {
            "default"
        }
    }

    pub(crate) fn reset_owned(&self, name: &str) -> Option<StackStr<256>> {
        let state = self.store.borrow();
        let values = &state.defaults;
        if name.eq_ignore_ascii_case("datestyle") {
            Some(StackStr::from_str(values.datestyle.as_str()))
        } else if name.eq_ignore_ascii_case("intervalstyle") {
            Some(StackStr::from_str(values.intervalstyle.as_str()))
        } else if name.eq_ignore_ascii_case("timezone") {
            Some(StackStr::from_str(values.timezone.as_str()))
        } else if name.eq_ignore_ascii_case("client_encoding") {
            Some(StackStr::from_str(values.client_encoding.as_str()))
        } else if name.eq_ignore_ascii_case("application_name") {
            Some(StackStr::from_str(values.application_name.as_str()))
        } else if name.eq_ignore_ascii_case("search_path") {
            Some(StackStr::from_str(values.search_path.as_str()))
        } else if name.eq_ignore_ascii_case("default_tablespace") {
            Some(StackStr::from_str(values.default_tablespace.as_str()))
        } else if name.eq_ignore_ascii_case("client_min_messages") {
            Some(StackStr::from_str(values.client_min_messages.as_str()))
        } else if name.eq_ignore_ascii_case("extra_float_digits") {
            Some(StackStr::from_str(values.extra_float_digits.as_str()))
        } else if name.eq_ignore_ascii_case("lock_timeout") {
            Some(StackStr::from_str(values.lock_timeout.as_str()))
        } else if name.eq_ignore_ascii_case("statement_timeout") {
            Some(StackStr::from_str(values.statement_timeout.as_str()))
        } else if name.eq_ignore_ascii_case("row_security") {
            Some(StackStr::from_str(values.row_security.as_str()))
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
        } else if name.eq_ignore_ascii_case("default_transaction_isolation") {
            Some(StackStr::from_str(
                values.default_transaction_isolation.as_str(),
            ))
        } else if name.eq_ignore_ascii_case("default_transaction_read_only") {
            Some(StackStr::from_str(
                if values.default_transaction_read_only {
                    "on"
                } else {
                    "off"
                },
            ))
        } else if name.eq_ignore_ascii_case("default_transaction_deferrable") {
            Some(StackStr::from_str(
                if values.default_transaction_deferrable {
                    "on"
                } else {
                    "off"
                },
            ))
        } else {
            None
        }
    }

    pub fn new() -> Self {
        let values = GucValues::new();
        let mut g = Self {
            store: RefCell::new(GucStore {
                current: values,
                defaults: values,
                cluster_defaults: values,
                cluster_overrides: 0,
                connection_overrides: 0,
                database_overrides: 0,
                role_overrides: 0,
                database_role_overrides: 0,
                client_overrides: 0,
                session_overrides: 0,
                transaction: GucTransaction {
                    active: false,
                    start: values,
                    session: values,
                    start_overrides: 0,
                    session_overrides: 0,
                    savepoints: [None; super::txn::MAX_SAVEPOINTS],
                    savepoint_count: 0,
                },
            }),
            authenticated_user: StackStr::new(),
            seq_session: SeqSession::new(),
            random: Cell::new(PrngState::UNINITIALIZED),
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

    pub(crate) fn set_random_seed(&self, seed: f64) -> Result<(), SqlError> {
        if !(-1.0..=1.0).contains(&seed) || seed.is_nan() {
            return Err(sql_err!(
                sqlstate::INVALID_PARAMETER_VALUE,
                "setseed parameter {} is out of allowed range [-1,1]",
                seed
            ));
        }
        let mut state = self.random.get();
        state.seed_f64(seed);
        self.random.set(state);
        Ok(())
    }

    pub(crate) fn random(&self) -> Result<f64, SqlError> {
        let mut state = self.random.get();
        if !state.initialized {
            let mut seed = [0_u8; 16];
            if unsafe { libc::getentropy(seed.as_mut_ptr().cast(), seed.len()) } != 0 {
                return Err(sql_err!(
                    sqlstate::INTERNAL_ERROR,
                    "could not initialize the session random generator"
                ));
            }
            state.s0 = u64::from_ne_bytes(seed[..8].try_into().expect("fixed seed half"));
            state.s1 = u64::from_ne_bytes(seed[8..].try_into().expect("fixed seed half"));
            if state.s0 == 0 && state.s1 == 0 {
                state.seed_u64(0);
            } else {
                state.initialized = true;
            }
        }
        let value = state.next_f64();
        self.random.set(state);
        Ok(value)
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

    pub(crate) fn transaction_defaults(&self) -> (TransactionIsolation, bool, bool) {
        let values = self.store.borrow().current;
        (
            values.default_transaction_isolation,
            values.default_transaction_read_only,
            values.default_transaction_deferrable,
        )
    }

    pub(crate) fn set_transaction_defaults(
        &self,
        characteristics: TransactionCharacteristics,
    ) -> Result<(), SqlError> {
        if let Some(isolation) = characteristics.isolation {
            self.set("default_transaction_isolation", isolation.as_str(), false)?;
        }
        if let Some(read_only) = characteristics.read_only {
            self.set(
                "default_transaction_read_only",
                if read_only { "on" } else { "off" },
                false,
            )?;
        }
        if let Some(deferrable) = characteristics.deferrable {
            self.set(
                "default_transaction_deferrable",
                if deferrable { "on" } else { "off" },
                false,
            )?;
        }
        Ok(())
    }

    pub(crate) fn current_transaction_setting(
        &self,
        name: &str,
        raw: &str,
    ) -> Option<Result<TransactionCharacteristics, SqlError>> {
        let value = unquote(raw);
        let mut characteristics = TransactionCharacteristics::EMPTY;
        let cannot_reset = || {
            sql_err!(
                sqlstate::FEATURE_NOT_SUPPORTED,
                "parameter \"{}\" cannot be reset",
                name
            )
        };
        if name.eq_ignore_ascii_case("transaction_isolation") {
            if value.eq_ignore_ascii_case("default") {
                return Some(Err(cannot_reset()));
            }
            let isolation = match parse_transaction_isolation(value) {
                Some(isolation) => isolation,
                None => {
                    return Some(Err(sql_err!(
                        sqlstate::INVALID_PARAMETER_VALUE,
                        "invalid value for parameter \"transaction_isolation\": \"{}\"",
                        value
                    )));
                }
            };
            characteristics.isolation = Some(isolation);
        } else if name.eq_ignore_ascii_case("transaction_read_only") {
            if value.eq_ignore_ascii_case("default") {
                return Some(Err(cannot_reset()));
            }
            let read_only = match parse_on_off(value) {
                Some(read_only) => read_only,
                None => return Some(Err(unsupported_value("transaction_read_only", value))),
            };
            characteristics.read_only = Some(read_only);
        } else if name.eq_ignore_ascii_case("transaction_deferrable") {
            if value.eq_ignore_ascii_case("default") {
                return Some(Err(cannot_reset()));
            }
            let deferrable = match parse_on_off(value) {
                Some(deferrable) => deferrable,
                None => return Some(Err(unsupported_value("transaction_deferrable", value))),
            };
            characteristics.deferrable = Some(deferrable);
        } else {
            return None;
        }
        Some(Ok(characteristics))
    }

    pub(crate) fn current_transaction_setting_from_current(
        &self,
        name: &str,
        isolation: TransactionIsolation,
        read_only: bool,
        deferrable: bool,
    ) -> Option<Result<TransactionCharacteristics, SqlError>> {
        let mut characteristics = TransactionCharacteristics::EMPTY;
        if name.eq_ignore_ascii_case("transaction_isolation") {
            characteristics.isolation = Some(isolation);
        } else if name.eq_ignore_ascii_case("transaction_read_only") {
            characteristics.read_only = Some(read_only);
        } else if name.eq_ignore_ascii_case("transaction_deferrable") {
            characteristics.deferrable = Some(deferrable);
        } else {
            return None;
        }
        Some(Ok(characteristics))
    }

    pub(crate) fn set_from_current(&self, name: &str, local: bool) -> Result<(), SqlError> {
        if name.eq_ignore_ascii_case("seed") {
            return self.set("seed", "unavailable", local);
        }
        let mut state = self.store.borrow_mut();
        let values = state.current;
        let mut validation = values;
        reset_setting(&mut validation, &values, name)?;
        finish_setting_change(&mut state, values, guc_bit(name), local, false);
        Ok(())
    }

    pub(crate) fn transaction_reset_owned(&self, name: &str) -> Option<StackStr<256>> {
        let values = self.store.borrow().defaults;
        let isolation = values.default_transaction_isolation;
        let read_only = values.default_transaction_read_only;
        let deferrable = values.default_transaction_deferrable;
        if name.eq_ignore_ascii_case("transaction_isolation") {
            Some(StackStr::from_str(isolation.as_str()))
        } else if name.eq_ignore_ascii_case("transaction_read_only") {
            Some(StackStr::from_str(if read_only { "on" } else { "off" }))
        } else if name.eq_ignore_ascii_case("transaction_deferrable") {
            Some(StackStr::from_str(if deferrable { "on" } else { "off" }))
        } else {
            None
        }
    }

    /// Applies `SET name = raw`. `raw` is the raw source text of the value
    /// (surrounding single quotes and whitespace are stripped here). Returns an
    /// error for an unknown parameter, a read-only parameter, or a value the
    /// engine cannot honor.
    pub fn set(&self, name: &str, raw: &str, local: bool) -> Result<(), SqlError> {
        if name.eq_ignore_ascii_case("seed") {
            let value = unquote(raw);
            if value.eq_ignore_ascii_case("default") {
                return Err(sql_err!(
                    sqlstate::FEATURE_NOT_SUPPORTED,
                    "parameter \"seed\" cannot be reset"
                ));
            }
            let seed = value.parse::<f64>().map_err(|_| {
                sql_err!(
                    sqlstate::INVALID_PARAMETER_VALUE,
                    "invalid value for parameter \"seed\": \"{}\"",
                    value
                )
            })?;
            return self.set_random_seed(seed);
        }
        let mut state = self.store.borrow_mut();
        let mut values = state.current;
        change_setting(&mut values, &state.defaults, name, raw)?;
        let bit = guc_bit(name);
        let resetting = unquote(raw).eq_ignore_ascii_case("default");
        finish_setting_change(&mut state, values, bit, local, resetting);
        Ok(())
    }

    pub(crate) fn set_time_zone_sql(&self, raw: &str, local: bool) -> Result<(), SqlError> {
        let value = unquote(raw);
        if value.eq_ignore_ascii_case("local") || value.eq_ignore_ascii_case("default") {
            return self.set("timezone", "DEFAULT", local);
        }
        let text = raw.trim();
        let numeric = text.strip_prefix(['+', '-']).unwrap_or(text);
        if numeric.is_empty()
            || !numeric
                .bytes()
                .all(|byte| byte.is_ascii_digit() || byte == b'.')
            || numeric.bytes().filter(|byte| *byte == b'.').count() > 1
        {
            return self.set("timezone", raw, local);
        }
        let hours = text
            .parse::<f64>()
            .map_err(|_| unsupported_value("TimeZone", value))?;
        let seconds = hours * 3600.0;
        if !seconds.is_finite() || seconds.fract() != 0.0 {
            return Err(unsupported_value("TimeZone", value));
        }
        self.set_time_zone_offset(seconds as i32, local)
    }

    pub(crate) fn set_time_zone_interval(
        &self,
        interval: super::types::Interval,
        local: bool,
    ) -> Result<(), SqlError> {
        if interval.months != 0 {
            return Err(unsupported_value("TimeZone", "interval containing months"));
        }
        let micros = i64::from(interval.days)
            .checked_mul(86_400_000_000)
            .and_then(|days| days.checked_add(interval.micros))
            .ok_or_else(|| unsupported_value("TimeZone", "interval"))?;
        if micros % 1_000_000 != 0 {
            return Err(unsupported_value("TimeZone", "fractional-second interval"));
        }
        let seconds = i32::try_from(micros / 1_000_000)
            .map_err(|_| unsupported_value("TimeZone", "interval"))?;
        self.set_time_zone_offset(seconds, local)
    }

    fn set_time_zone_offset(&self, seconds_east: i32, local: bool) -> Result<(), SqlError> {
        if !(-57_599..=57_599).contains(&seconds_east) {
            return Err(unsupported_value("TimeZone", "UTC offset"));
        }
        let east = super::datetime::iso_offset_string(seconds_east);
        let west = super::datetime::iso_offset_string(-seconds_east);
        let display = crate::stack_format!(32, "<{}>{}", east.as_str(), west.as_str());
        let mut state = self.store.borrow_mut();
        let mut values = state.current;
        store(&mut values.timezone, display.as_str())?;
        values.parsed_timezone = super::timezone::Timezone::fixed(seconds_east, "");
        finish_setting_change(&mut state, values, GUC_TIMEZONE, local, false);
        Ok(())
    }

    pub(crate) fn set_cluster_default(&self, name: &str, raw: &str) -> Result<(), SqlError> {
        let mut state = self.store.borrow_mut();
        let mut cluster = state.cluster_defaults;
        change_setting(&mut cluster, &GucValues::new(), name, raw)?;
        state.cluster_defaults = cluster;
        state.cluster_overrides |= guc_bit(name);
        Self::recompute_cluster_defaults(&mut state);
        Ok(())
    }
    pub(crate) fn reset_cluster_defaults(&self) {
        let mut state = self.store.borrow_mut();
        state.cluster_defaults = GucValues::new();
        state.cluster_overrides = 0;
        Self::recompute_cluster_defaults(&mut state);
    }

    pub(crate) fn set_connection_default(
        &self,
        name: &str,
        raw: &str,
        source: ConnectionDefaultSource,
    ) -> Result<(), SqlError> {
        let mut state = self.store.borrow_mut();
        let mut values = state.defaults;
        change_setting(&mut values, &state.defaults, name, raw)?;
        let bit = guc_bit(name);
        state.connection_overrides |= bit;
        state.database_overrides &= !bit;
        state.role_overrides &= !bit;
        state.database_role_overrides &= !bit;
        state.client_overrides &= !bit;
        match source {
            ConnectionDefaultSource::Database => state.database_overrides |= bit,
            ConnectionDefaultSource::Role => state.role_overrides |= bit,
            ConnectionDefaultSource::DatabaseRole => state.database_role_overrides |= bit,
        }
        state.defaults = values;
        if state.session_overrides & bit == 0 {
            copy_guc_values(&mut state.current, &values, bit);
            if state.transaction.active {
                copy_guc_values(&mut state.transaction.start, &values, bit);
                copy_guc_values(&mut state.transaction.session, &values, bit);
            }
        }
        Ok(())
    }

    fn recompute_cluster_defaults(state: &mut GucStore) {
        let old_defaults = state.defaults;
        let old_current = state.current;
        let old_start = state.transaction.start;
        let old_session = state.transaction.session;
        let mut defaults = state.cluster_defaults;
        defaults.current_role = old_defaults.current_role;
        defaults.session_authorization = old_defaults.session_authorization;
        copy_guc_values(
            &mut defaults,
            &old_defaults,
            state.connection_overrides & GUC_ALL,
        );
        state.defaults = defaults;

        let mut current = defaults;
        current.current_role = old_current.current_role;
        current.session_authorization = old_current.session_authorization;
        copy_guc_values(&mut current, &old_current, state.session_overrides);
        state.current = current;
        if state.transaction.active {
            let mut start = defaults;
            start.current_role = old_start.current_role;
            start.session_authorization = old_start.session_authorization;
            copy_guc_values(&mut start, &old_start, state.transaction.start_overrides);
            state.transaction.start = start;
            let mut session = defaults;
            session.current_role = old_session.current_role;
            session.session_authorization = old_session.session_authorization;
            copy_guc_values(
                &mut session,
                &old_session,
                state.transaction.session_overrides,
            );
            state.transaction.session = session;
            for savepoint in state.transaction.savepoints.iter_mut().flatten() {
                let old_current = savepoint.current;
                let old_session = savepoint.session;
                savepoint.current = defaults;
                savepoint.current.current_role = old_current.current_role;
                savepoint.current.session_authorization = old_current.session_authorization;
                copy_guc_values(
                    &mut savepoint.current,
                    &old_current,
                    savepoint.current_overrides,
                );
                savepoint.session = defaults;
                savepoint.session.current_role = old_session.current_role;
                savepoint.session.session_authorization = old_session.session_authorization;
                copy_guc_values(
                    &mut savepoint.session,
                    &old_session,
                    savepoint.session_overrides,
                );
            }
        }
    }

    pub fn reset(&self, name: &str) -> Result<(), SqlError> {
        self.set(name, "DEFAULT", false)
    }

    pub fn reset_all(&self) {
        let mut state = self.store.borrow_mut();
        let values = state.defaults;
        state.current = values;
        state.session_overrides = 0;
        if state.transaction.active {
            state.transaction.session = values;
            state.transaction.session_overrides = 0;
        }
    }

    pub fn discard_all(&self) {
        let mut state = self.store.borrow_mut();
        let values = state.defaults;
        state.current = values;
        state.session_overrides = 0;
        state.transaction.start = values;
        state.transaction.session = values;
        state.transaction.start_overrides = 0;
        state.transaction.session_overrides = 0;
        state.transaction.savepoint_count = 0;
        self.seq_session.discard();
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
        state.transaction.start_overrides = state.session_overrides;
        state.transaction.session_overrides = state.session_overrides;
        state.transaction.savepoint_count = 0;
    }

    pub fn commit_transaction(&self) {
        let mut state = self.store.borrow_mut();
        if !state.transaction.active {
            return;
        }
        state.current = state.transaction.session;
        state.session_overrides = state.transaction.session_overrides;
        state.transaction.active = false;
        state.transaction.savepoint_count = 0;
    }

    pub fn rollback_transaction(&self) {
        let mut state = self.store.borrow_mut();
        if !state.transaction.active {
            return;
        }
        state.current = state.transaction.start;
        state.session_overrides = state.transaction.start_overrides;
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
            current_overrides: state.session_overrides,
            session_overrides: state.transaction.session_overrides,
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
        state.session_overrides = savepoint.current_overrides;
        state.transaction.session_overrides = savepoint.session_overrides;
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

    pub(crate) fn canonical_routine_setting(
        &self,
        name: &str,
        raw: &str,
    ) -> Result<StackStr<256>, SqlError> {
        let prior = {
            let mut state = self.store.borrow_mut();
            let prior = state.current;
            let mut values = state.current;
            change_setting(&mut values, &state.defaults, name, raw)?;
            state.current = values;
            prior
        };
        let value = self.get_owned(name).ok_or_else(|| {
            sql_err!(
                sqlstate::UNDEFINED_OBJECT,
                "unrecognized configuration parameter \"{}\"",
                name
            )
        });
        self.store.borrow_mut().current = prior;
        value
    }

    pub(crate) fn enter_routine_configs(
        &self,
        configs: &[crate::storage::RoutineConfig],
    ) -> Result<RoutineConfigScope, SqlError> {
        let mut state = self.store.borrow_mut();
        let prior = state.current;
        let session = state.transaction.session;
        let mut current = state.current;
        for config in configs {
            change_setting(
                &mut current,
                &state.defaults,
                config.name.as_str(),
                config.value.as_str(),
            )?;
        }
        state.current = current;
        drop(state);
        for config in configs {
            if let Some(value) = self.get_owned(config.name.as_str()) {
                let reset = self.reset_owned(config.name.as_str()).unwrap_or(value);
                crate::sql::eval::funcs::system::update_session_setting(
                    config.name.as_str(),
                    value,
                    reset,
                    self.source(config.name.as_str()),
                );
            }
        }
        ACTIVE_RENDER.with(|active| *active.borrow_mut() = Some(self.render()));
        crate::sql::timezone::set_session(self.timezone());
        Ok(RoutineConfigScope {
            guc: self as *const GucState,
            prior,
            session,
            names: {
                let mut names =
                    [crate::storage::SqlName::EMPTY; crate::storage::MAX_ROUTINE_CONFIGS];
                for (index, config) in configs.iter().enumerate() {
                    names[index] = config.name;
                }
                names
            },
            count: configs.len(),
        })
    }
}

pub(crate) fn enter_active_routine_configs(
    configs: &[crate::storage::RoutineConfig],
) -> Result<RoutineConfigScope, SqlError> {
    ACTIVE_GUC.with(|active| {
        let pointer = active.get();
        if pointer.is_null() {
            return Err(sql_err!(
                sqlstate::FEATURE_NOT_SUPPORTED,
                "routine configuration is unavailable outside statement execution"
            ));
        }
        // SAFETY: EvalScope owns this dynamic extent and every returned scope
        // is consumed before evaluation returns.
        unsafe { &*pointer }.enter_routine_configs(configs)
    })
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
        values.intervalstyle = defaults.intervalstyle;
    } else if name.eq_ignore_ascii_case("default_transaction_isolation") {
        values.default_transaction_isolation = defaults.default_transaction_isolation;
    } else if name.eq_ignore_ascii_case("default_transaction_read_only") {
        values.default_transaction_read_only = defaults.default_transaction_read_only;
    } else if name.eq_ignore_ascii_case("default_transaction_deferrable") {
        values.default_transaction_deferrable = defaults.default_transaction_deferrable;
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
        values.intervalstyle = if is_default {
            IntervalStyle::Postgres
        } else {
            IntervalStyle::parse(v).ok_or_else(|| {
                sql_err!(
                    sqlstate::INVALID_PARAMETER_VALUE,
                    "invalid value for parameter \"IntervalStyle\": \"{}\"",
                    v
                )
            })?
        };
        return Ok(());
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
    if name.eq_ignore_ascii_case("default_transaction_isolation") {
        values.default_transaction_isolation = if is_default {
            TransactionIsolation::ReadCommitted
        } else {
            parse_transaction_isolation(v).ok_or_else(|| {
                sql_err!(
                    sqlstate::INVALID_PARAMETER_VALUE,
                    "invalid value for parameter \"default_transaction_isolation\": \"{}\"",
                    v
                )
            })?
        };
        return Ok(());
    }
    if name.eq_ignore_ascii_case("default_transaction_read_only") {
        values.default_transaction_read_only = if is_default {
            false
        } else {
            parse_on_off(v).ok_or_else(|| unsupported_value("default_transaction_read_only", v))?
        };
        return Ok(());
    }
    if name.eq_ignore_ascii_case("default_transaction_deferrable") {
        values.default_transaction_deferrable = if is_default {
            false
        } else {
            parse_on_off(v).ok_or_else(|| unsupported_value("default_transaction_deferrable", v))?
        };
        return Ok(());
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
            Some(StackStr::from_str(values.intervalstyle.as_str()))
        } else if name.eq_ignore_ascii_case("synchronize_seqscans") {
            Some(StackStr::from_str("off"))
        } else if name.eq_ignore_ascii_case("default_transaction_isolation") {
            Some(StackStr::from_str(
                values.default_transaction_isolation.as_str(),
            ))
        } else if name.eq_ignore_ascii_case("default_transaction_read_only") {
            Some(StackStr::from_str(
                if values.default_transaction_read_only {
                    "on"
                } else {
                    "off"
                },
            ))
        } else if name.eq_ignore_ascii_case("default_transaction_deferrable") {
            Some(StackStr::from_str(
                if values.default_transaction_deferrable {
                    "on"
                } else {
                    "off"
                },
            ))
        } else if name.eq_ignore_ascii_case("seed") {
            Some(StackStr::from_str("unavailable"))
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
            intervalstyle: values.intervalstyle,
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

pub(crate) fn parse_transaction_isolation(v: &str) -> Option<TransactionIsolation> {
    if v.eq_ignore_ascii_case("read uncommitted") {
        Some(TransactionIsolation::ReadUncommitted)
    } else if v.eq_ignore_ascii_case("read committed") {
        Some(TransactionIsolation::ReadCommitted)
    } else if v.eq_ignore_ascii_case("repeatable read") {
        Some(TransactionIsolation::RepeatableRead)
    } else if v.eq_ignore_ascii_case("serializable") {
        Some(TransactionIsolation::Serializable)
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
        "{} \"{}\" is not supported (only the default is implemented)",
        param,
        v
    )
}
