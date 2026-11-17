//! Engine calls made while evaluating an index key, kept by the key's site.
//!
//! PostgreSQL computes each index key once per row. A statement that builds
//! or checks keys evaluates them in several layers (tuple sizes, the sorted
//! build, the uniqueness enforcer), and a routine that needs the engine
//! leaves the statement and restarts it. Results are therefore kept by
//! site: the row, the index and the key's position, and the call's ordinal
//! within that key. A later evaluation of the same key reads them without
//! calling again.
//!
//! An attempt does not stop at the first call it lacks: each such call is
//! collected, its value read as NULL, and every later call of the same key
//! (whose arguments may depend on it) also reads NULL without being
//! collected. When the attempt ends, the engine runs the collected calls in
//! their order and the statement restarts, so a build needs as many
//! attempts as its keys nest calls, not one per row.

use super::*;

/// The key whose evaluation is running: its row, index and position, and
/// how many engine calls it made so far.
#[derive(Clone, Copy)]
struct ActiveSite {
    site: KeySite,
    calls: u16,
    /// A call of this key read a placeholder; the rest of its calls wait.
    tainted: bool,
}

/// One key of one row: the row identifier, the index's creation identity
/// and the key position (`PREDICATE` for a partial index's predicate).
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) struct KeySite {
    pub(crate) rowid: u64,
    pub(crate) index: u64,
    pub(crate) position: u8,
}

impl KeySite {
    /// The position naming a partial index's predicate.
    pub(crate) const PREDICATE: u8 = u8::MAX;
}

std::thread_local! {
    static ACTIVE: Cell<Option<ActiveSite>> = const { Cell::new(None) };
    /// The row whose keys are being evaluated.
    static ROW: Cell<Option<u64>> = const { Cell::new(None) };
    /// The index whose keys are being evaluated.
    static INDEX: Cell<Option<u64>> = const { Cell::new(None) };
}

struct RestoreCell(
    &'static std::thread::LocalKey<Cell<Option<u64>>>,
    Option<u64>,
);

impl Drop for RestoreCell {
    fn drop(&mut self) {
        self.0.with(|cell| cell.set(self.1));
    }
}

/// Runs `run` evaluating keys of the row `rowid`.
pub(crate) fn with_key_row<R>(rowid: u64, run: impl FnOnce() -> R) -> R {
    let _restore = RestoreCell(&ROW, ROW.with(|cell| cell.replace(Some(rowid))));
    run()
}

/// An index's identity for its keys' sites: its name, which, unlike its
/// OID, a statement's restarted attempt gives the index again.
pub(crate) fn index_identity(name: &str) -> u64 {
    // FNV-1a.
    name.bytes().fold(0xcbf2_9ce4_8422_2325, |hash, byte| {
        (hash ^ u64::from(byte)).wrapping_mul(0x0100_0000_01b3)
    })
}

/// Runs `run` evaluating keys of the index named `name`.
pub(crate) fn with_key_index<R>(name: &str, run: impl FnOnce() -> R) -> R {
    let _restore = RestoreCell(
        &INDEX,
        INDEX.with(|cell| cell.replace(Some(index_identity(name)))),
    );
    run()
}

/// Evaluating the keys of one row of one index, until dropped. Without an
/// index (a key no index holds) the row's calls are not kept by site.
pub(crate) struct KeyScope {
    _row: RestoreCell,
    _index: RestoreCell,
}

impl KeyScope {
    pub(crate) fn enter(rowid: u64, index: Option<u64>) -> Self {
        Self {
            _row: RestoreCell(&ROW, ROW.with(|cell| cell.replace(Some(rowid)))),
            _index: RestoreCell(&INDEX, INDEX.with(|cell| cell.replace(index))),
        }
    }
}

/// Runs `evaluate`, the key at `position` of the current row and index, at
/// its site when both are known.
pub(crate) fn at_key_position<R>(position: u8, evaluate: impl FnOnce() -> R) -> R {
    match (ROW.with(Cell::get), INDEX.with(Cell::get)) {
        (Some(rowid), Some(index)) => with_key_site(
            KeySite {
                rowid,
                index,
                position,
            },
            evaluate,
        ),
        _ => evaluate(),
    }
}

/// Runs `evaluate`, the evaluation of the key at `site`, so that engine calls
/// it makes are kept by that site.
fn with_key_site<R>(site: KeySite, evaluate: impl FnOnce() -> R) -> R {
    struct Restore(Option<ActiveSite>);
    impl Drop for Restore {
        fn drop(&mut self) {
            ACTIVE.with(|active| active.set(self.0));
        }
    }
    let _restore = Restore(ACTIVE.with(|active| {
        active.replace(Some(ActiveSite {
            site,
            calls: 0,
            tainted: false,
        }))
    }));
    evaluate()
}

/// A kept call: its site, its ordinal within the key, its result once the
/// engine ran it, and the next entry of its hash bucket.
#[derive(Clone, Copy)]
struct Entry {
    site: KeySite,
    call: u16,
    value: Option<invocation_results::ResultBytes>,
    next: u32,
}

/// A collected call the engine runs before the statement restarts.
#[derive(Clone, Copy)]
pub(crate) struct CollectedCall {
    entry: u32,
    pub(crate) pending: PendingRoutineInvocation,
    /// Made under a maintenance operation's restricted search path.
    pub(crate) restricted: bool,
}

const NO_ENTRY: u32 = u32::MAX;

/// The kept calls of a statement, in the statement arena's retained tail.
#[derive(Clone, Copy)]
pub(crate) struct KeyCalls {
    entries: crate::mem::arena::RetainedList<Entry>,
    buckets: crate::mem::arena::RetainedList<u32>,
    collected: crate::mem::arena::RetainedList<CollectedCall>,
    /// The next collected call the engine runs.
    running: usize,
    /// This attempt deferred a call that may read a placeholder; later
    /// calls wait too, so the engine runs calls in the statement's order.
    deferred: bool,
}

fn full() -> SqlError {
    sql_err!(
        sqlstate::PROGRAM_LIMIT_EXCEEDED,
        "index key calls exceed statement memory"
    )
}

fn list_error(_: crate::mem::arena::RetainedListError) -> SqlError {
    full()
}

fn slice_error(_: crate::mem::arena::ArenaSliceError) -> SqlError {
    sql_err!(
        sqlstate::INTERNAL_ERROR,
        "index key calls left their statement memory"
    )
}

fn hash(site: KeySite, call: u16) -> u64 {
    let mut value = site.rowid ^ site.index.rotate_left(17);
    value ^= (u64::from(site.position) << 48) ^ (u64::from(call) << 32);
    // A 64-bit finalizer (MurmurHash3's fmix64) spreads sequential row ids.
    value ^= value >> 33;
    value = value.wrapping_mul(0xff51_afd7_ed55_8ccd);
    value ^= value >> 33;
    value = value.wrapping_mul(0xc4ce_b9fe_1a85_ec53);
    value ^ (value >> 33)
}

impl KeyCalls {
    pub(crate) const fn new() -> Self {
        Self {
            entries: crate::mem::arena::RetainedList::new(),
            buckets: crate::mem::arena::RetainedList::new(),
            collected: crate::mem::arena::RetainedList::new(),
            running: 0,
            deferred: false,
        }
    }

    fn find(&self, arena: &Arena, site: KeySite, call: u16) -> Result<Option<u32>, SqlError> {
        let buckets = self.buckets.len();
        if buckets == 0 {
            return Ok(None);
        }
        let mut at = self
            .buckets
            .get(arena, (hash(site, call) as usize) & (buckets - 1))
            .map_err(slice_error)?
            .unwrap_or(NO_ENTRY);
        while at != NO_ENTRY {
            let entry = self
                .entries
                .get(arena, at as usize)
                .map_err(slice_error)?
                .ok_or_else(|| slice_error(crate::mem::arena::ArenaSliceError::Stale))?;
            if entry.site == site && entry.call == call {
                return Ok(Some(at));
            }
            at = entry.next;
        }
        Ok(None)
    }

    fn insert(&mut self, arena: &Arena, site: KeySite, call: u16) -> Result<u32, SqlError> {
        let index = u32::try_from(self.entries.len())
            .ok()
            .filter(|index| *index != NO_ENTRY)
            .ok_or_else(full)?;
        self.entries
            .push(
                arena,
                Entry {
                    site,
                    call,
                    value: None,
                    next: NO_ENTRY,
                },
            )
            .map_err(list_error)?;
        // Keep at most two entries per bucket on average.
        if self.entries.len() > self.buckets.len() * 2 {
            self.rebuild(arena)?;
        } else {
            self.link(arena, index)?;
        }
        Ok(index)
    }

    fn link(&mut self, arena: &Arena, index: u32) -> Result<(), SqlError> {
        let entry = self
            .entries
            .get(arena, index as usize)
            .map_err(slice_error)?
            .ok_or_else(|| slice_error(crate::mem::arena::ArenaSliceError::Stale))?;
        let bucket = (hash(entry.site, entry.call) as usize) & (self.buckets.len() - 1);
        let head = self
            .buckets
            .with_mut_slice(arena, |buckets| {
                core::mem::replace(&mut buckets[bucket], index)
            })
            .map_err(slice_error)?;
        self.entries
            .with_mut_slice(arena, |entries| entries[index as usize].next = head)
            .map_err(slice_error)
    }

    fn rebuild(&mut self, arena: &Arena) -> Result<(), SqlError> {
        let count = self.entries.len().next_power_of_two().max(64);
        let mut buckets = crate::mem::arena::RetainedList::new();
        for _ in 0..count {
            buckets.push(arena, NO_ENTRY).map_err(list_error)?;
        }
        self.buckets = buckets;
        for index in 0..self.entries.len() {
            self.link(arena, index as u32)?;
        }
        Ok(())
    }

    /// Whether the attempt collected calls the engine must run first.
    pub(crate) fn has_collected(&self) -> bool {
        self.running < self.collected.len()
    }

    /// The next collected call to run, once the attempt ended.
    pub(crate) fn next_collected(&self, arena: &Arena) -> Result<Option<CollectedCall>, SqlError> {
        self.collected.get(arena, self.running).map_err(slice_error)
    }

    /// Records the result of the collected call [`Self::next_collected`]
    /// returned.
    pub(crate) fn complete_collected(
        &mut self,
        arena: &Arena,
        value: invocation_results::ResultBytes,
    ) -> Result<(), SqlError> {
        let call = self
            .collected
            .get(arena, self.running)
            .map_err(slice_error)?
            .ok_or_else(|| {
                sql_err!(
                    sqlstate::INTERNAL_ERROR,
                    "an index key call completed without a collected call"
                )
            })?;
        self.entries
            .with_mut_slice(arena, |entries| {
                entries[call.entry as usize].value = Some(value);
            })
            .map_err(slice_error)?;
        self.running += 1;
        if self.running == self.collected.len() {
            self.collected.truncate(0);
            self.running = 0;
        }
        Ok(())
    }
}

/// What the call at the active key site reads.
pub(crate) enum KeyCall {
    /// No key site is active: the statement's ordinary replay applies.
    Unkeyed,
    /// The engine ran this call in an earlier attempt.
    Kept(invocation_results::ResultBytes),
    /// The value is not known in this attempt; the call reads NULL.
    Placeholder,
}

impl RoutineInvocationState {
    /// Resolves an engine call made at the active key site, collecting it
    /// when no attempt ran it yet. `pending` builds the call to collect.
    pub(super) fn key_call(
        &self,
        statement_arena: &Arena,
        pending: impl FnOnce() -> Result<PendingRoutineInvocation, SqlError>,
    ) -> Result<KeyCall, SqlError> {
        if !self.engine_calls_only {
            return Ok(KeyCall::Unkeyed);
        }
        let Some(mut active) = ACTIVE.with(Cell::get) else {
            return Ok(KeyCall::Unkeyed);
        };
        let call = active.calls;
        active.calls = active.calls.checked_add(1).ok_or_else(full)?;
        let tainted = active.tainted;
        active.tainted = true;
        let mut calls = self.key_calls.get();
        let found = calls.find(statement_arena, active.site, call)?;
        let kept = match found {
            Some(at) => calls
                .entries
                .get(statement_arena, at as usize)
                .map_err(slice_error)?
                .and_then(|entry| entry.value),
            None => None,
        };
        if let Some(value) = kept {
            active.tainted = tainted;
            ACTIVE.with(|slot| slot.set(Some(active)));
            return Ok(KeyCall::Kept(value));
        }
        ACTIVE.with(|slot| slot.set(Some(active)));
        // A call after a placeholder may read it: it waits for a later
        // attempt, and so does every call after it, which keeps the calls'
        // order. A call already collected waits for the engine.
        if tainted && found.is_none() {
            calls.deferred = true;
        } else if !calls.deferred && found.is_none() {
            let entry = calls.insert(statement_arena, active.site, call)?;
            calls
                .collected
                .push(
                    statement_arena,
                    CollectedCall {
                        entry,
                        pending: pending()?,
                        restricted: crate::sql::exec::maintenance_path_active(),
                    },
                )
                .map_err(list_error)?;
        }
        self.key_calls.set(calls);
        Ok(KeyCall::Placeholder)
    }

    /// Starts an attempt, which may collect calls again.
    pub(super) fn begin_key_attempt(&self) {
        let mut calls = self.key_calls.get();
        calls.deferred = false;
        self.key_calls.set(calls);
    }

    /// Whether the attempt collected index key calls the engine must run
    /// before the statement restarts.
    pub(crate) fn has_collected_key_calls(&self) -> bool {
        self.key_calls.get().has_collected()
    }

    /// The next collected index key call, which the engine runs now; its
    /// completion keeps the result by the call's site.
    pub(crate) fn next_key_call(&self, arena: &Arena) -> Result<Option<CollectedCall>, SqlError> {
        let calls = self.key_calls.get();
        if !calls.has_collected() {
            self.completing_key_call.set(false);
            return Ok(None);
        }
        let next = calls.next_collected(arena)?;
        self.completing_key_call.set(next.is_some());
        Ok(next)
    }
}
