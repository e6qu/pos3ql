//! Per-session configuration parameters (GUCs). `SET` writes them and `SHOW`
//! reads them. A value we cannot honor is rejected loudly, so a client never
//! receives false success for an ignored setting.

use crate::sql::eval::sqlstate;
use core::cell::{Cell, RefCell};
use core::fmt::Write;

use crate::mem::{Budget, BudgetError, FixedVec};
use crate::sql_err;
use crate::storage::{SequenceCacheIdentity, SequenceDef};
use crate::util::StackStr;

use super::ast::{TransactionCharacteristics, TransactionIsolation};
use super::datetime::{DateFormat, DateStyle, FieldOrder, IntervalStyle};
use super::eval::SqlError;

/// Maximum stored byte length of a session search path. The resolver's
/// entry bound is derived from this value, so a byte-valid path cannot overrun
/// a narrower effective-path array.
pub const SEARCH_PATH_BYTES: usize = 128;

pub(crate) const SHOW_ALL_COLUMNS: [super::types::ColDesc<'static>; 3] = [
    super::types::ColDesc::new("name", super::types::oid::TEXT, -1),
    super::types::ColDesc::new("setting", super::types::oid::TEXT, -1),
    super::types::ColDesc::new("description", super::types::oid::TEXT, -1),
];

#[derive(Clone, Copy, PartialEq, Eq)]
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

    /// Select uniformly from the inclusive range.  Mask-and-reject is the
    /// algorithm PostgreSQL uses; modulo reduction would bias most ranges.
    fn next_u64_range(&mut self, min: u64, max: u64) -> u64 {
        if max <= min {
            return min;
        }
        let range = max - min;
        let shift = range.leading_zeros();
        loop {
            let value = self.next_u64() >> shift;
            if value <= range {
                return min + value;
            }
        }
    }

    fn next_i64_range(&mut self, min: i64, max: i64) -> i64 {
        if max <= min {
            return min;
        }
        let offset = self.next_u64_range(0, (max as u64).wrapping_sub(min as u64));
        (min as u64).wrapping_add(offset) as i64
    }

    fn next_normal(&mut self) -> f64 {
        // Box-Muller requires (0, 1], while next_f64() produces [0, 1).
        let u1 = 1.0 - self.next_f64();
        let u2 = 1.0 - self.next_f64();
        (-2.0 * u1.ln()).sqrt() * (2.0 * core::f64::consts::PI * u2).sin()
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

/// Publishes the session's settings, without a transaction, to work a
/// transaction boundary runs, such as constraint triggers fired at COMMIT.
pub(crate) struct SettingsScope {
    prior: *const GucState,
}

impl Drop for SettingsScope {
    fn drop(&mut self) {
        ACTIVE_GUC.with(|active| active.set(self.prior));
    }
}

pub(crate) fn enter_settings_scope(guc: &GucState) -> SettingsScope {
    SettingsScope {
        prior: ACTIVE_GUC.with(|active| active.replace(guc as *const GucState)),
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

pub(crate) fn with_active_state<T>(
    apply: impl FnOnce(&GucState) -> Result<T, SqlError>,
) -> Result<T, SqlError> {
    ACTIVE_GUC.with(|active| {
        let pointer = active.get();
        if pointer.is_null() {
            return Err(sql_err!(
                sqlstate::INTERNAL_ERROR,
                "procedural subtransaction has no active session configuration"
            ));
        }
        // EvalScope keeps the connection-owned state live. This boundary
        // borrows no transaction through ACTIVE_TXN while its executor owns it.
        apply(unsafe { &*pointer })
    })
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

pub(crate) fn active_random_u64_range(min: u64, max: u64) -> Result<u64, SqlError> {
    ACTIVE_GUC.with(|active| {
        let pointer = active.get();
        if pointer.is_null() {
            return Err(sql_err!(
                sqlstate::FEATURE_NOT_SUPPORTED,
                "random is unavailable outside statement execution"
            ));
        }
        // SAFETY: EvalScope owns the pointer's dynamic extent.
        unsafe { &*pointer }.random_u64_range(min, max)
    })
}

pub(crate) fn active_random_i64_range(min: i64, max: i64) -> Result<i64, SqlError> {
    ACTIVE_GUC.with(|active| {
        let pointer = active.get();
        if pointer.is_null() {
            return Err(sql_err!(
                sqlstate::FEATURE_NOT_SUPPORTED,
                "random is unavailable outside statement execution"
            ));
        }
        // SAFETY: EvalScope owns the pointer's dynamic extent.
        unsafe { &*pointer }.random_i64_range(min, max)
    })
}

pub(crate) fn active_random_normal(mean: f64, stddev: f64) -> Result<f64, SqlError> {
    ACTIVE_GUC.with(|active| {
        let pointer = active.get();
        if pointer.is_null() {
            return Err(sql_err!(
                sqlstate::FEATURE_NOT_SUPPORTED,
                "random_normal is unavailable outside statement execution"
            ));
        }
        // SAFETY: EvalScope owns the pointer's dynamic extent.
        unsafe { &*pointer }.random_normal(mean, stddev)
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

/// Snapshot the active session's planner inputs for nested SQL, such as a
/// PL/pgSQL dynamic statement.  Statement execution installs this context for
/// the entire recursive call chain.
pub(crate) fn active_planner_settings() -> PlannerSettings {
    ACTIVE_GUC.with(|active| {
        let pointer = active.get();
        if pointer.is_null() {
            return PlannerSettings::EMPTY;
        }
        // SAFETY: `enter_eval_scope` owns the pointer's dynamic extent.
        unsafe { &*pointer }.planner_settings()
    })
}

/// The PL/pgSQL settings a body is compiled under.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct PlpgsqlSettings {
    pub(crate) variable_conflict: VariableConflict,
    pub(crate) print_strict_params: bool,
}

/// The session's PL/pgSQL settings, for a trigger function compiled while a
/// statement runs.
pub(crate) fn active_plpgsql_settings() -> Result<PlpgsqlSettings, SqlError> {
    ACTIVE_GUC.with(|active| {
        let pointer = active.get();
        if pointer.is_null() {
            return Err(sql_err!(
                sqlstate::INTERNAL_ERROR,
                "PL/pgSQL compilation has no session settings"
            ));
        }
        // SAFETY: `enter_eval_scope` owns the pointer's dynamic extent.
        Ok(unsafe { &*pointer }.plpgsql_settings())
    })
}

pub(crate) fn active_track_functions() -> TrackFunctions {
    ACTIVE_GUC.with(|active| {
        let pointer = active.get();
        if pointer.is_null() {
            return TrackFunctions::None;
        }
        // SAFETY: `enter_eval_scope` owns the pointer's dynamic extent.
        unsafe { &*pointer }.track_functions()
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
        if let Some(characteristics) = guc.current_transaction_value(name, value) {
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
    if let Some(value) = guc.get_owned(name) {
        let reset = guc.reset_owned(name).unwrap_or(value);
        crate::sql::eval::funcs::system::update_session_setting(
            name,
            value,
            reset,
            guc.source(name),
        );
    } else {
        // Ordinary settings do not borrow the executor's mutable transaction.
        let txn_pointer = ACTIVE_TXN.with(Cell::get);
        if !txn_pointer.is_null() {
            // SAFETY: EvalScope owns the pointer's dynamic extent.
            let txn = unsafe { &*txn_pointer };
            if let Some(value) = transaction_setting_owned(txn, name) {
                let reset = guc.transaction_reset_owned(name).unwrap_or(value);
                crate::sql::eval::funcs::system::update_session_setting(
                    name, value, reset, "session",
                );
            }
        }
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
    pub xml_document: bool,
    /// PostgreSQL's legacy significant-digit reduction for negative values.
    /// Non-negative values use shortest-round-trip output.
    pub extra_float_digits: i8,
}

impl Default for RenderContext {
    fn default() -> Self {
        RenderContext {
            datestyle: DateStyle::default(),
            intervalstyle: IntervalStyle::Postgres,
            parsed_timezone: super::timezone::Timezone::utc(),
            min_message_level: MessageLevel::Notice,
            bytea_escape: false,
            xml_document: false,
            extra_float_digits: 1,
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
    pub(super) statement_replay: Cell<super::sequence::StatementSequenceReplay>,
    /// Per-slot `(created_at, value)`; `created_at == 0` means undefined.
    currvals: [Cell<(u64, i64)>; INLINE_SEQUENCE_SLOTS],
    /// The last nextval source; setval can change its value without selecting
    /// a different source, and slot reuse must invalidate it.
    lastval: Cell<Option<(usize, u64)>>,
    caches: [Cell<SequenceCache>; INLINE_SEQUENCE_SLOTS],
    overflow_currvals: Option<FixedVec<Cell<(u64, i64)>>>,
    overflow_caches: Option<FixedVec<Cell<SequenceCache>>>,
    /// The wake time (microseconds since the Unix epoch) of the `pg_sleep`
    /// call that the current statement is parked on. The statement's effect
    /// journal replays the sleeps that already finished.
    sleep_until: Cell<Option<i64>>,
    /// When the current statement first found its database still used by
    /// other sessions (microseconds since the Unix epoch).
    database_wait_since: Cell<Option<i64>>,
}

const INLINE_SEQUENCE_SLOTS: usize = 64;

#[derive(Clone, Copy)]
struct SequenceCache {
    identity: Option<SequenceCacheIdentity>,
    next: i64,
    remaining: i64,
    increment: i64,
    min_value: i64,
    max_value: i64,
    cycle: bool,
}

impl SequenceCache {
    const EMPTY: Self = Self {
        identity: None,
        next: 0,
        remaining: 0,
        increment: 1,
        min_value: 0,
        max_value: 0,
        cycle: false,
    };

    fn advance(self, value: i64) -> i64 {
        match value.checked_add(self.increment) {
            Some(next) if self.increment > 0 && next <= self.max_value => next,
            Some(next) if self.increment < 0 && next >= self.min_value => next,
            _ if self.cycle && self.increment > 0 => self.min_value,
            _ if self.cycle => self.max_value,
            _ => value,
        }
    }
}

impl SeqSession {
    pub(crate) fn retains_statement_effects(&self) -> bool {
        self.statement_replay.get().is_retained()
    }

    const fn new() -> Self {
        SeqSession {
            statement_replay: Cell::new(super::sequence::StatementSequenceReplay::EMPTY),
            currvals: [const { Cell::new((0u64, 0i64)) }; INLINE_SEQUENCE_SLOTS],
            lastval: Cell::new(None),
            caches: [const { Cell::new(SequenceCache::EMPTY) }; INLINE_SEQUENCE_SLOTS],
            overflow_currvals: None,
            overflow_caches: None,
            sleep_until: Cell::new(None),
            database_wait_since: Cell::new(None),
        }
    }

    /// The wake time of the sleep the current statement waits for.
    pub(crate) fn sleep_until(&self) -> Option<i64> {
        self.sleep_until.get()
    }

    pub(super) fn set_sleep_until(&self, deadline: Option<i64>) {
        self.sleep_until.set(deadline);
    }

    pub(super) fn database_wait_since(&self) -> Option<i64> {
        self.database_wait_since.get()
    }

    pub(super) fn set_database_wait_since(&self, started: Option<i64>) {
        self.database_wait_since.set(started);
    }

    pub(crate) const fn extra_budget_bytes(sequence_capacity: usize) -> usize {
        sequence_capacity
            .saturating_sub(INLINE_SEQUENCE_SLOTS)
            .saturating_mul(
                core::mem::size_of::<Cell<(u64, i64)>>()
                    .saturating_add(core::mem::size_of::<Cell<SequenceCache>>()),
            )
    }

    fn with_capacity(budget: &mut Budget, sequence_capacity: usize) -> Result<Self, BudgetError> {
        let overflow = sequence_capacity.saturating_sub(INLINE_SEQUENCE_SLOTS);
        if overflow == 0 {
            return Ok(Self::new());
        }
        let mut overflow_currvals = FixedVec::new(budget, "session_sequence_currvals", overflow)?;
        let mut overflow_caches = FixedVec::new(budget, "session_sequence_caches", overflow)?;
        for _ in 0..overflow {
            overflow_currvals
                .push(Cell::new((0, 0)))
                .expect("sized to max_sequences");
            overflow_caches
                .push(Cell::new(SequenceCache::EMPTY))
                .expect("sized to max_sequences");
        }
        Ok(Self {
            overflow_currvals: Some(overflow_currvals),
            overflow_caches: Some(overflow_caches),
            ..Self::new()
        })
    }

    fn currval_cell(&self, slot: usize) -> &Cell<(u64, i64)> {
        if slot < INLINE_SEQUENCE_SLOTS {
            &self.currvals[slot]
        } else {
            &self
                .overflow_currvals
                .as_ref()
                .expect("sequence slot is configured")[slot - INLINE_SEQUENCE_SLOTS]
        }
    }

    fn cache_cell(&self, slot: usize) -> &Cell<SequenceCache> {
        if slot < INLINE_SEQUENCE_SLOTS {
            &self.caches[slot]
        } else {
            &self
                .overflow_caches
                .as_ref()
                .expect("sequence slot is configured")[slot - INLINE_SEQUENCE_SLOTS]
        }
    }

    pub(crate) fn take_cached(&self, slot: usize, identity: SequenceCacheIdentity) -> Option<i64> {
        let mut cache = self.cache_cell(slot).get();
        if cache.identity != Some(identity) || cache.remaining == 0 {
            return None;
        }
        let value = cache.next;
        cache.remaining -= 1;
        if cache.remaining != 0 {
            cache.next = cache.advance(value);
        }
        self.cache_cell(slot).set(cache);
        Some(value)
    }

    pub(crate) fn store_cache(
        &self,
        slot: usize,
        first: i64,
        reserved: i64,
        definition: &SequenceDef,
    ) {
        let mut cache = SequenceCache {
            identity: Some(definition.cache_identity()),
            next: first,
            remaining: reserved.saturating_sub(1),
            increment: definition.increment,
            min_value: definition.min_value,
            max_value: definition.max_value,
            cycle: definition.cycle,
        };
        cache.next = cache.advance(first);
        self.cache_cell(slot).set(cache);
    }

    pub(crate) fn invalidate_cache(&self, slot: usize) {
        self.cache_cell(slot).set(SequenceCache::EMPTY);
    }

    /// Records a `nextval`: defines both this sequence's `currval` and `lastval`.
    pub fn record_nextval(&self, slot: usize, created_at: u64, value: i64) {
        self.currval_cell(slot).set((created_at, value));
        self.lastval.set(Some((slot, created_at)));
    }

    /// Restore the logical result. Cache invalidation belongs to the physical
    /// setval, so replay cannot discard values reserved later in the attempt.
    pub fn record_setval(&self, slot: usize, created_at: u64, value: i64, is_called: bool) {
        if is_called {
            self.currval_cell(slot).set((created_at, value));
        }
    }

    /// This sequence's `currval` in this session, if `nextval`/`setval` has
    /// defined it (the stamp must still match the live sequence).
    pub fn currval(&self, slot: usize, created_at: u64) -> Option<i64> {
        let (stamp, value) = self.currval_cell(slot).get();
        (stamp != 0 && stamp == created_at).then_some(value)
    }

    pub fn lastval(&self) -> Option<i64> {
        let (slot, created_at) = self.lastval.get()?;
        self.currval(slot, created_at)
    }

    pub(crate) fn last_sequence(&self) -> Option<(usize, u64)> {
        self.lastval.get()
    }

    pub(super) fn restore_last_sequence(&self, source: Option<(usize, u64)>) {
        self.lastval.set(source);
    }

    pub fn discard(&self) {
        for value in &self.currvals {
            value.set((0, 0));
        }
        for cache in &self.caches {
            cache.set(SequenceCache::EMPTY);
        }
        if let Some(values) = &self.overflow_currvals {
            for value in values.iter() {
                value.set((0, 0));
            }
        }
        if let Some(caches) = &self.overflow_caches {
            for cache in caches.iter() {
                cache.set(SequenceCache::EMPTY);
            }
        }
        self.lastval.set(None);
    }
}

#[derive(Clone, Copy)]
struct GucValues {
    /// Effective authorization identifier. Kept in the transactional GUC
    /// snapshot so SET ROLE follows PostgreSQL's rollback/savepoint behavior.
    current_role: crate::storage::SessionIdentity,
    /// Transactional session authorization identifier. SET SESSION
    /// AUTHORIZATION changes this and current_role together.
    session_authorization: crate::storage::SessionIdentity,
    datestyle: StackStr<48>,
    intervalstyle: IntervalStyle,
    lc_monetary: StackStr<16>,
    timezone: StackStr<64>,
    /// Parsed current time zone, so rendering does not re-parse it.
    parsed_timezone: super::timezone::Timezone,

    client_encoding: StackStr<32>,
    application_name: StackStr<64>,
    search_path: StackStr<SEARCH_PATH_BYTES>,
    default_tablespace: StackStr<64>,
    default_text_search_config: StackStr<128>,
    client_min_messages: MessageLevel,
    extra_float_digits: i32,
    /// Milliseconds; 0 disables the timeout.
    lock_timeout: i32,
    /// Milliseconds (0 disables it), enforced at scan boundaries during
    /// execution.
    statement_timeout: i32,
    /// Kilobytes. Reported and saved like PostgreSQL's `work_mem`; the fixed
    /// startup `work_arena_bytes` workspace bounds actual query memory.
    work_mem: i32,
    row_security: StackStr<4>,
    event_triggers: bool,
    /// bytea_output = escape (false = hex, the default).
    bytea_escape: bool,
    xml_document: bool,
    /// Whether stored function bodies are checked when their definitions are created.
    check_function_bodies: bool,
    default_transaction_isolation: TransactionIsolation,
    default_transaction_read_only: bool,
    default_transaction_deferrable: bool,
    password_encryption: PasswordEncryption,
    track_functions: TrackFunctions,
    plpgsql_variable_conflict: VariableConflict,
    plpgsql_print_strict_params: bool,
}

/// `plpgsql.variable_conflict` and a PL/pgSQL body's `#variable_conflict`:
/// what a name that is both a PL/pgSQL variable and a table column means in a
/// statement the body runs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VariableConflict {
    /// Report the reference as ambiguous (42702), PostgreSQL's default.
    Error,
    UseVariable,
    UseColumn,
}

impl VariableConflict {
    pub(crate) const NAMES: [&'static str; 3] = ["error", "use_variable", "use_column"];

    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::Error => "error",
            Self::UseVariable => "use_variable",
            Self::UseColumn => "use_column",
        }
    }

    pub(crate) fn parse(value: &str) -> Option<Self> {
        [Self::Error, Self::UseVariable, Self::UseColumn]
            .into_iter()
            .find(|mode| value.eq_ignore_ascii_case(mode.as_str()))
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum TrackFunctions {
    None,
    Pl,
    All,
}

impl TrackFunctions {
    const fn as_str(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Pl => "pl",
            Self::All => "all",
        }
    }
}

/// Planner inputs reported by `EXPLAIN (SETTINGS)`.  This is deliberately a
/// closed snapshot: only values consulted while resolving a query belong here,
/// rather than every session presentation setting.
#[derive(Clone, Copy)]
pub(crate) struct PlannerSettings {
    entries: [PlannerSetting; 1],
    count: usize,
}

#[derive(Clone, Copy)]
pub(crate) struct PlannerSetting {
    name: &'static str,
    value: StackStr<128>,
}

impl PlannerSettings {
    pub(crate) const EMPTY: Self = Self {
        entries: [PlannerSetting {
            name: "",
            value: StackStr::new(),
        }],
        count: 0,
    };

    pub(crate) fn entries(&self) -> &[PlannerSetting] {
        &self.entries[..self.count]
    }
}

impl PlannerSetting {
    pub(crate) fn name(&self) -> &'static str {
        self.name
    }

    pub(crate) fn value(&self) -> &str {
        self.value.as_str()
    }
}

/// PostgreSQL's closed credential format choice for new plaintext role
/// passwords. Imported verifier strings remain their own typed input.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PasswordEncryption {
    ScramSha256,
    Md5,
}

impl GucValues {
    fn same_settings(&self, other: &Self) -> bool {
        // The stored time-zone name determines its private parsed cache.
        macro_rules! equal { ($($field:ident),+ $(,)?) => { true $(&& self.$field == other.$field)+ }; }
        equal!(
            current_role,
            session_authorization,
            datestyle,
            intervalstyle,
            lc_monetary,
            timezone,
            client_encoding,
            application_name,
            search_path,
            default_tablespace,
            default_text_search_config,
            client_min_messages,
            extra_float_digits,
            lock_timeout,
            statement_timeout,
            work_mem,
            row_security,
            event_triggers,
            bytea_escape,
            xml_document,
            check_function_bodies,
            default_transaction_isolation,
            default_transaction_read_only,
            default_transaction_deferrable,
            password_encryption,
            track_functions,
            plpgsql_variable_conflict,
            plpgsql_print_strict_params
        )
    }

    fn new() -> Self {
        let mut values = Self {
            current_role: crate::storage::SessionIdentity::bootstrap(),
            session_authorization: crate::storage::SessionIdentity::bootstrap(),
            datestyle: StackStr::new(),
            intervalstyle: IntervalStyle::Postgres,
            lc_monetary: StackStr::from_str("C.UTF-8"),
            timezone: StackStr::new(),
            parsed_timezone: super::timezone::Timezone::utc(),
            client_encoding: StackStr::new(),
            application_name: StackStr::new(),
            search_path: StackStr::new(),
            default_tablespace: StackStr::new(),
            default_text_search_config: StackStr::new(),
            client_min_messages: MessageLevel::Notice,
            extra_float_digits: 1,
            lock_timeout: 0,
            statement_timeout: 0,
            work_mem: 4096,
            row_security: StackStr::new(),
            event_triggers: true,
            bytea_escape: false,
            xml_document: false,
            check_function_bodies: true,
            default_transaction_isolation: TransactionIsolation::ReadCommitted,
            default_transaction_read_only: false,
            default_transaction_deferrable: false,
            password_encryption: PasswordEncryption::ScramSha256,
            track_functions: TrackFunctions::None,
            plpgsql_variable_conflict: VariableConflict::Error,
            plpgsql_print_strict_params: false,
        };
        let _ = write!(values.datestyle, "ISO, MDY");
        let _ = write!(values.timezone, "UTC");
        let _ = write!(values.client_encoding, "UTF8");
        let _ = write!(values.search_path, "\"$user\", public");
        let _ = write!(values.default_text_search_config, "pg_catalog.english");
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
const GUC_EVENT_TRIGGERS: u32 = 1 << 23;
const GUC_DEFAULT_TEXT_SEARCH_CONFIG: u32 = 1 << 24;
const GUC_PASSWORD_ENCRYPTION: u32 = 1 << 25;
const GUC_LC_MONETARY: u32 = 1 << 26;
const GUC_TRACK_FUNCTIONS: u32 = 1 << 27;
const GUC_PLPGSQL_VARIABLE_CONFLICT: u32 = 1 << 28;
const GUC_WORK_MEM: u32 = 1 << 29;
const GUC_PLPGSQL_PRINT_STRICT_PARAMS: u32 = 1 << 30;
const GUC_SETTING_COUNT: usize = 31;
const GUC_ALL: u32 = (1 << GUC_SETTING_COUNT) - 1;

#[derive(Clone, Copy)]
struct SettingActions {
    next: u64,
    session: [u64; GUC_SETTING_COUNT],
}

impl SettingActions {
    const EMPTY: Self = Self {
        next: 0,
        session: [0; GUC_SETTING_COUNT],
    };

    fn record_session(&mut self, mask: u32) -> Result<(), SqlError> {
        let next = self.next.checked_add(1).ok_or_else(|| {
            sql_err!(
                sqlstate::PROGRAM_LIMIT_EXCEEDED,
                "session setting assignment counter exhausted"
            )
        })?;
        for (index, action) in self.session.iter_mut().enumerate() {
            if mask & (1 << index) != 0 {
                *action = next;
            }
        }
        self.next = next;
        Ok(())
    }

    fn restore_mask(&self, configured: u32, entered: u64) -> u32 {
        self.session
            .iter()
            .enumerate()
            .fold(configured, |mask, (index, &action)| {
                if action > entered {
                    mask & !(1 << index)
                } else {
                    mask
                }
            })
    }
}

fn guc_bit(name: &str) -> u32 {
    if name.eq_ignore_ascii_case("datestyle") {
        GUC_DATESTYLE
    } else if name.eq_ignore_ascii_case("intervalstyle") {
        GUC_INTERVALSTYLE
    } else if name.eq_ignore_ascii_case("lc_monetary") {
        GUC_LC_MONETARY
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
    } else if name.eq_ignore_ascii_case("default_text_search_config") {
        GUC_DEFAULT_TEXT_SEARCH_CONFIG
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
    } else if name.eq_ignore_ascii_case("event_triggers") {
        GUC_EVENT_TRIGGERS
    } else if name.eq_ignore_ascii_case("password_encryption") {
        GUC_PASSWORD_ENCRYPTION
    } else if name.eq_ignore_ascii_case("track_functions") {
        GUC_TRACK_FUNCTIONS
    } else if name.eq_ignore_ascii_case("plpgsql.variable_conflict") {
        GUC_PLPGSQL_VARIABLE_CONFLICT
    } else if name.eq_ignore_ascii_case("work_mem") {
        GUC_WORK_MEM
    } else if name.eq_ignore_ascii_case("plpgsql.print_strict_params") {
        GUC_PLPGSQL_PRINT_STRICT_PARAMS
    } else {
        0
    }
}

pub(crate) fn knows_parameter(name: &str) -> bool {
    guc_bit(name) != 0
}

pub(crate) fn requires_set_privilege(name: &str) -> bool {
    name.eq_ignore_ascii_case("event_triggers")
        || name.eq_ignore_ascii_case("track_functions")
        || name.eq_ignore_ascii_case("plpgsql.variable_conflict")
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
    copy!(GUC_LC_MONETARY, lc_monetary);
    if mask & GUC_TIMEZONE != 0 {
        target.timezone = source.timezone;
        target.parsed_timezone = source.parsed_timezone;
    }
    copy!(GUC_CLIENT_ENCODING, client_encoding);
    copy!(GUC_APPLICATION_NAME, application_name);
    copy!(GUC_SEARCH_PATH, search_path);
    copy!(GUC_DEFAULT_TABLESPACE, default_tablespace);
    copy!(GUC_DEFAULT_TEXT_SEARCH_CONFIG, default_text_search_config);
    copy!(GUC_CLIENT_MIN_MESSAGES, client_min_messages);
    copy!(GUC_EXTRA_FLOAT_DIGITS, extra_float_digits);
    copy!(GUC_LOCK_TIMEOUT, lock_timeout);
    copy!(GUC_STATEMENT_TIMEOUT, statement_timeout);
    copy!(GUC_WORK_MEM, work_mem);
    copy!(GUC_ROW_SECURITY, row_security);
    copy!(GUC_EVENT_TRIGGERS, event_triggers);
    copy!(GUC_BYTEA_OUTPUT, bytea_escape);
    copy!(GUC_CHECK_FUNCTION_BODIES, check_function_bodies);
    copy!(GUC_XML_OPTION, xml_document);
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
    copy!(GUC_PASSWORD_ENCRYPTION, password_encryption);
    copy!(GUC_TRACK_FUNCTIONS, track_functions);
    copy!(GUC_PLPGSQL_VARIABLE_CONFLICT, plpgsql_variable_conflict);
    copy!(GUC_PLPGSQL_PRINT_STRICT_PARAMS, plpgsql_print_strict_params);
}

fn finish_setting_change(
    state: &mut GucStore,
    values: GucValues,
    bit: u32,
    local: bool,
    resetting: bool,
) -> Result<(), SqlError> {
    if !local {
        state.actions.record_session(bit)?;
    }
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
    Ok(())
}

#[derive(Clone, Copy)]
struct GucSavepoint {
    current: GucValues,
    session: GucValues,
    current_overrides: u32,
    session_overrides: u32,
    session_actions: [u64; GUC_SETTING_COUNT],
}

pub(crate) struct StatementSettings {
    values: GucSavepoint,
    random: PrngState,
}

struct GucTransaction {
    active: bool,
    start: GucValues,
    session: GucValues,
    start_overrides: u32,
    session_overrides: u32,
    start_actions: [u64; GUC_SETTING_COUNT],
    savepoints: Vec<Option<GucSavepoint>>,
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
    actions: SettingActions,
    transaction: GucTransaction,
}

#[derive(Clone, Copy)]
pub(crate) enum ConnectionDefaultSource {
    Database,
    Role,
    DatabaseRole,
}

pub struct GucState {
    transaction_mode: crate::config::TransactionMode,
    store: RefCell<GucStore>,
    /// Immutable authenticated identity from the startup packet. PostgreSQL
    /// uses this identity—not a later SET ROLE or SET SESSION
    /// AUTHORIZATION—as the authority for changing session authorization.
    authenticated_user: Cell<crate::storage::SessionIdentity>,
    /// This connection's `currval`/`lastval` state.
    seq_session: SeqSession,
    random: Cell<PrngState>,
}

pub(crate) struct RoutineConfigScope {
    guc: *const GucState,
    prior: GucValues,
    configured: u32,
    entered: u64,
    owner: super::ExecutionIdentity,
}

/// Pointer-free settings retained with a paused invocation. The caller stores
/// this image in its existing execution-control reservation.
#[derive(Clone, Copy)]
pub(crate) struct RoutineConfigImage {
    prior: GucValues,
    current: GucValues,
    configured: u32,
    restore: u32,
    entered: u64,
    paused: u64,
    owner: super::ExecutionIdentity,
}

impl RoutineConfigScope {
    pub(crate) fn save(&self) -> Result<RoutineConfigImage, SqlError> {
        if self.owner != super::execution_identity() || ACTIVE_GUC.with(Cell::get) != self.guc {
            return Err(routine_config_changed());
        }
        // SAFETY: the matching active evaluation still owns this scope.
        let state = unsafe { &*self.guc }.store.borrow();
        Ok(RoutineConfigImage {
            prior: self.prior,
            current: state.current,
            configured: self.configured,
            restore: state.actions.restore_mask(self.configured, self.entered),
            entered: self.entered,
            paused: state.actions.next,
            owner: self.owner,
        })
    }
}

impl RoutineConfigImage {
    pub(crate) fn enter(&self) -> Result<RoutineConfigScope, SqlError> {
        if self.owner != super::execution_identity() {
            return Err(routine_config_changed());
        }
        with_active_state(|guc| {
            let mut state = guc.store.borrow_mut();
            let mut parent = self.current;
            copy_guc_values(&mut parent, &self.prior, self.restore);
            if state.actions.next != self.paused || !parent.same_settings(&state.current) {
                return Err(routine_config_changed());
            }
            state.current = self.current;
            drop(state);
            guc.publish_owned_settings();
            Ok(RoutineConfigScope {
                guc: guc as *const GucState,
                prior: self.prior,
                configured: self.configured,
                entered: self.entered,
                owner: self.owner,
            })
        })
    }
}

fn routine_config_changed() -> SqlError {
    sql_err!(
        sqlstate::SERIALIZATION_FAILURE,
        "saved routine settings belong to another execution or parent scope"
    )
}

impl Drop for RoutineConfigScope {
    fn drop(&mut self) {
        // SAFETY: routine scopes are created only beneath the statement's
        // EvalScope and are dropped before that scope releases the GucState.
        let guc = unsafe { &*self.guc };
        let mut state = guc.store.borrow_mut();
        let restore = state.actions.restore_mask(self.configured, self.entered);
        let mut restored = state.current;
        copy_guc_values(&mut restored, &self.prior, restore);
        state.current = restored;
        drop(state);
        guc.publish_owned_settings();
    }
}

impl Default for GucState {
    fn default() -> Self {
        Self::new()
    }
}

impl GucState {
    pub(crate) fn statement_settings(&self) -> StatementSettings {
        let state = self.store.borrow();
        StatementSettings {
            values: GucSavepoint {
                current: state.current,
                session: state.transaction.session,
                current_overrides: state.session_overrides,
                session_overrides: state.transaction.session_overrides,
                session_actions: state.actions.session,
            },
            random: self.random.get(),
        }
    }

    pub(crate) fn restore_statement_settings(&self, mark: StatementSettings) {
        {
            let mut state = self.store.borrow_mut();
            state.current = mark.values.current;
            state.transaction.session = mark.values.session;
            state.session_overrides = mark.values.current_overrides;
            state.transaction.session_overrides = mark.values.session_overrides;
            state.actions.session = mark.values.session_actions;
        }
        self.random.set(mark.random);
        ACTIVE_RENDER.with(|active| *active.borrow_mut() = Some(self.render()));
        crate::sql::timezone::set_session(self.timezone());
    }

    pub(crate) fn matches_statement_settings(&self, mark: &StatementSettings) -> bool {
        let state = self.store.borrow();
        state.current.same_settings(&mark.values.current)
            && state
                .transaction
                .session
                .same_settings(&mark.values.session)
            && state.session_overrides == mark.values.current_overrides
            && state.transaction.session_overrides == mark.values.session_overrides
            && state.actions.session == mark.values.session_actions
            && self.random.get() == mark.random
    }

    #[cfg(test)]
    pub(crate) fn new_with_sequence_capacity(
        budget: &mut Budget,
        sequence_capacity: usize,
    ) -> Result<Self, BudgetError> {
        Self::new_with_capacities(
            budget,
            sequence_capacity,
            super::txn::DEFAULT_MAX_SAVEPOINTS,
            crate::config::TransactionMode::Mvcc,
        )
    }

    pub(crate) const fn extra_savepoint_budget_bytes(capacity: usize) -> usize {
        capacity.saturating_mul(core::mem::size_of::<Option<GucSavepoint>>())
    }

    pub fn new_with_capacities(
        budget: &mut Budget,
        sequence_capacity: usize,
        savepoint_capacity: usize,
        transaction_mode: crate::config::TransactionMode,
    ) -> Result<Self, BudgetError> {
        budget.draw_array(
            savepoint_capacity,
            core::mem::size_of::<Option<GucSavepoint>>(),
            "GUC savepoints",
        )?;
        let mut state = Self::with_savepoints(vec![None; savepoint_capacity], transaction_mode);
        state.seq_session = SeqSession::with_capacity(budget, sequence_capacity)?;
        Ok(state)
    }

    /// Copies only into buffers reserved by the destination at startup.
    pub(crate) fn capture_commit_context(&self, target: &mut Self) -> Result<(), SqlError> {
        let source = self.store.borrow();
        let destination = target.store.get_mut();
        if source.transaction.savepoint_count > destination.transaction.savepoints.len() {
            return Err(sql_err!(
                sqlstate::PROGRAM_LIMIT_EXCEEDED,
                "commit context exceeds max_savepoints_per_transaction"
            ));
        }
        destination.current = source.current;
        destination.defaults = source.defaults;
        destination.cluster_defaults = source.cluster_defaults;
        destination.cluster_overrides = source.cluster_overrides;
        destination.connection_overrides = source.connection_overrides;
        destination.database_overrides = source.database_overrides;
        destination.role_overrides = source.role_overrides;
        destination.database_role_overrides = source.database_role_overrides;
        destination.client_overrides = source.client_overrides;
        destination.session_overrides = source.session_overrides;
        destination.actions = source.actions;
        destination.transaction.active = source.transaction.active;
        destination.transaction.start = source.transaction.start;
        destination.transaction.session = source.transaction.session;
        destination.transaction.start_overrides = source.transaction.start_overrides;
        destination.transaction.session_overrides = source.transaction.session_overrides;
        destination.transaction.start_actions = source.transaction.start_actions;
        destination.transaction.savepoints.fill(None);
        let count = source.transaction.savepoint_count;
        destination.transaction.savepoints[..count]
            .clone_from_slice(&source.transaction.savepoints[..count]);
        destination.transaction.savepoint_count = count;
        target.transaction_mode = self.transaction_mode;
        target.authenticated_user.set(self.authenticated_user.get());
        target.random.set(self.random.get());
        target.seq_session.discard();
        for (source, destination) in self
            .seq_session
            .currvals
            .iter()
            .zip(&target.seq_session.currvals)
        {
            destination.set(source.get());
        }
        for (source, destination) in self
            .seq_session
            .caches
            .iter()
            .zip(&target.seq_session.caches)
        {
            destination.set(source.get());
        }
        target
            .seq_session
            .lastval
            .set(self.seq_session.lastval.get());
        if let Some(source) = &self.seq_session.overflow_currvals {
            let destination = target
                .seq_session
                .overflow_currvals
                .as_ref()
                .filter(|destination| destination.len() >= source.len())
                .ok_or_else(|| {
                    sql_err!(
                        sqlstate::PROGRAM_LIMIT_EXCEEDED,
                        "commit context exceeds max_sequences"
                    )
                })?;
            for (source, destination) in source.iter().zip(destination.iter()) {
                destination.set(source.get());
            }
        }
        if let Some(source) = &self.seq_session.overflow_caches {
            let destination = target
                .seq_session
                .overflow_caches
                .as_ref()
                .filter(|destination| destination.len() >= source.len())
                .ok_or_else(|| {
                    sql_err!(
                        sqlstate::PROGRAM_LIMIT_EXCEEDED,
                        "commit context exceeds max_sequences"
                    )
                })?;
            for (source, destination) in source.iter().zip(destination.iter()) {
                destination.set(source.get());
            }
        }
        Ok(())
    }

    /// Restores connection-local state without replacing startup allocations.
    pub(crate) fn reset_session_state(&mut self) {
        let mut sequences = SeqSession::new();
        core::mem::swap(&mut sequences, &mut self.seq_session);
        let mut savepoints = core::mem::take(&mut self.store.get_mut().transaction.savepoints);
        savepoints.fill(None);
        *self = Self::with_savepoints(savepoints, self.transaction_mode);
        core::mem::swap(&mut sequences, &mut self.seq_session);
        self.seq_session.discard();
    }

    #[cfg(test)]
    pub(crate) fn store_savepoint_allocation_for_test(&self) -> usize {
        self.store.borrow().transaction.savepoints.as_ptr() as usize
    }

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
        owned_setting(&self.store.borrow().defaults, name)
    }

    pub fn new() -> Self {
        Self::with_savepoints(
            vec![None; super::txn::DEFAULT_MAX_SAVEPOINTS],
            crate::config::TransactionMode::Mvcc,
        )
    }

    fn with_savepoints(
        savepoints: Vec<Option<GucSavepoint>>,
        transaction_mode: crate::config::TransactionMode,
    ) -> Self {
        let values = GucValues::new();
        Self {
            transaction_mode,
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
                actions: SettingActions::EMPTY,
                transaction: GucTransaction {
                    active: false,
                    start: values,
                    session: values,
                    start_overrides: 0,
                    session_overrides: 0,
                    start_actions: [0; GUC_SETTING_COUNT],
                    savepoints,
                    savepoint_count: 0,
                },
            }),
            authenticated_user: Cell::new(crate::storage::SessionIdentity::bootstrap()),
            seq_session: SeqSession::new(),
            random: Cell::new(PrngState::UNINITIALIZED),
        }
    }

    pub fn search_path(&self) -> StackStr<SEARCH_PATH_BYTES> {
        self.store.borrow().current.search_path
    }

    /// Captures the non-default session values which the planner consumes.
    /// The returned storage is fixed-size and owns its values so rendering can
    /// continue after the GUC borrow ends.
    pub(crate) fn planner_settings(&self) -> PlannerSettings {
        let state = self.store.borrow();
        let current = &state.current;
        let built_in = GucValues::new();
        let mut settings = PlannerSettings::EMPTY;
        if current.search_path != built_in.search_path {
            settings.entries[settings.count] = PlannerSetting {
                name: "search_path",
                value: current.search_path,
            };
            settings.count += 1;
        }
        settings
    }

    pub fn default_tablespace(&self) -> StackStr<64> {
        self.store.borrow().current.default_tablespace
    }

    pub(crate) fn password_encryption(&self) -> PasswordEncryption {
        self.store.borrow().current.password_encryption
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

    fn initialized_random_state(&self) -> Result<PrngState, SqlError> {
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
        Ok(state)
    }

    pub(crate) fn random(&self) -> Result<f64, SqlError> {
        let mut state = self.initialized_random_state()?;
        let value = state.next_f64();
        self.random.set(state);
        Ok(value)
    }

    pub(crate) fn random_u64_range(&self, min: u64, max: u64) -> Result<u64, SqlError> {
        let mut state = self.initialized_random_state()?;
        let value = state.next_u64_range(min, max);
        self.random.set(state);
        Ok(value)
    }

    pub(crate) fn random_i64_range(&self, min: i64, max: i64) -> Result<i64, SqlError> {
        let mut state = self.initialized_random_state()?;
        let value = state.next_i64_range(min, max);
        self.random.set(state);
        Ok(value)
    }

    pub(crate) fn random_normal(&self, mean: f64, stddev: f64) -> Result<f64, SqlError> {
        let mut state = self.initialized_random_state()?;
        let value = stddev * state.next_normal() + mean;
        self.random.set(state);
        Ok(value)
    }

    pub fn session_user(&self) -> StackStr<64> {
        self.store.borrow().current.session_authorization.name()
    }

    pub fn authenticated_user(&self) -> StackStr<64> {
        self.authenticated_user.get().name()
    }

    pub fn set_session_user(&mut self, user: &str) {
        self.bind_session_user(crate::storage::SessionIdentity::unauthenticated(user));
    }

    pub(crate) fn bind_session_user(&mut self, role: crate::storage::SessionIdentity) {
        self.authenticated_user.set(role);
        let mut store = self.store.borrow_mut();
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
        self.store.borrow().current.current_role.name()
    }

    pub(crate) fn set_role(&self, role: crate::storage::SessionIdentity, local: bool) {
        let mut store = self.store.borrow_mut();
        store.current.current_role = role;
        if store.transaction.active && !local {
            store.transaction.session.current_role = role;
        }
    }

    pub fn reset_role(&self, local: bool) {
        let session_user = self.store.borrow().current.session_authorization;
        self.set_role(session_user, local);
    }

    pub(crate) fn set_session_authorization(
        &self,
        role: crate::storage::SessionIdentity,
        local: bool,
    ) {
        let mut store = self.store.borrow_mut();
        store.current.session_authorization = role;
        store.current.current_role = role;
        if store.transaction.active && !local {
            store.transaction.session.session_authorization = role;
            store.transaction.session.current_role = role;
        }
    }

    pub fn reset_session_authorization(&self, local: bool) {
        let authenticated_user = self.authenticated_user.get();
        self.set_session_authorization(authenticated_user, local);
    }

    pub(crate) fn current_identity(&self) -> crate::storage::SessionIdentity {
        self.store.borrow().current.current_role
    }

    pub(crate) fn session_identity(&self) -> crate::storage::SessionIdentity {
        self.store.borrow().current.session_authorization
    }

    pub(crate) fn authenticated_identity(&self) -> crate::storage::SessionIdentity {
        self.authenticated_user.get()
    }

    pub(crate) fn refresh_identities(&self, storage: &crate::storage::Storage, transaction: u32) {
        self.authenticated_user
            .set(self.authenticated_user.get().refresh(storage, transaction));
        let mut store = self.store.borrow_mut();
        store.current.current_role = store.current.current_role.refresh(storage, transaction);
        store.current.session_authorization = store
            .current
            .session_authorization
            .refresh(storage, transaction);
    }

    pub fn statement_timeout_ms(&self) -> u64 {
        u64::try_from(self.store.borrow().current.statement_timeout)
            .expect("stored statement_timeout is validated")
    }

    pub fn lock_timeout_ms(&self) -> u64 {
        u64::try_from(self.store.borrow().current.lock_timeout)
            .expect("stored lock_timeout is validated")
    }

    pub(crate) fn event_triggers(&self) -> bool {
        self.store.borrow().current.event_triggers
    }

    pub(crate) fn track_functions(&self) -> TrackFunctions {
        self.store.borrow().current.track_functions
    }

    pub(crate) fn plpgsql_settings(&self) -> PlpgsqlSettings {
        let current = self.store.borrow().current;
        PlpgsqlSettings {
            variable_conflict: current.plpgsql_variable_conflict,
            print_strict_params: current.plpgsql_print_strict_params,
        }
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
            self.assign(
                "default_transaction_isolation",
                Some(isolation.as_str()),
                false,
            )?;
        }
        if let Some(read_only) = characteristics.read_only {
            self.assign(
                "default_transaction_read_only",
                Some(if read_only { "on" } else { "off" }),
                false,
            )?;
        }
        if let Some(deferrable) = characteristics.deferrable {
            self.assign(
                "default_transaction_deferrable",
                Some(if deferrable { "on" } else { "off" }),
                false,
            )?;
        }
        Ok(())
    }

    pub(crate) fn current_transaction_value(
        &self,
        name: &str,
        value: Option<&str>,
    ) -> Option<Result<TransactionCharacteristics, SqlError>> {
        if !name.eq_ignore_ascii_case("transaction_isolation")
            && !name.eq_ignore_ascii_case("transaction_read_only")
            && !name.eq_ignore_ascii_case("transaction_deferrable")
        {
            return None;
        }
        let mut characteristics = TransactionCharacteristics::EMPTY;
        let Some(value) = value else {
            return Some(Err(sql_err!(
                sqlstate::FEATURE_NOT_SUPPORTED,
                "parameter \"{}\" cannot be reset",
                name
            )));
        };
        if name.eq_ignore_ascii_case("transaction_isolation") {
            let isolation = match parse_transaction_isolation(value) {
                Some(isolation) => isolation,
                None => {
                    return Some(Err(invalid_enum_value(
                        "transaction_isolation",
                        value,
                        "serializable, repeatable read, read committed, read uncommitted",
                    )));
                }
            };
            characteristics.isolation = Some(isolation);
        } else if name.eq_ignore_ascii_case("transaction_read_only") {
            let read_only = match parse_on_off(value) {
                Some(read_only) => read_only,
                None => return Some(Err(invalid_setting_value("transaction_read_only", value))),
            };
            characteristics.read_only = Some(read_only);
        } else if name.eq_ignore_ascii_case("transaction_deferrable") {
            let deferrable = match parse_on_off(value) {
                Some(deferrable) => deferrable,
                None => return Some(Err(invalid_setting_value("transaction_deferrable", value))),
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
            return self.assign("seed", Some("unavailable"), local);
        }
        self.capture_setting_value(name)?;
        let mut state = self.store.borrow_mut();
        let values = state.current;
        finish_setting_change(&mut state, values, guc_bit(name), local, false)
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

    fn assign(&self, name: &str, value: Option<&str>, local: bool) -> Result<(), SqlError> {
        if name.eq_ignore_ascii_case("seed") {
            let Some(value) = value else {
                return Err(sql_err!(
                    sqlstate::FEATURE_NOT_SUPPORTED,
                    "parameter \"seed\" cannot be reset"
                ));
            };
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
        change_setting(&mut values, &state.defaults, name, value)?;
        super::txn::validate_isolation_mode(
            self.transaction_mode,
            values.default_transaction_isolation,
        )?;
        let bit = guc_bit(name);
        let resetting = value.is_none();
        finish_setting_change(&mut state, values, bit, local, resetting)
    }

    pub(crate) fn set_time_zone_sql(&self, raw: &str, local: bool) -> Result<(), SqlError> {
        let value = raw.trim();
        if value.eq_ignore_ascii_case("local") || value.eq_ignore_ascii_case("default") {
            return self.assign("timezone", None, local);
        }
        if let Some(seconds) = time_zone_numeric_offset(value)? {
            return self.set_time_zone_offset(seconds, local);
        }
        self.assign("timezone", Some(value), local)
    }

    pub(crate) fn set_time_zone_interval(
        &self,
        interval: super::types::Interval,
        local: bool,
    ) -> Result<(), SqlError> {
        if interval.months != 0 || interval.days != 0 {
            return Err(invalid_setting_value(
                "TimeZone",
                "interval containing months or days",
            ));
        }
        let seconds = i32::try_from(interval.micros / 1_000_000)
            .map_err(|_| invalid_setting_value("TimeZone", "interval"))?;
        self.set_time_zone_offset(seconds, local)
    }

    fn set_time_zone_offset(&self, seconds_east: i32, local: bool) -> Result<(), SqlError> {
        let mut state = self.store.borrow_mut();
        let mut values = state.current;
        apply_time_zone_offset(&mut values, seconds_east)?;
        finish_setting_change(&mut state, values, GUC_TIMEZONE, local, false)
    }

    pub(crate) fn set_cluster_default(&self, name: &str, raw: &str) -> Result<(), SqlError> {
        let mut state = self.store.borrow_mut();
        let mut cluster = state.cluster_defaults;
        change_setting(&mut cluster, &GucValues::new(), name, Some(raw))?;
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
        change_setting(&mut values, &state.defaults, name, Some(raw))?;
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
        self.assign(name, None, false)
    }

    pub fn reset_all(&self) -> Result<(), SqlError> {
        let mut state = self.store.borrow_mut();
        state.actions.record_session(GUC_ALL)?;
        let values = state.defaults;
        copy_guc_values(&mut state.current, &values, GUC_ALL);
        state.session_overrides = 0;
        if state.transaction.active {
            copy_guc_values(&mut state.transaction.session, &values, GUC_ALL);
            state.transaction.session_overrides = 0;
        }
        Ok(())
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
        state.transaction.start_actions = state.actions.session;
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
        state.actions.session = state.transaction.start_actions;
        state.transaction.active = false;
        state.transaction.savepoint_count = 0;
    }

    pub fn savepoint(&self) {
        let mut state = self.store.borrow_mut();
        if !state.transaction.active {
            return;
        }
        let index = state.transaction.savepoint_count;
        assert!(
            index < state.transaction.savepoints.len(),
            "GUC savepoint capacity mirrors transaction capacity"
        );
        state.transaction.savepoints[index] = Some(GucSavepoint {
            current: state.current,
            session: state.transaction.session,
            current_overrides: state.session_overrides,
            session_overrides: state.transaction.session_overrides,
            session_actions: state.actions.session,
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
        state.actions.session = savepoint.session_actions;
        state.transaction.savepoint_count = index + 1;
    }

    pub(crate) fn publish_rollback_settings(&self, txn: &super::txn::TxnState) {
        for &name in super::SETTING_NAMES {
            if let Some(value) = transaction_setting_owned(txn, name) {
                let reset = self.transaction_reset_owned(name).unwrap_or(value);
                crate::sql::eval::funcs::system::update_session_setting(
                    name,
                    value,
                    reset,
                    txn.setting_source(name).unwrap_or("default"),
                );
            } else if let Some(value) = self.get_owned(name) {
                let reset = self.reset_owned(name).unwrap_or(value);
                crate::sql::eval::funcs::system::update_session_setting(
                    name,
                    value,
                    reset,
                    self.source(name),
                );
            }
        }
        ACTIVE_RENDER.with(|active| *active.borrow_mut() = Some(self.render()));
        crate::sql::timezone::set_session(self.timezone());
    }

    pub fn set_config(
        &self,
        name: &str,
        value: Option<&str>,
        local: bool,
    ) -> Result<StackStr<256>, SqlError> {
        self.assign(name, value, local)?;
        self.get_owned(name).ok_or_else(|| {
            sql_err!(
                sqlstate::UNDEFINED_OBJECT,
                "unrecognized configuration parameter \"{}\"",
                name
            )
        })
    }

    pub(crate) fn check_function_bodies(&self) -> bool {
        self.store.borrow().current.check_function_bodies
    }

    pub(crate) fn capture_setting_value(&self, name: &str) -> Result<StackStr<256>, SqlError> {
        let value = self.get_owned(name).ok_or_else(|| {
            sql_err!(
                sqlstate::UNDEFINED_OBJECT,
                "unrecognized configuration parameter \"{}\"",
                name
            )
        })?;
        self.validate_setting_value(name, Some(value.as_str()))?;
        Ok(value)
    }

    pub(crate) fn validate_system_reset(&self, name: &str) -> Result<(), SqlError> {
        match read_only_parameter_context(name) {
            Some(ReadOnlyParameterContext::Startup) => Ok(()),
            Some(ReadOnlyParameterContext::Internal) | None => {
                self.validate_setting_value(name, None)
            }
        }
    }

    pub(crate) fn validate_setting_value(
        &self,
        name: &str,
        value: Option<&str>,
    ) -> Result<(), SqlError> {
        let state = self.store.borrow();
        let mut values = state.current;
        change_setting(&mut values, &state.defaults, name, value)
    }

    pub(crate) fn enter_routine_configs(
        &self,
        configs: &[crate::storage::RoutineConfig],
    ) -> Result<RoutineConfigScope, SqlError> {
        let mut state = self.store.borrow_mut();
        let prior = state.current;
        let entered = state.actions.next;
        let mut configured = 0;
        let mut current = state.current;
        for config in configs {
            change_setting(
                &mut current,
                &state.defaults,
                config.name.as_str(),
                Some(config.value.as_str()),
            )?;
            configured |= guc_bit(config.name.as_str());
        }
        state.current = current;
        drop(state);
        self.publish_owned_settings();
        Ok(RoutineConfigScope {
            guc: self as *const GucState,
            prior,
            configured,
            entered,
            owner: super::execution_identity(),
        })
    }

    fn publish_owned_settings(&self) {
        // A routine can change settings absent from its SET clause. Publish
        // the whole restored scope before another trigger observes it.
        for &name in super::SETTING_NAMES {
            if let Some(value) = self.get_owned(name) {
                let reset = self.reset_owned(name).unwrap_or(value);
                crate::sql::eval::funcs::system::update_session_setting(
                    name,
                    value,
                    reset,
                    self.source(name),
                );
            }
        }
        ACTIVE_RENDER.with(|active| *active.borrow_mut() = Some(self.render()));
        crate::sql::timezone::set_session(self.timezone());
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
    value: Option<&str>,
) -> Result<(), SqlError> {
    match value {
        Some(value) => apply_setting(values, defaults, name, value),
        None => reset_setting(values, defaults, name),
    }
}

fn reset_setting(values: &mut GucValues, defaults: &GucValues, name: &str) -> Result<(), SqlError> {
    if name.eq_ignore_ascii_case("datestyle") {
        values.datestyle = defaults.datestyle;
    } else if name.eq_ignore_ascii_case("timezone") {
        values.timezone = defaults.timezone;
        values.parsed_timezone = defaults.parsed_timezone;
    } else if name.eq_ignore_ascii_case("lc_monetary") {
        values.lc_monetary = defaults.lc_monetary;
    } else if name.eq_ignore_ascii_case("client_encoding") {
        values.client_encoding = defaults.client_encoding;
    } else if name.eq_ignore_ascii_case("application_name") {
        values.application_name = defaults.application_name;
    } else if name.eq_ignore_ascii_case("search_path") {
        values.search_path = defaults.search_path;
    } else if name.eq_ignore_ascii_case("default_tablespace") {
        values.default_tablespace = defaults.default_tablespace;
    } else if name.eq_ignore_ascii_case("default_text_search_config") {
        values.default_text_search_config = defaults.default_text_search_config;
    } else if name.eq_ignore_ascii_case("client_min_messages") {
        values.client_min_messages = defaults.client_min_messages;
    } else if name.eq_ignore_ascii_case("extra_float_digits") {
        values.extra_float_digits = defaults.extra_float_digits;
    } else if name.eq_ignore_ascii_case("lock_timeout") {
        values.lock_timeout = defaults.lock_timeout;
    } else if name.eq_ignore_ascii_case("statement_timeout") {
        values.statement_timeout = defaults.statement_timeout;
    } else if name.eq_ignore_ascii_case("work_mem") {
        values.work_mem = defaults.work_mem;
    } else if name.eq_ignore_ascii_case("row_security") {
        values.row_security = defaults.row_security;
    } else if name.eq_ignore_ascii_case("event_triggers") {
        values.event_triggers = defaults.event_triggers;
    } else if name.eq_ignore_ascii_case("bytea_output") {
        values.bytea_escape = defaults.bytea_escape;
    } else if name.eq_ignore_ascii_case("check_function_bodies") {
        values.check_function_bodies = defaults.check_function_bodies;
    } else if name.eq_ignore_ascii_case("xmloption") {
        values.xml_document = defaults.xml_document;
    } else if name.eq_ignore_ascii_case("intervalstyle") {
        values.intervalstyle = defaults.intervalstyle;
    } else if name.eq_ignore_ascii_case("default_transaction_isolation") {
        values.default_transaction_isolation = defaults.default_transaction_isolation;
    } else if name.eq_ignore_ascii_case("default_transaction_read_only") {
        values.default_transaction_read_only = defaults.default_transaction_read_only;
    } else if name.eq_ignore_ascii_case("default_transaction_deferrable") {
        values.default_transaction_deferrable = defaults.default_transaction_deferrable;
    } else if name.eq_ignore_ascii_case("password_encryption") {
        values.password_encryption = defaults.password_encryption;
    } else if name.eq_ignore_ascii_case("track_functions") {
        values.track_functions = defaults.track_functions;
    } else if name.eq_ignore_ascii_case("plpgsql.variable_conflict") {
        values.plpgsql_variable_conflict = defaults.plpgsql_variable_conflict;
    } else if name.eq_ignore_ascii_case("plpgsql.print_strict_params") {
        values.plpgsql_print_strict_params = defaults.plpgsql_print_strict_params;
    } else if name.eq_ignore_ascii_case("synchronize_seqscans") {
        // Storage scans are deterministic and never synchronize their starts.
    } else if name.eq_ignore_ascii_case("standard_conforming_strings")
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

fn apply_setting(
    values: &mut GucValues,
    defaults: &GucValues,
    name: &str,
    v: &str,
) -> Result<(), SqlError> {
    if name.eq_ignore_ascii_case("datestyle") {
        let current = parse_full(values.datestyle.as_str());
        let reset = parse_full(defaults.datestyle.as_str());
        let (format, order) = apply_datestyle(current, reset, v)?;
        return store(
            &mut values.datestyle,
            canonical_datestyle(format, order).as_str(),
        );
    }
    if name.eq_ignore_ascii_case("timezone") {
        if let Some(seconds) = time_zone_numeric_offset(v)? {
            return apply_time_zone_offset(values, seconds);
        }
        // UTC, fixed numeric offsets, Etc/GMT±N, and named IANA zones (with
        // DST) are honored; an unknown zone is rejected loudly.
        let timezone = parse_timezone(v).ok_or_else(|| {
            sql_err!(
                crate::sql::eval::sqlstate::INVALID_PARAMETER_VALUE,
                "invalid value for parameter \"TimeZone\": \"{}\"",
                v
            )
        })?;
        store(&mut values.timezone, v)?;
        values.parsed_timezone = timezone;
        return Ok(());
    }
    if name.eq_ignore_ascii_case("client_encoding") {
        // UTF8 is native; SQL_ASCII is byte-pass-through (no conversion), so
        // both are served without transcoding. Any other encoding would
        // require a conversion we do not implement.
        if is_utf8(v) {
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
        if parse_boolean_setting("standard_conforming_strings", v)? {
            return Ok(());
        }
        return Err(sql_err!(
            sqlstate::FEATURE_NOT_SUPPORTED,
            "standard_conforming_strings can only be on (strings always conform)"
        ));
    }
    if name.eq_ignore_ascii_case("check_function_bodies") {
        values.check_function_bodies = parse_boolean_setting("check_function_bodies", v)?;
        return Ok(());
    }
    if name.eq_ignore_ascii_case("xmloption") {
        values.xml_document = if v.eq_ignore_ascii_case("content") {
            false
        } else if v.eq_ignore_ascii_case("document") {
            true
        } else {
            return Err(invalid_enum_value("xmloption", v, "content, document"));
        };
        return Ok(());
    }
    if name.eq_ignore_ascii_case("default_tablespace") {
        return store(&mut values.default_tablespace, v);
    }
    if name.eq_ignore_ascii_case("default_text_search_config") {
        if v.is_empty() {
            return Err(invalid_setting_value("default_text_search_config", v));
        }
        return store(&mut values.default_text_search_config, v);
    }
    if name.eq_ignore_ascii_case("default_table_access_method") {
        if v == "heap" {
            return Ok(());
        }
        return Err(sql_err!(
            sqlstate::FEATURE_NOT_SUPPORTED,
            "table access method \"{}\" is not supported",
            v
        ));
    }
    if name.eq_ignore_ascii_case("intervalstyle") {
        values.intervalstyle = IntervalStyle::parse(v).ok_or_else(|| {
            invalid_enum_value(
                "IntervalStyle",
                v,
                "postgres, postgres_verbose, sql_standard, iso_8601",
            )
        })?;
        return Ok(());
    }
    if name.eq_ignore_ascii_case("lc_monetary") {
        let canonical = if v.eq_ignore_ascii_case("C.UTF-8") || v.eq_ignore_ascii_case("C.utf8") {
            "C.UTF-8"
        } else if v.eq_ignore_ascii_case("C") || v.eq_ignore_ascii_case("POSIX") {
            "C"
        } else if v.eq_ignore_ascii_case("en_US.UTF-8") || v.eq_ignore_ascii_case("en_US.utf8") {
            "en_US.UTF-8"
        } else {
            return Err(sql_err!(
                sqlstate::INVALID_PARAMETER_VALUE,
                "invalid value for parameter \"lc_monetary\": \"{}\"",
                v
            ));
        };
        return store(&mut values.lc_monetary, canonical);
    }
    if name.eq_ignore_ascii_case("synchronize_seqscans") {
        if !parse_boolean_setting("synchronize_seqscans", v)? {
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
        values.client_min_messages = MessageLevel::parse(v).ok_or_else(|| {
            invalid_enum_value(
                "client_min_messages",
                v,
                "debug5, debug4, debug3, debug2, debug1, log, notice, warning, error",
            )
        })?;
        return Ok(());
    }
    if name.eq_ignore_ascii_case("application_name") {
        values.application_name = sanitize_application_name(v)?;
        return Ok(());
    }
    if name.eq_ignore_ascii_case("search_path") {
        validate_search_path(v)?;
        return store(&mut values.search_path, v);
    }
    if name.eq_ignore_ascii_case("extra_float_digits") {
        // Floats already render at their shortest exact round-trip form —
        // the extra_float_digits >= 0 behavior — so the value is validated
        // to PostgreSQL's range and retained for SHOW.
        values.extra_float_digits = parse_int_setting(EXTRA_FLOAT_DIGITS, v)?;
        return Ok(());
    }
    if name.eq_ignore_ascii_case("lock_timeout") {
        values.lock_timeout = parse_int_setting(LOCK_TIMEOUT, v)?;
        return Ok(());
    }
    if name.eq_ignore_ascii_case("statement_timeout") {
        values.statement_timeout = parse_int_setting(STATEMENT_TIMEOUT, v)?;
        return Ok(());
    }
    if name.eq_ignore_ascii_case("work_mem") {
        values.work_mem = parse_int_setting(WORK_MEM, v)?;
        return Ok(());
    }
    if let Some(setting) = [IDLE_IN_TRANSACTION_SESSION_TIMEOUT, TRANSACTION_TIMEOUT]
        .into_iter()
        .find(|setting| setting.name.eq_ignore_ascii_case(name))
    {
        // These timeout mechanisms do not run yet, so only the disabled value
        // is honored; a non-zero value would be a silent no-operator.
        if parse_int_setting(setting, v)? == 0 {
            return Ok(());
        }
        return Err(sql_err!(
            sqlstate::FEATURE_NOT_SUPPORTED,
            "{} is not enforced yet; only 0 (disabled) is accepted",
            setting.name
        ));
    }
    if name.eq_ignore_ascii_case("bytea_output") {
        if v.eq_ignore_ascii_case("hex") {
            values.bytea_escape = false;
            return Ok(());
        }
        if v.eq_ignore_ascii_case("escape") {
            values.bytea_escape = true;
            return Ok(());
        }
        return Err(invalid_enum_value("bytea_output", v, "escape, hex"));
    }
    if name.eq_ignore_ascii_case("row_security") {
        let on = parse_boolean_setting("row_security", v)?;
        return store(&mut values.row_security, if on { "on" } else { "off" });
    }
    if name.eq_ignore_ascii_case("event_triggers") {
        values.event_triggers = parse_boolean_setting("event_triggers", v)?;
        return Ok(());
    }
    if name.eq_ignore_ascii_case("default_transaction_isolation") {
        values.default_transaction_isolation = parse_transaction_isolation(v).ok_or_else(|| {
            invalid_enum_value(
                "default_transaction_isolation",
                v,
                "serializable, repeatable read, read committed, read uncommitted",
            )
        })?;
        return Ok(());
    }
    if name.eq_ignore_ascii_case("default_transaction_read_only") {
        values.default_transaction_read_only =
            parse_boolean_setting("default_transaction_read_only", v)?;
        return Ok(());
    }
    if name.eq_ignore_ascii_case("default_transaction_deferrable") {
        values.default_transaction_deferrable =
            parse_boolean_setting("default_transaction_deferrable", v)?;
        return Ok(());
    }
    if name.eq_ignore_ascii_case("password_encryption") {
        values.password_encryption = if v.eq_ignore_ascii_case("scram-sha-256") {
            PasswordEncryption::ScramSha256
        } else if v.eq_ignore_ascii_case("md5") {
            PasswordEncryption::Md5
        } else {
            return Err(invalid_enum_value(
                "password_encryption",
                v,
                "md5, scram-sha-256",
            ));
        };
        return Ok(());
    }
    if name.eq_ignore_ascii_case("track_functions") {
        values.track_functions = if v.eq_ignore_ascii_case("none") {
            TrackFunctions::None
        } else if v.eq_ignore_ascii_case("pl") {
            TrackFunctions::Pl
        } else if v.eq_ignore_ascii_case("all") {
            TrackFunctions::All
        } else {
            return Err(invalid_enum_value("track_functions", v, "none, pl, all"));
        };
        return Ok(());
    }
    if name.eq_ignore_ascii_case("plpgsql.print_strict_params") {
        values.plpgsql_print_strict_params =
            parse_boolean_setting("plpgsql.print_strict_params", v)?;
        return Ok(());
    }
    if name.eq_ignore_ascii_case("plpgsql.variable_conflict") {
        values.plpgsql_variable_conflict = VariableConflict::parse(v).ok_or_else(|| {
            invalid_enum_value(
                "plpgsql.variable_conflict",
                v,
                "error, use_variable, use_column",
            )
        })?;
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
        owned_setting(&self.store.borrow().current, name)
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
            xml_document: values.xml_document,
            extra_float_digits: i8::try_from(values.extra_float_digits)
                .expect("stored extra_float_digits is validated"),
        }
    }
}

fn owned_setting(values: &GucValues, name: &str) -> Option<StackStr<256>> {
    if name.eq_ignore_ascii_case("standard_conforming_strings") {
        Some(StackStr::from_str("on"))
    } else if name.eq_ignore_ascii_case("datestyle") {
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
        Some(StackStr::from_str(
            show_int_setting(EXTRA_FLOAT_DIGITS, values.extra_float_digits).as_str(),
        ))
    } else if name.eq_ignore_ascii_case("lock_timeout") {
        Some(StackStr::from_str(
            show_int_setting(LOCK_TIMEOUT, values.lock_timeout).as_str(),
        ))
    } else if name.eq_ignore_ascii_case("row_security") {
        Some(StackStr::from_str(values.row_security.as_str()))
    } else if name.eq_ignore_ascii_case("event_triggers") {
        Some(StackStr::from_str(if values.event_triggers {
            "on"
        } else {
            "off"
        }))
    } else if name.eq_ignore_ascii_case("statement_timeout") {
        Some(StackStr::from_str(
            show_int_setting(STATEMENT_TIMEOUT, values.statement_timeout).as_str(),
        ))
    } else if name.eq_ignore_ascii_case("work_mem") {
        Some(StackStr::from_str(
            show_int_setting(WORK_MEM, values.work_mem).as_str(),
        ))
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
        Some(StackStr::from_str(if values.xml_document {
            "document"
        } else {
            "content"
        }))
    } else if name.eq_ignore_ascii_case("default_tablespace") {
        Some(StackStr::from_str(values.default_tablespace.as_str()))
    } else if name.eq_ignore_ascii_case("default_text_search_config") {
        Some(StackStr::from_str(
            values.default_text_search_config.as_str(),
        ))
    } else if name.eq_ignore_ascii_case("default_table_access_method") {
        Some(StackStr::from_str("heap"))
    } else if name.eq_ignore_ascii_case("intervalstyle") {
        Some(StackStr::from_str(values.intervalstyle.as_str()))
    } else if name.eq_ignore_ascii_case("lc_monetary") {
        Some(StackStr::from_str(values.lc_monetary.as_str()))
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
    } else if name.eq_ignore_ascii_case("password_encryption") {
        Some(StackStr::from_str(match values.password_encryption {
            PasswordEncryption::ScramSha256 => "scram-sha-256",
            PasswordEncryption::Md5 => "md5",
        }))
    } else if name.eq_ignore_ascii_case("track_functions") {
        Some(StackStr::from_str(values.track_functions.as_str()))
    } else if name.eq_ignore_ascii_case("plpgsql.variable_conflict") {
        Some(StackStr::from_str(
            values.plpgsql_variable_conflict.as_str(),
        ))
    } else if name.eq_ignore_ascii_case("plpgsql.print_strict_params") {
        Some(StackStr::from_str(if values.plpgsql_print_strict_params {
            "on"
        } else {
            "off"
        }))
    } else if name.eq_ignore_ascii_case("seed") {
        Some(StackStr::from_str("unavailable"))
    } else {
        None
    }
}

/// Parses a PostgreSQL boolean GUC value (on/off/true/false/1/0), allocation-free.
fn parse_on_off(v: &str) -> Option<bool> {
    if v.is_empty() {
        return None;
    }
    let matches = |word: &str| {
        word.get(..v.len())
            .is_some_and(|prefix| prefix.eq_ignore_ascii_case(v))
    };
    let on = ["on", "true", "yes", "1"].into_iter().any(matches);
    let off = ["off", "false", "no", "0"].into_iter().any(matches);
    match (on, off) {
        (true, false) => Some(true),
        (false, true) => Some(false),
        _ => None,
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

/// Base unit of an integer setting, as PostgreSQL's `GUC_UNIT_KB` and
/// `GUC_UNIT_MS` flags define it.
#[derive(Clone, Copy, PartialEq, Eq)]
enum SettingUnit {
    None,
    Kilobytes,
    Milliseconds,
}

/// One integer setting's name, unit and accepted range.
#[derive(Clone, Copy)]
struct IntSetting {
    name: &'static str,
    unit: SettingUnit,
    min: i32,
    max: i32,
}

const WORK_MEM: IntSetting = IntSetting {
    name: "work_mem",
    unit: SettingUnit::Kilobytes,
    min: 64,
    max: i32::MAX,
};
const LOCK_TIMEOUT: IntSetting = IntSetting {
    name: "lock_timeout",
    unit: SettingUnit::Milliseconds,
    min: 0,
    max: i32::MAX,
};
const STATEMENT_TIMEOUT: IntSetting = IntSetting {
    name: "statement_timeout",
    unit: SettingUnit::Milliseconds,
    min: 0,
    max: i32::MAX,
};
const IDLE_IN_TRANSACTION_SESSION_TIMEOUT: IntSetting = IntSetting {
    name: "idle_in_transaction_session_timeout",
    unit: SettingUnit::Milliseconds,
    min: 0,
    max: i32::MAX,
};
const TRANSACTION_TIMEOUT: IntSetting = IntSetting {
    name: "transaction_timeout",
    unit: SettingUnit::Milliseconds,
    min: 0,
    max: i32::MAX,
};
const EXTRA_FLOAT_DIGITS: IntSetting = IntSetting {
    name: "extra_float_digits",
    unit: SettingUnit::None,
    min: -15,
    max: 3,
};

/// Unit names and multipliers into each base unit, largest first, as in
/// PostgreSQL's `memory_unit_conversion_table` and
/// `time_unit_conversion_table`.
/// https://github.com/postgres/postgres/blob/REL_18_6/src/backend/utils/misc/guc.c#L1022
const KILOBYTE_UNITS: [(&str, f64); 5] = [
    ("TB", 1024.0 * 1024.0 * 1024.0),
    ("GB", 1024.0 * 1024.0),
    ("MB", 1024.0),
    ("kB", 1.0),
    ("B", 1.0 / 1024.0),
];
const MILLISECOND_UNITS: [(&str, f64); 6] = [
    ("d", 86_400_000.0),
    ("h", 3_600_000.0),
    ("min", 60_000.0),
    ("s", 1000.0),
    ("ms", 1.0),
    ("us", 1.0 / 1000.0),
];

impl SettingUnit {
    fn table(self) -> &'static [(&'static str, f64)] {
        match self {
            Self::None => &[],
            Self::Kilobytes => &KILOBYTE_UNITS,
            Self::Milliseconds => &MILLISECOND_UNITS,
        }
    }

    fn base_name(self) -> &'static str {
        match self {
            Self::None => "",
            Self::Kilobytes => " kB",
            Self::Milliseconds => " ms",
        }
    }

    fn hint(self) -> &'static str {
        match self {
            Self::None => "",
            Self::Kilobytes => {
                "Valid units for this parameter are \"B\", \"kB\", \"MB\", \"GB\", and \"TB\"."
            }
            Self::Milliseconds => {
                "Valid units for this parameter are \"us\", \"ms\", \"s\", \"min\", \"h\", and \"d\"."
            }
        }
    }
}

/// Parses an integer setting as PostgreSQL's `parse_int` and
/// `parse_and_validate_value` do: decimal, octal or hexadecimal integers,
/// fractional or exponent values rounded to the nearest integer, and an
/// optional case-sensitive unit converted to the base unit.
/// https://github.com/postgres/postgres/blob/REL_18_6/src/backend/utils/misc/guc.c#L2780
fn parse_int_setting(setting: IntSetting, value: &str) -> Result<i32, SqlError> {
    let invalid = |hint: &'static str| {
        if !hint.is_empty() {
            super::eval::stash_diagnostic(None, Some(StackStr::from_str(hint)));
        }
        invalid_setting_value(setting.name, value)
    };
    let bytes = value.as_bytes();
    let start = bytes
        .iter()
        .position(|byte| !byte.is_ascii_whitespace())
        .unwrap_or(bytes.len());
    let (integer, integer_end) = parse_c_long(value, start);
    let mut number;
    let mut end;
    let reparse = integer_end.is_none_or(|end| matches!(bytes.get(end), Some(b'.' | b'e' | b'E')));
    if reparse {
        let (parsed, parsed_end) = parse_c_double(value, start).ok_or_else(|| invalid(""))?;
        number = parsed;
        end = parsed_end;
    } else {
        number = integer as f64;
        end = integer_end.expect("integer parse consumed digits");
    }
    if number.is_nan() {
        return Err(invalid(""));
    }
    while bytes.get(end).is_some_and(u8::is_ascii_whitespace) {
        end += 1;
    }
    if end < bytes.len() {
        if setting.unit == SettingUnit::None {
            return Err(invalid(""));
        }
        let rest = &value[end..];
        let unit_length = rest
            .bytes()
            .position(|byte| byte.is_ascii_whitespace())
            .unwrap_or(rest.len());
        let (unit, trailing) = rest.split_at(unit_length);
        let table = setting.unit.table();
        let index = (unit_length <= 9 && trailing.trim_start().is_empty())
            .then(|| table.iter().position(|(name, _)| *name == unit))
            .flatten()
            .ok_or_else(|| invalid(setting.unit.hint()))?;
        number *= table[index].1;
        // A fractional value rounds to the next smaller unit, if there is one.
        if let Some(&(_, smaller)) = table.get(index + 1) {
            number = (number / smaller).round_ties_even() * smaller;
        }
    }
    let number = number.round_ties_even();
    if !(f64::from(i32::MIN)..=f64::from(i32::MAX)).contains(&number) {
        return Err(invalid("Value exceeds integer range."));
    }
    let number = number as i32;
    if !(setting.min..=setting.max).contains(&number) {
        let unit = setting.unit.base_name();
        return Err(sql_err!(
            sqlstate::INVALID_PARAMETER_VALUE,
            "{}{} is outside the valid range for parameter \"{}\" ({}{} .. {}{})",
            number,
            unit,
            setting.name,
            setting.min,
            unit,
            setting.max,
            unit
        ));
    }
    Ok(number)
}

/// Parses C `strtol(value, &end, 0)` from `start`: optional sign, then a
/// `0x` hexadecimal, `0` octal or decimal integer. Returns the end offset,
/// or None when no digits were read or the value overflows.
fn parse_c_long(value: &str, start: usize) -> (i64, Option<usize>) {
    let bytes = value.as_bytes();
    let mut index = start;
    let negative = match bytes.get(index) {
        Some(b'-') => {
            index += 1;
            true
        }
        Some(b'+') => {
            index += 1;
            false
        }
        _ => false,
    };
    let mut radix = 10;
    if bytes.get(index) == Some(&b'0') {
        if matches!(bytes.get(index + 1), Some(b'x' | b'X'))
            && bytes.get(index + 2).is_some_and(u8::is_ascii_hexdigit)
        {
            radix = 16;
            index += 2;
        } else {
            radix = 8;
        }
    }
    let digits = index;
    let mut magnitude: i64 = 0;
    let mut overflow = false;
    while let Some(digit) = bytes
        .get(index)
        .and_then(|byte| (*byte as char).to_digit(radix))
    {
        match magnitude
            .checked_mul(i64::from(radix))
            .and_then(|value| value.checked_add(i64::from(digit)))
        {
            Some(value) => magnitude = value,
            None => overflow = true,
        }
        index += 1;
    }
    if index == digits || overflow {
        return (0, None);
    }
    (if negative { -magnitude } else { magnitude }, Some(index))
}

/// Parses the longest C `strtod` decimal prefix from `start`, returning the
/// value and its end offset. None when no number was read or it overflows.
fn parse_c_double(value: &str, start: usize) -> Option<(f64, usize)> {
    let bytes = value.as_bytes();
    let mut index = start;
    if matches!(bytes.get(index), Some(b'+' | b'-')) {
        index += 1;
    }
    let mantissa = index;
    while bytes.get(index).is_some_and(u8::is_ascii_digit) {
        index += 1;
    }
    let mut digits = index - mantissa;
    if bytes.get(index) == Some(&b'.') {
        let fraction = index + 1;
        let mut end = fraction;
        while bytes.get(end).is_some_and(u8::is_ascii_digit) {
            end += 1;
        }
        digits += end - fraction;
        if digits > 0 {
            index = end;
        }
    }
    if digits == 0 {
        return None;
    }
    if matches!(bytes.get(index), Some(b'e' | b'E')) {
        let mut exponent = index + 1;
        if matches!(bytes.get(exponent), Some(b'+' | b'-')) {
            exponent += 1;
        }
        if bytes.get(exponent).is_some_and(u8::is_ascii_digit) {
            while bytes.get(exponent).is_some_and(u8::is_ascii_digit) {
                exponent += 1;
            }
            index = exponent;
        }
    }
    let number: f64 = value[start..index].parse().ok()?;
    number.is_finite().then_some((number, index))
}

/// Shows an integer setting as PostgreSQL's `ShowGUCOption` does: a positive
/// value uses the largest unit that divides it evenly.
fn show_int_setting(setting: IntSetting, value: i32) -> StackStr<24> {
    let mut shown = StackStr::new();
    if value <= 0 || setting.unit == SettingUnit::None {
        let _ = write!(shown, "{value}");
        return shown;
    }
    let base = i64::from(value);
    for &(name, multiplier) in setting.unit.table() {
        if multiplier <= 1.0 || base % multiplier as i64 == 0 {
            let _ = write!(
                shown,
                "{}{name}",
                (base as f64 / multiplier).round_ties_even() as i64
            );
            break;
        }
    }
    shown
}

/// The base-unit integer `pg_settings.setting` reports for a shown integer
/// setting, or None for settings without a unit.
pub(crate) fn int_setting_base_value(name: &str, shown: &str) -> Option<i32> {
    let setting = int_setting(name).filter(|setting| setting.unit != SettingUnit::None)?;
    Some(parse_int_setting(setting, shown).expect("shown integer settings parse"))
}

/// The unit, minimum and maximum `pg_settings` reports for an integer setting.
pub(crate) fn int_setting_metadata(name: &str) -> Option<(&'static str, i32, i32)> {
    let setting = int_setting(name)?;
    Some((
        setting.unit.base_name().trim_start(),
        setting.min,
        setting.max,
    ))
}

fn int_setting(name: &str) -> Option<IntSetting> {
    [
        WORK_MEM,
        LOCK_TIMEOUT,
        STATEMENT_TIMEOUT,
        IDLE_IN_TRANSACTION_SESSION_TIMEOUT,
        TRANSACTION_TIMEOUT,
        EXTRA_FLOAT_DIGITS,
    ]
    .into_iter()
    .find(|setting| setting.name.eq_ignore_ascii_case(name))
}

// PostgreSQL escapes each non-printable UTF-8 byte, preserving ASCII backslashes.
// https://github.com/postgres/postgres/blob/REL_18_6/src/common/string.c#L81
fn sanitize_application_name(value: &str) -> Result<StackStr<64>, SqlError> {
    let mut clean = StackStr::new();
    for byte in value.bytes() {
        if byte == 0 {
            return Err(sql_err!(
                sqlstate::CHARACTER_NOT_IN_REPERTOIRE,
                "invalid byte sequence for encoding \"UTF8\": 0x00"
            ));
        }
        if (b' '..=b'~').contains(&byte) {
            let _ = clean.write_char(char::from(byte));
        } else {
            let _ = write!(clean, "\\x{byte:02x}");
        }
    }
    if clean.is_truncated() {
        return Err(sql_err!(
            sqlstate::INVALID_PARAMETER_VALUE,
            "application_name exceeds 64 bytes after escaping"
        ));
    }
    Ok(clean)
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
    let (style, order) = s
        .split_once(", ")
        .expect("stored DateStyle has both canonical components");
    let style = match style {
        "ISO" => DateFormat::Iso,
        "SQL" => DateFormat::Sql,
        "Postgres" => DateFormat::Postgres,
        "German" => DateFormat::German,
        _ => panic!("stored DateStyle has a validated output style"),
    };
    let order = match order {
        "MDY" => Order3::Mdy,
        "DMY" => Order3::Dmy,
        "YMD" => Order3::Ymd,
        _ => panic!("stored DateStyle has a validated field order"),
    };
    (style, order)
}

/// PostgreSQL's check_datestyle applies DEFAULT only to components not yet
/// explicitly selected. German implies DMY at its own position in the list;
/// DEFAULT after German can therefore replace that implied order.
fn apply_datestyle(
    current: (DateFormat, Order3),
    reset: (DateFormat, Order3),
    value: &str,
) -> Result<(DateFormat, Order3), SqlError> {
    use core::fmt::Write as _;
    let (mut format, mut order) = current;
    let (mut have_style, mut have_order) = (false, false);
    let mut conflict = false;
    for component in search_path_components(value) {
        let component = component.map_err(|_| invalid_setting_value("DateStyle", value))?;
        let component = component.trim_matches(search_path_space);
        let mut token = StackStr::<63>::new();
        if let Some(quoted) = component.strip_prefix('"') {
            let inner = quoted
                .strip_suffix('"')
                .expect("validated identifier has paired quotes");
            let mut characters = inner.chars().peekable();
            while let Some(character) = characters.next() {
                let _ = token.write_char(character);
                if token.is_truncated() {
                    break;
                }
                if character == '"' {
                    characters.next();
                }
            }
        } else {
            let _ = token.write_str(component);
        }
        let token = token.as_str();
        let starts = |prefix: &str| {
            token
                .get(..prefix.len())
                .is_some_and(|part| part.eq_ignore_ascii_case(prefix))
        };
        let selected_style = if token.eq_ignore_ascii_case("iso") {
            Some(DateFormat::Iso)
        } else if token.eq_ignore_ascii_case("sql") {
            Some(DateFormat::Sql)
        } else if starts("postgres") {
            Some(DateFormat::Postgres)
        } else if token.eq_ignore_ascii_case("german") {
            Some(DateFormat::German)
        } else {
            None
        };
        if let Some(selected) = selected_style {
            conflict |= have_style && format != selected;
            format = selected;
            have_style = true;
            if selected == DateFormat::German && !have_order {
                order = Order3::Dmy;
            }
            continue;
        }
        let selected_order = if token.eq_ignore_ascii_case("ymd") {
            Some(Order3::Ymd)
        } else if token.eq_ignore_ascii_case("dmy") || starts("euro") {
            Some(Order3::Dmy)
        } else if token.eq_ignore_ascii_case("mdy")
            || token.eq_ignore_ascii_case("us")
            || starts("noneuro")
        {
            Some(Order3::Mdy)
        } else {
            None
        };
        if let Some(selected) = selected_order {
            conflict |= have_order && order != selected;
            order = selected;
            have_order = true;
        } else if token.eq_ignore_ascii_case("default") {
            if !have_style {
                format = reset.0;
            }
            if !have_order {
                order = reset.1;
            }
        } else {
            return Err(invalid_setting_value("DateStyle", value));
        }
    }
    if conflict {
        return Err(invalid_setting_value("DateStyle", value));
    }
    Ok((format, order))
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

/// Catalog settings use PostgreSQL's canonical parameter spelling.
pub(crate) fn canonical_setting_name(name: &str) -> Result<crate::storage::SqlName, SqlError> {
    let validated = crate::storage::SqlName::parse(name)?;
    let special = if name.eq_ignore_ascii_case("datestyle") {
        Some("DateStyle")
    } else if name.eq_ignore_ascii_case("timezone") {
        Some("TimeZone")
    } else if name.eq_ignore_ascii_case("intervalstyle") {
        Some("IntervalStyle")
    } else {
        None
    };
    if let Some(special) = special {
        return crate::storage::SqlName::parse(special);
    }
    let mut normalized = StackStr::<63>::new();
    for character in validated.as_str().chars() {
        normalized
            .write_char(character.to_ascii_lowercase())
            .expect("ASCII case folding preserves the validated name length");
    }
    crate::storage::SqlName::parse(normalized.as_str())
}

fn is_utc(v: &str) -> bool {
    v.eq_ignore_ascii_case("utc")
        || v.eq_ignore_ascii_case("gmt")
        || v.eq_ignore_ascii_case("etc/utc")
        || v.eq_ignore_ascii_case("universal")
}

// PostgreSQL localtime.c accepts offsets through 167:59:59.
const TIME_ZONE_MAX_OFFSET_SECONDS: i32 = 167 * 3600 + 59 * 60 + 59;

// Decimal GUC values denote hours east of UTC, including quoted strings.
// Canonical POSIX names are parsed separately from numeric input.
// Bare colon-separated offsets instead follow the timezone parser's POSIX sign.
fn time_zone_numeric_offset(value: &str) -> Result<Option<i32>, SqlError> {
    let text = value.trim();
    if let Some((east, west)) = text.strip_prefix('<').and_then(|text| text.split_once('>')) {
        if let Some(seconds) = parse_hms(east, 167)
            && super::datetime::iso_offset_string(seconds).as_str() == east
            && (if seconds == 0 {
                west == "-00"
            } else {
                super::datetime::iso_offset_string(-seconds).as_str() == west
            })
        {
            // PostgreSQL rejects second-bearing named zones as leap-second
            // zones, although its direct numeric-offset path accepts them.
            if seconds % 60 != 0 {
                return Err(invalid_setting_value("TimeZone", value));
            }
            return Ok(Some(seconds));
        }
        return Ok(None);
    }
    let Ok(hours) = text.parse::<f64>() else {
        return Ok(None);
    };
    if hours.is_nan() {
        return Err(sql_err!(
            sqlstate::FEATURE_NOT_SUPPORTED,
            "NaN time zone offsets are not supported"
        ));
    }
    // PostgreSQL converts hours to whole seconds by truncating toward zero.
    let seconds = (hours * 3600.0).trunc();
    if !seconds.is_finite()
        || !(-f64::from(TIME_ZONE_MAX_OFFSET_SECONDS)..=f64::from(TIME_ZONE_MAX_OFFSET_SECONDS))
            .contains(&seconds)
    {
        return Err(invalid_setting_value("TimeZone", value));
    }
    Ok(Some(seconds as i32))
}

fn apply_time_zone_offset(values: &mut GucValues, seconds_east: i32) -> Result<(), SqlError> {
    if !(-TIME_ZONE_MAX_OFFSET_SECONDS..=TIME_ZONE_MAX_OFFSET_SECONDS).contains(&seconds_east) {
        return Err(invalid_setting_value("TimeZone", "UTC offset"));
    }
    let east = super::datetime::iso_offset_string(seconds_east);
    let west = super::datetime::iso_offset_string(-seconds_east);
    let display = crate::stack_format!(
        32,
        "<{}>{}",
        east.as_str(),
        if seconds_east == 0 {
            "-00"
        } else {
            west.as_str()
        }
    );
    store(&mut values.timezone, display.as_str())?;
    values.parsed_timezone = super::timezone::Timezone::fixed(seconds_east, "", false);
    Ok(())
}

/// Parses timezone names and POSIX offsets; datetime input also recognizes
/// abbreviations separately. GUC decimal hours are handled before this parser.
/// `Etc/GMT+5` is UTC-5 and `+05:30` is UTC-5:30.
pub fn parse_timezone(v: &str) -> Option<super::timezone::Timezone> {
    use super::timezone::Timezone;
    let t = v.trim();
    if is_utc(t) || t.eq_ignore_ascii_case("z") || t.eq_ignore_ascii_case("zulu") {
        return Some(Timezone::utc());
    }
    // Session settings select installed IANA zones, including their history.
    if let Some(timezone) = super::timezone::lookup(t) {
        return Some(timezone);
    }
    // Etc/GMT±N and GMT±N: the sign is inverted; the abbreviation PostgreSQL
    // shows is the resulting ISO offset (e.g. Etc/GMT+5 -> "-05").
    let etc = if t
        .get(..7)
        .is_some_and(|prefix| prefix.eq_ignore_ascii_case("etc/gmt"))
    {
        Some(&t[7..])
    } else if t
        .get(..3)
        .is_some_and(|prefix| prefix.eq_ignore_ascii_case("gmt"))
    {
        Some(&t[3..])
    } else {
        None
    };
    if let Some(rest) = etc {
        if rest.is_empty() {
            return Some(Timezone::utc());
        }
        let offset = -parse_hms(rest, 24)?;
        return Some(Timezone::fixed(
            offset,
            super::datetime::iso_offset_string(offset).as_str(),
            false,
        ));
    }
    // Bare numeric offset: POSIX inverted sign, no abbreviation shown.
    if t.starts_with('+') || t.starts_with('-') {
        return Some(Timezone::fixed(-parse_hms(t, 24)?, "", false));
    }
    None
}

/// Parses `±HH[:MM[:SS]]` to signed seconds (sign as written, not inverted).
fn parse_hms(s: &str, max_hours: i32) -> Option<i32> {
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
        || !(0..=max_hours).contains(&hh)
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

pub(crate) fn search_path_space(character: char) -> bool {
    matches!(character, ' ' | '\t' | '\n' | '\r' | '\x0b' | '\x0c')
}

/// Splits the decoded GUC value, preserving its spelling for SHOW and saved
/// settings. Quoting and separators follow PostgreSQL SplitIdentifierString.
pub(crate) fn search_path_components(value: &str) -> impl Iterator<Item = Result<&str, SqlError>> {
    let mut remaining = Some(value);
    let mut after_comma = false;
    core::iter::from_fn(move || {
        let raw = remaining.take()?;
        let bytes = raw.as_bytes();
        let mut at = 0;
        while at < bytes.len() && search_path_space(char::from(bytes[at])) {
            at += 1;
        }
        let invalid = || {
            sql_err!(
                sqlstate::INVALID_PARAMETER_VALUE,
                "invalid value for parameter \"search_path\": \"{}\"; list syntax is invalid",
                value
            )
        };
        if at == bytes.len() {
            return after_comma.then(|| Err(invalid()));
        }
        if bytes[at] == b'"' {
            at += 1;
            loop {
                if at == bytes.len() || bytes[at] == 0 {
                    return Some(Err(invalid()));
                }
                if bytes[at] == b'"' {
                    at += 1;
                    if bytes.get(at) != Some(&b'"') {
                        break;
                    }
                }
                at += 1;
            }
        } else {
            let start = at;
            while at < bytes.len() && bytes[at] != b',' && !search_path_space(char::from(bytes[at]))
            {
                if bytes[at] == 0 {
                    return Some(Err(invalid()));
                }
                at += 1;
            }
            if at == start {
                return Some(Err(invalid()));
            }
        }
        while at < bytes.len() && search_path_space(char::from(bytes[at])) {
            at += 1;
        }
        if at < bytes.len() {
            if bytes[at] != b',' {
                return Some(Err(invalid()));
            }
            remaining = Some(&raw[at + 1..]);
            after_comma = true;
        }
        Some(Ok(&raw[..at]))
    })
}

pub(crate) fn validate_search_path(value: &str) -> Result<(), SqlError> {
    if value.len() > SEARCH_PATH_BYTES {
        return Err(sql_err!(
            sqlstate::INVALID_PARAMETER_VALUE,
            "search_path is too long (limit {} bytes)",
            SEARCH_PATH_BYTES
        ));
    }
    for component in search_path_components(value) {
        component?;
    }
    Ok(())
}

#[derive(Clone, Copy)]
enum ReadOnlyParameterContext {
    Startup,
    Internal,
}

fn read_only_parameter_context(name: &str) -> Option<ReadOnlyParameterContext> {
    if [
        "max_connections",
        "max_locks_per_transaction",
        "max_prepared_transactions",
    ]
    .iter()
    .any(|parameter| name.eq_ignore_ascii_case(parameter))
    {
        return Some(ReadOnlyParameterContext::Startup);
    }
    if [
        "server_version",
        "server_version_num",
        "server_encoding",
        "is_superuser",
        "integer_datetimes",
        "data_directory_mode",
        "in_hot_standby",
    ]
    .iter()
    .any(|parameter| name.eq_ignore_ascii_case(parameter))
    {
        return Some(ReadOnlyParameterContext::Internal);
    }
    None
}

fn is_read_only(name: &str) -> bool {
    read_only_parameter_context(name).is_some()
}

/// An enumerated setting's rejection, with PostgreSQL's list of its values.
fn invalid_enum_value(param: &str, v: &str, available: &str) -> SqlError {
    crate::sql::eval::stash_diagnostic(
        None,
        Some(crate::stack_format!(
            128,
            "Available values: {}.",
            available
        )),
    );
    invalid_setting_value(param, v)
}

/// A boolean setting's value, rejected as PostgreSQL's `parse_bool` does.
fn parse_boolean_setting(param: &str, v: &str) -> Result<bool, SqlError> {
    parse_on_off(v).ok_or_else(|| {
        sql_err!(
            sqlstate::INVALID_PARAMETER_VALUE,
            "parameter \"{}\" requires a Boolean value",
            param
        )
    })
}

fn invalid_setting_value(param: &str, v: &str) -> SqlError {
    sql_err!(
        sqlstate::INVALID_PARAMETER_VALUE,
        "invalid value for parameter \"{}\": \"{}\"",
        param,
        v
    )
}

#[cfg(test)]
mod ownership_tests {
    use super::*;

    #[test]
    fn routine_exit_keeps_unlisted_local_settings_and_publishes_restored_values() {
        assert_eq!(
            super::super::SETTING_NAMES
                .iter()
                .fold(0, |mask, name| mask | guc_bit(name)),
            GUC_ALL
        );
        let guc = GucState::new();
        let mut budget = Budget::new(
            super::super::txn::TxnState::budget_bytes_with_large_objects(1, 1, 1) + 128,
        );
        let mut txn =
            super::super::txn::TxnState::new_with_large_objects(&mut budget, 1, 1, 1).unwrap();
        let arena =
            crate::mem::Arena::new(&mut budget, "setting-dependent binary output", 128).unwrap();
        let _session = crate::sql::eval::funcs::system::retain_session_evaluation();
        let _evaluation = enter_eval_scope(&guc, &mut txn);
        let configs = [crate::storage::RoutineConfig {
            name: crate::storage::SqlName::parse("application_name").unwrap(),
            value: StackStr::from_str("configured"),
        }];
        crate::mem::guard::forbid_alloc(|| {
            let names = ["application_name", "bytea_output", "client_min_messages"];
            let initial = [
                StackStr::from_str("outer"),
                StackStr::from_str("hex"),
                StackStr::from_str("notice"),
            ];
            crate::sql::eval::funcs::system::set_session_settings(
                &names,
                &initial,
                &initial,
                &["session"; 3],
            )
            .unwrap();
            guc.begin_transaction();
            guc.set_config("application_name", Some("outer"), false)
                .unwrap();
            {
                let _scope = guc.enter_routine_configs(&configs).unwrap();
                guc.set_config("bytea_output", Some("escape"), true)
                    .unwrap();
                publish_active_setting(&guc, "bytea_output");
                guc.set_config("client_min_messages", Some("warning"), false)
                    .unwrap();
                publish_active_setting(&guc, "client_min_messages");
                assert_eq!(
                    crate::sql::eval::funcs::system::session_setting("bytea_output")
                        .unwrap()
                        .as_str(),
                    "escape"
                );
                assert!(active_render().unwrap().bytea_escape);
                assert_eq!(
                    crate::sql::eval::cast_to_text(
                        crate::sql::types::Datum::Bytea(&[0, b'\\', 127, 255]),
                        &arena,
                    )
                    .unwrap(),
                    r"\000\\\177\377"
                );
            }
            for (name, expected) in names.into_iter().zip(["outer", "escape", "warning"]) {
                assert_eq!(guc.get_owned(name).unwrap().as_str(), expected);
                assert_eq!(
                    crate::sql::eval::funcs::system::session_setting(name)
                        .unwrap()
                        .as_str(),
                    expected
                );
            }
            assert!(active_render().unwrap().bytea_escape);
            guc.set_config("bytea_output", Some("hex"), true).unwrap();
            publish_active_setting(&guc, "bytea_output");
            assert_eq!(
                crate::sql::eval::cast_to_text(
                    crate::sql::types::Datum::Bytea(&[0, b'\\', 127, 255]),
                    &arena,
                )
                .unwrap(),
                r"\x005c7fff"
            );
        });
    }

    #[test]
    fn saved_routine_settings_restore_after_worker_reuse_and_repeated_pauses() {
        use crate::mem::lease_pool::{LeasePool, Purpose};

        let guc = GucState::new();
        let other = GucState::new();
        let mut budget = Budget::new(
            2 * super::super::txn::TxnState::budget_bytes_with_large_objects(1, 1, 1)
                + LeasePool::budget_bytes(16 << 10, 4),
        );
        let mut txn =
            super::super::txn::TxnState::new_with_large_objects(&mut budget, 1, 1, 1).unwrap();
        let mut other_txn =
            super::super::txn::TxnState::new_with_large_objects(&mut budget, 1, 1, 1).unwrap();
        let controls =
            LeasePool::new(&mut budget, Purpose::EXECUTION_CONTROL, 16 << 10, 4).unwrap();
        let _session = crate::sql::eval::funcs::system::retain_session_evaluation();
        let _evaluation = enter_eval_scope(&guc, &mut txn);
        let configs = [crate::storage::RoutineConfig {
            name: crate::storage::SqlName::parse("application_name").unwrap(),
            value: StackStr::from_str("configured"),
        }];
        crate::mem::guard::forbid_alloc(|| {
            let names = ["application_name", "bytea_output", "client_min_messages"];
            let initial = [
                StackStr::from_str("outer"),
                StackStr::from_str("hex"),
                StackStr::from_str("notice"),
            ];
            crate::sql::eval::funcs::system::set_session_settings(
                &names,
                &initial,
                &initial,
                &["session"; 3],
            )
            .unwrap();
            guc.begin_transaction();
            guc.set_config("application_name", Some("outer"), false)
                .unwrap();
            let mut saved = {
                let scope = guc.enter_routine_configs(&configs).unwrap();
                guc.set_config("bytea_output", Some("escape"), true)
                    .unwrap();
                publish_active_setting(&guc, "bytea_output");
                guc.set_config("client_min_messages", Some("warning"), false)
                    .unwrap();
                publish_active_setting(&guc, "client_min_messages");
                let held = controls.lease::<u8>(16 << 10).unwrap();
                let image = scope.save().unwrap();
                assert!(controls.copy_slice(&[image]).is_err());
                assert_eq!(
                    guc.get_owned("application_name").unwrap().as_str(),
                    "configured"
                );
                assert!(active_render().unwrap().bytea_escape);
                drop(held);
                controls.copy_slice(&[image]).unwrap()
            };
            assert_eq!(guc.get_owned("application_name").unwrap().as_str(), "outer");
            assert!(active_render().unwrap().bytea_escape);
            // Recycling the worker's published values cannot change the image.
            crate::sql::eval::funcs::system::set_session_settings(
                &names,
                &[StackStr::from_str("other connection"); 3],
                &initial,
                &["session"; 3],
            )
            .unwrap();
            {
                let prior_owner = super::super::execution_identity();
                super::super::set_execution_connection_id(
                    prior_owner.connection_id.wrapping_add(1),
                );
                let _other_evaluation = enter_eval_scope(&other, &mut other_txn);
                other
                    .set_config("application_name", Some("other connection"), false)
                    .unwrap();
                assert_eq!(
                    saved[0].enter().err().unwrap().sqlstate,
                    sqlstate::SERIALIZATION_FAILURE
                );
                assert_eq!(
                    other.get_owned("application_name").unwrap().as_str(),
                    "other connection"
                );
                super::super::publish_execution_identity(prior_owner);
            }
            // Parent settings must still match before any saved value is installed.
            guc.set_config("application_name", Some("unrelated"), true)
                .unwrap();
            assert_eq!(
                saved[0].enter().err().unwrap().sqlstate,
                sqlstate::SERIALIZATION_FAILURE
            );
            assert_eq!(
                guc.get_owned("application_name").unwrap().as_str(),
                "unrelated"
            );
            guc.set_config("application_name", Some("outer"), true)
                .unwrap();
            let mut expected_application = "configured";
            let mut expected_messages = "warning";
            for iteration in 0..3 {
                let scope = saved[0].enter().unwrap();
                assert_eq!(
                    crate::sql::eval::funcs::system::session_setting("application_name")
                        .unwrap()
                        .as_str(),
                    expected_application
                );
                assert_eq!(
                    crate::sql::eval::funcs::system::session_setting("bytea_output")
                        .unwrap()
                        .as_str(),
                    "escape"
                );
                assert!(active_render().unwrap().bytea_escape);
                if iteration == 0 {
                    guc.set_config("application_name", Some("continued"), true)
                        .unwrap();
                    publish_active_setting(&guc, "application_name");
                    expected_application = "continued";
                }
                if iteration == 1 {
                    guc.set_config("client_min_messages", Some("error"), false)
                        .unwrap();
                    publish_active_setting(&guc, "client_min_messages");
                    expected_messages = "error";
                }
                let next = controls.copy_slice(&[scope.save().unwrap()]).unwrap();
                drop(scope);
                saved = next;
                assert_eq!(
                    crate::sql::eval::funcs::system::session_setting("application_name")
                        .unwrap()
                        .as_str(),
                    "outer"
                );
                assert_eq!(
                    crate::sql::eval::funcs::system::session_setting("bytea_output")
                        .unwrap()
                        .as_str(),
                    "escape"
                );
                assert_eq!(
                    guc.get_owned("client_min_messages").unwrap().as_str(),
                    expected_messages
                );
                assert!(active_render().unwrap().bytea_escape);
            }
            drop(other);
            drop(controls);
            let error: Result<(), SqlError> = (|| {
                let _scope = saved[0].enter()?;
                assert!(active_render().unwrap().bytea_escape);
                Err(sql_err!(sqlstate::DIVISION_BY_ZERO, "original error"))
            })();
            assert_eq!(error.unwrap_err().sqlstate, sqlstate::DIVISION_BY_ZERO);
            assert_eq!(
                crate::sql::eval::funcs::system::session_setting("bytea_output")
                    .unwrap()
                    .as_str(),
                "escape"
            );
            assert!(active_render().unwrap().bytea_escape);
        });
    }

    #[test]
    fn routine_setting_actions_preserve_equal_session_sets_and_rollback() {
        let guc = GucState::new();
        let _session = crate::sql::eval::funcs::system::retain_session_evaluation();
        let configs = [crate::storage::RoutineConfig {
            name: crate::storage::SqlName::parse("application_name").unwrap(),
            value: StackStr::from_str("configured"),
        }];
        crate::mem::guard::forbid_alloc(|| {
            guc.begin_transaction();
            guc.set_config("application_name", Some("outer"), false)
                .unwrap();
            {
                let _scope = guc.enter_routine_configs(&configs).unwrap();
                // Equal values are still explicit SET actions, overriding SAVE.
                guc.set_config("application_name", Some("outer"), false)
                    .unwrap();
                guc.set_config("application_name", Some("until commit"), true)
                    .unwrap();
            }
            assert_eq!(
                guc.get_owned("application_name").unwrap().as_str(),
                "until commit"
            );
            guc.commit_transaction();
            assert_eq!(guc.get_owned("application_name").unwrap().as_str(), "outer");

            guc.begin_transaction();
            {
                let _scope = guc.enter_routine_configs(&configs).unwrap();
                guc.savepoint();
                guc.set_config("application_name", Some("outer"), false)
                    .unwrap();
                guc.rollback_to_savepoint(0);
                guc.release_savepoints_from(0);
                assert_eq!(
                    guc.get_owned("application_name").unwrap().as_str(),
                    "configured"
                );
            }
            assert_eq!(guc.get_owned("application_name").unwrap().as_str(), "outer");
            let authorization = crate::storage::SessionIdentity::unauthenticated("session owner");
            let local_role = crate::storage::SessionIdentity::unauthenticated("local role");
            guc.set_session_authorization(authorization, false);
            guc.set_role(local_role, true);
            {
                let _scope = guc.enter_routine_configs(&configs).unwrap();
                guc.reset_all().unwrap();
            }
            assert_eq!(guc.get_owned("application_name").unwrap().as_str(), "");
            assert_eq!(guc.current_identity(), local_role);
            assert_eq!(guc.session_identity(), authorization);
            guc.rollback_transaction();
            assert_eq!(guc.get_owned("application_name").unwrap().as_str(), "outer");

            guc.begin_transaction();
            let mark = guc.statement_settings();
            guc.set_config("application_name", Some("discarded attempt"), false)
                .unwrap();
            guc.restore_statement_settings(mark);
            {
                let _scope = guc.enter_routine_configs(&configs).unwrap();
            }
            assert_eq!(guc.get_owned("application_name").unwrap().as_str(), "outer");
            guc.store.borrow_mut().actions.next = u64::MAX;
            let error = guc
                .set_config("application_name", Some("unpublished"), false)
                .unwrap_err();
            assert_eq!(error.sqlstate, sqlstate::PROGRAM_LIMIT_EXCEEDED);
            assert!(
                error
                    .message
                    .as_str()
                    .contains("session setting assignment counter")
            );
            assert_eq!(guc.get_owned("application_name").unwrap().as_str(), "outer");
            assert_eq!(
                guc.reset_all().unwrap_err().sqlstate,
                sqlstate::PROGRAM_LIMIT_EXCEEDED
            );
            assert_eq!(guc.get_owned("application_name").unwrap().as_str(), "outer");
        });
    }
}

#[cfg(all(test, not(all(miri, feature = "miri-ownership"))))]
mod budget_tests {
    use super::*;

    #[test]
    fn statement_retry_restores_settings_and_the_random_stream() {
        let state = GucState::new();
        state.begin_transaction();
        state
            .set_config("application_name", Some("before"), false)
            .unwrap();
        state.set_random_seed(0.5).unwrap();
        let expected = state.random().unwrap();
        state.set_random_seed(0.5).unwrap();
        crate::mem::guard::forbid_alloc(|| {
            for _ in 0..3 {
                let mark = state.statement_settings();
                assert_eq!(state.random().unwrap(), expected);
                state
                    .set_config("application_name", Some("during"), false)
                    .unwrap();
                state.restore_statement_settings(mark);
                assert_eq!(
                    state.get_owned("application_name").unwrap().as_str(),
                    "before"
                );
            }
            assert_eq!(state.random().unwrap(), expected);
            state.commit_transaction();
            assert_eq!(
                state.get_owned("application_name").unwrap().as_str(),
                "before"
            );
        });
    }

    #[test]
    fn configured_guc_capacities_saturate_and_valid_reservations_remain_exact() {
        let mut config = crate::config::Config::default_dev();
        config.max_sequences = usize::MAX;
        config.max_savepoints_per_transaction = usize::MAX;
        assert_eq!(
            SeqSession::extra_budget_bytes(config.max_sequences),
            usize::MAX
        );
        assert_eq!(
            GucState::extra_savepoint_budget_bytes(config.max_savepoints_per_transaction),
            usize::MAX
        );
        for sequences in [0, INLINE_SEQUENCE_SLOTS, INLINE_SEQUENCE_SLOTS + 3] {
            for savepoints in [0, 1, 3] {
                let bytes = SeqSession::extra_budget_bytes(sequences)
                    + GucState::extra_savepoint_budget_bytes(savepoints);
                let mut budget = Budget::new(bytes);
                let _state = GucState::new_with_capacities(
                    &mut budget,
                    sequences,
                    savepoints,
                    config.transaction_mode,
                )
                .unwrap();
                assert_eq!(budget.remaining(), 0);
            }
        }
    }
}
