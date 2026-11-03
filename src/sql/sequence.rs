//! The runtime bridge for `nextval`/`currval`/`lastval`/`setval`.
//!
//! Sequence functions are volatile and side-effecting, yet the expression
//! evaluator is a pure `&`-only pass. [`SeqEval`] reconciles the two: it holds a
//! shared `&Storage` and advances a generator through the [`SequenceDef`]'s
//! `Cell` value state (allocation-free interior mutability), recording
//! `currval`/`lastval` in the per-connection [`SeqSession`]. It implements the
//! [`SequenceAccess`] trait the evaluator calls through [`EvalHooks`].
//!
//! [`EvalHooks`]: crate::sql::eval::EvalHooks

use crate::mem::arena::{Arena, ArenaHandle, ArenaList, ArenaSliceHandle};
use crate::sql::eval::{SequenceAccess, SequenceReference, SqlError, sqlstate};
use crate::sql::guc::SeqSession;
use crate::sql_err;
use crate::storage::{AccessClass, AccessObject, PrivilegeSet, Storage};
use core::cell::Cell;
use core::marker::PhantomData;

#[derive(Clone, Copy, PartialEq, Eq)]
enum SequenceOperation {
    Next,
    Current,
    Last,
    Set { value: i64, is_called: bool },
}

#[derive(Clone, Copy, PartialEq, Eq)]
struct SequenceCall {
    operation: SequenceOperation,
    source: Option<crate::storage::SequenceReplaySource>,
}

#[derive(Clone, Copy)]
struct SequenceEffect {
    call: SequenceCall,
    value: i64,
    next: Option<ArenaHandle<SequenceEffect>>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum StatementEffectKind {
    StatisticsVoid,
    Cursor,
    RoutineInput,
    PlanAllocation,
}

#[derive(Clone, Copy)]
enum StatementEffectResult {
    Values(ArenaSliceHandle<u8>),
    Error(ArenaHandle<StatementEffectError>),
}

#[derive(Clone, Copy)]
struct StatementEffectError {
    state: super::eval::SqlState,
    message: crate::util::StackStr<192>,
    detail: Option<ArenaSliceHandle<u8>>,
    hint: Option<ArenaSliceHandle<u8>>,
}

#[derive(Clone, Copy)]
struct SequenceSessionValue {
    slot: usize,
    created_at: u64,
    value: i64,
}

#[derive(Clone, Copy)]
struct StatementEffect {
    kind: StatementEffectKind,
    key: ArenaSliceHandle<u8>,
    result: Option<StatementEffectResult>,
    replayable: bool,
    children: StatementSequenceReplay,
    after_lastval: Option<(usize, u64)>,
    after_sequences: Option<ArenaSliceHandle<SequenceSessionValue>>,
    next: Option<ArenaHandle<StatementEffect>>,
}

#[derive(Clone, Copy)]
pub(crate) enum StatementEffectRetry {
    Restart,
    Continue,
}

#[derive(Clone, Copy)]
pub(super) struct StatementSequenceReplay {
    head: Option<ArenaHandle<SequenceEffect>>,
    tail: Option<ArenaHandle<SequenceEffect>>,
    next: Option<ArenaHandle<SequenceEffect>>,
    initial_lastval: Option<(usize, u64)>,
    effects_head: Option<ArenaHandle<StatementEffect>>,
    effects_tail: Option<ArenaHandle<StatementEffect>>,
    effects_next: Option<ArenaHandle<StatementEffect>>,
    nonreplayable: bool,
}

impl StatementSequenceReplay {
    pub(super) fn is_retained(self) -> bool {
        self.head.is_some() || self.effects_head.is_some()
    }

    pub(super) const EMPTY: Self = Self {
        head: None,
        tail: None,
        next: None,
        initial_lastval: None,
        effects_head: None,
        effects_tail: None,
        effects_next: None,
        nonreplayable: false,
    };
}

thread_local! {
    static STATEMENT_ARENA: Cell<*const Arena> = const { Cell::new(core::ptr::null()) };
    static STATEMENT_SESSION: Cell<*const SeqSession> = const { Cell::new(core::ptr::null()) };
}

pub(super) struct StatementSequenceScope<'arena, 'session> {
    previous: *const Arena,
    previous_session: *const SeqSession,
    arena: PhantomData<(&'arena Arena, &'session SeqSession)>,
}

impl StatementSequenceScope<'_, '_> {
    pub(super) fn finish(&self, session: &SeqSession) -> Result<(), SqlError> {
        if !self.previous.is_null() {
            return Ok(());
        }
        let replay = session.statement_replay.get();
        let complete = replay.next.is_none() && replay.effects_next.is_none();
        finish_statement(session);
        if complete {
            Ok(())
        } else {
            Err(replay_changed())
        }
    }

    pub(super) fn discard(&self, session: &SeqSession) {
        if self.previous.is_null() {
            finish_statement(session);
        }
    }
}

impl Drop for StatementSequenceScope<'_, '_> {
    fn drop(&mut self) {
        STATEMENT_ARENA.with(|active| active.set(self.previous));
        STATEMENT_SESSION.with(|active| active.set(self.previous_session));
    }
}

pub(super) fn enter_statement<'arena, 'session>(
    arena: &'arena Arena,
    session: &'session SeqSession,
    resumed: bool,
) -> StatementSequenceScope<'arena, 'session> {
    let arena = arena.retention_owner();
    let previous = STATEMENT_ARENA.with(|active| active.get());
    if !previous.is_null() {
        return StatementSequenceScope {
            previous,
            previous_session: STATEMENT_SESSION.with(Cell::get),
            arena: PhantomData,
        };
    }
    let retained = session.statement_replay.get();
    let mut replay = if resumed && retained.is_retained() {
        session.restore_last_sequence(retained.initial_lastval);
        retained
    } else {
        StatementSequenceReplay {
            initial_lastval: session.last_sequence(),
            ..StatementSequenceReplay::EMPTY
        }
    };
    replay.next = replay.head;
    replay.effects_next = replay.effects_head;
    session.statement_replay.set(replay);
    StatementSequenceScope {
        previous: STATEMENT_ARENA.with(|active| active.replace(arena)),
        previous_session: STATEMENT_SESSION.with(|active| active.replace(session)),
        arena: PhantomData,
    }
}

pub(super) fn finish_statement(session: &SeqSession) {
    session.statement_replay.set(StatementSequenceReplay::EMPTY);
}

fn replay_changed() -> SqlError {
    sql_err!(
        sqlstate::SERIALIZATION_FAILURE,
        "sequence source or call order changed while resuming the statement"
    )
}

fn statement_call(
    session: &SeqSession,
    call: SequenceCall,
    action: impl FnOnce() -> Result<i64, SqlError>,
) -> Result<i64, SqlError> {
    if matches!(
        call.operation,
        SequenceOperation::Next | SequenceOperation::Set { .. }
    ) {
        super::query::protect_statement_effects();
    }
    STATEMENT_ARENA.with(|active| {
        let pointer = active.get();
        if pointer.is_null() {
            return action();
        }
        // SAFETY: the scope borrows this thread's arena until it restores the
        // previous pointer. No reference escapes this synchronous invocation.
        let arena = unsafe { &*pointer };
        let mut replay = session.statement_replay.get();
        if let Some(handle) = replay.next {
            let effect = arena.retained(handle).ok_or_else(replay_changed)?;
            if effect.call != call {
                let recreated = effect.call.operation == call.operation
                    && call
                        .source
                        .zip(effect.call.source)
                        .is_some_and(|(now, prior)| now.recreates(prior));
                if !recreated {
                    return Err(replay_changed());
                }
                // A CREATE or RESTART undone with the failed attempt needs its
                // new private generator advanced; shared generators never do.
                if action()? != effect.value {
                    return Err(replay_changed());
                }
                assert!(arena.replace_retained(handle, SequenceEffect { call, ..effect }));
            }
            replay.next = effect.next;
            session.statement_replay.set(replay);
            return Ok(effect.value);
        }
        let prior_tail = replay
            .tail
            .map(|tail| {
                arena
                    .retained(tail)
                    .map(|effect| (tail, effect))
                    .ok_or_else(replay_changed)
            })
            .transpose()?;
        // Reserve before advancing a sequence: exhaustion cannot lose an
        // unrecorded nontransactional effect.
        let handle = arena
            .retain(SequenceEffect {
                call,
                value: 0,
                next: None,
            })
            .map_err(|_| {
                sql_err!(
                    sqlstate::PROGRAM_LIMIT_EXCEEDED,
                    "volatile sequence replay exceeds statement memory"
                )
            })?;
        let value = action()?;
        assert!(arena.replace_retained(
            handle,
            SequenceEffect {
                call,
                value,
                next: None
            }
        ));
        if let Some((tail, effect)) = prior_tail {
            assert!(arena.replace_retained(
                tail,
                SequenceEffect {
                    next: Some(handle),
                    ..effect
                }
            ));
        } else {
            replay.head = Some(handle);
        }
        replay.tail = Some(handle);
        session.statement_replay.set(replay);
        Ok(value)
    })
}

pub(crate) struct StatementEffectAttempt<'a> {
    pub result: Result<&'a [super::types::Datum<'a>], SqlError>,
    pub rewound_state: bool,
}

fn nonreplayable_effect() -> SqlError {
    sql_err!(
        sqlstate::FEATURE_NOT_SUPPORTED,
        "a cursor or routine input with transactional or session effects cannot resume after a statement wait"
    )
}

pub(crate) fn ensure_statement_effects_replayable() -> Result<(), SqlError> {
    with_statement_context(|context| {
        if context.is_some_and(|(_, session)| session.statement_replay.get().nonreplayable) {
            Err(nonreplayable_effect())
        } else {
            Ok(())
        }
    })
}

fn effect_full() -> SqlError {
    sql_err!(
        sqlstate::PROGRAM_LIMIT_EXCEEDED,
        "statement effect replay exceeds statement memory"
    )
}

fn with_statement_context<R>(
    execute: impl for<'scope> FnOnce(Option<(&'scope Arena, &'scope SeqSession)>) -> R,
) -> R {
    let arena = STATEMENT_ARENA.with(Cell::get);
    let session = STATEMENT_SESSION.with(Cell::get);
    if arena.is_null() || session.is_null() {
        return execute(None);
    }
    // SAFETY: enter_statement keeps both owners borrowed on this thread. The
    // higher-ranked closure cannot return a borrow into the active context.
    execute(Some(unsafe { (&*arena, &*session) }))
}

fn retained_effect_bytes(
    arena: &Arena,
    values: &[super::types::Datum<'_>],
) -> Result<ArenaSliceHandle<u8>, SqlError> {
    let length = super::exec::projected_row_len(values)?;
    let handle = arena
        .retain_slice_with(length, |_| 0u8)
        .map_err(|_| effect_full())?;
    {
        let mut bytes = arena
            .retained_slice_mut(handle)
            .map_err(|_| replay_changed())?;
        super::exec::encode_projected_into(values, &mut bytes)?;
    }
    Ok(handle)
}

fn restore_effect_values<'a>(
    arena: &Arena,
    handle: ArenaSliceHandle<u8>,
    output: &'a Arena,
) -> Result<&'a [super::types::Datum<'a>], SqlError> {
    let encoded = {
        let bytes = arena
            .retained_slice_mut(handle)
            .map_err(|_| replay_changed())?;
        output.alloc_slice_copy(&bytes).map_err(|_| effect_full())?
    };
    let count = super::exec::projected_row_width(encoded);
    let values = output
        .alloc_slice_with(count, |_| super::types::Datum::Null)
        .map_err(|_| effect_full())?;
    super::exec::decode_projected_row_into(encoded, output, values)?;
    Ok(values)
}

#[inline(never)]
fn retain_effect_error(
    arena: &Arena,
    error: &SqlError,
) -> Result<ArenaHandle<StatementEffectError>, SqlError> {
    let diagnostic = super::eval::take_diagnostic();
    let copy = |value: &str| {
        arena
            .retain_slice_with(value.len(), |at| value.as_bytes()[at])
            .map_err(|_| effect_full())
    };
    let (detail, hint) = match diagnostic {
        Some(diagnostic) => (
            Some(copy(diagnostic.detail.as_str())?),
            diagnostic
                .hint
                .map(|hint| copy(hint.as_str()))
                .transpose()?,
        ),
        None => (None, None),
    };
    let saved = arena
        .retain(StatementEffectError {
            state: error.sqlstate,
            message: error.message,
            detail,
            hint,
        })
        .map_err(|_| effect_full())?;
    if let Some(diagnostic) = diagnostic {
        super::eval::stash_diagnostic(diagnostic.detail, diagnostic.hint);
    }
    Ok(saved)
}

#[inline(never)]
fn restore_effect_error(
    arena: &Arena,
    handle: ArenaHandle<StatementEffectError>,
) -> Result<SqlError, SqlError> {
    let saved = arena.retained(handle).ok_or_else(replay_changed)?;
    if let Some(detail) = saved.detail {
        let detail = arena
            .retained_slice_mut(detail)
            .map_err(|_| replay_changed())?;
        let text = crate::util::StackStr::from_str(
            core::str::from_utf8(&detail).map_err(|_| replay_changed())?,
        );
        let hint = if let Some(hint) = saved.hint {
            let hint = arena
                .retained_slice_mut(hint)
                .map_err(|_| replay_changed())?;
            let text = crate::util::StackStr::from_str(
                core::str::from_utf8(&hint).map_err(|_| replay_changed())?,
            );
            Some(text)
        } else {
            None
        };
        super::eval::stash_diagnostic(text, hint);
    }
    Ok(SqlError {
        sqlstate: saved.state,
        message: saved.message,
    })
}

// Capture only sources changed inside this subtree. Values are read after the
// complete subtree, so nested effects and later sibling calls share the same
// final session value without requiring a global sequence-sized snapshot.
fn retain_sequence_session(
    arena: &Arena,
    scratch: &Arena,
    session: &SeqSession,
    replay: StatementSequenceReplay,
) -> Result<Option<ArenaSliceHandle<SequenceSessionValue>>, SqlError> {
    if replay.head.is_none() && replay.effects_head.is_none() {
        return Ok(None);
    }
    let mut pending = ArenaList::new(scratch);
    let mut values = ArenaList::new(scratch);
    pending.push(replay).map_err(|_| effect_full())?;
    let mut at = 0;
    while at < pending.len() {
        let current = pending.borrowed_slice()[at];
        at += 1;
        let mut sequence = current.head;
        while let Some(handle) = sequence {
            let effect = arena.retained(handle).ok_or_else(replay_changed)?;
            if matches!(
                effect.call.operation,
                SequenceOperation::Next
                    | SequenceOperation::Set {
                        is_called: true,
                        ..
                    }
            ) && let Some(source) = effect.call.source
            {
                let (slot, created_at) = source.session_identity();
                if let Some(value) = session.currval(slot, created_at) {
                    values
                        .push(SequenceSessionValue {
                            slot,
                            created_at,
                            value,
                        })
                        .map_err(|_| effect_full())?;
                }
            }
            sequence = effect.next;
        }
        let mut nested = current.effects_head;
        while let Some(handle) = nested {
            let effect = arena.retained(handle).ok_or_else(replay_changed)?;
            pending.push(effect.children).map_err(|_| effect_full())?;
            nested = effect.next;
        }
    }
    if values.is_empty() {
        return Ok(None);
    }
    let values = values.as_slice();
    arena
        .retain_slice_with(values.len(), |at| values[at])
        .map(Some)
        .map_err(|_| effect_full())
}

/// A completed session effect is reused without touching its cursor or
/// global counters. Nested effects have their own replay subtree.
pub(crate) fn statement_effect<'a>(
    kind: StatementEffectKind,
    key: &[super::types::Datum<'_>],
    retry: StatementEffectRetry,
    output: &'a Arena,
    execute: impl FnOnce() -> StatementEffectAttempt<'a>,
) -> Result<&'a [super::types::Datum<'a>], SqlError> {
    with_statement_context(|context| {
        let Some((arena, session)) = context else {
            return execute().result;
        };
        let key_length = super::exec::projected_row_len(key)?;
        let encoded_key = output
            .alloc_slice_with(key_length, |_| 0u8)
            .map_err(|_| effect_full())?;
        super::exec::encode_projected_into(key, encoded_key)?;
        let mut parent = session.statement_replay.get();
        let (handle, mut effect) = if let Some(handle) = parent.effects_next {
            let effect = arena.retained(handle).ok_or_else(replay_changed)?;
            let same = {
                let prior = arena
                    .retained_slice_mut(effect.key)
                    .map_err(|_| replay_changed())?;
                effect.kind == kind && &*prior == encoded_key
            };
            if !same {
                return Err(replay_changed());
            }
            parent.effects_next = effect.next;
            (handle, effect)
        } else {
            let key = retained_effect_bytes(arena, key)?;
            let effect = StatementEffect {
                kind,
                key,
                result: None,
                replayable: true,
                children: StatementSequenceReplay {
                    initial_lastval: session.last_sequence(),
                    ..StatementSequenceReplay::EMPTY
                },
                after_lastval: None,
                after_sequences: None,
                next: None,
            };
            let handle = arena.retain(effect).map_err(|_| effect_full())?;
            if let Some(tail) = parent.effects_tail {
                let prior = arena.retained(tail).ok_or_else(replay_changed)?;
                if !arena.replace_retained(
                    tail,
                    StatementEffect {
                        next: Some(handle),
                        ..prior
                    },
                ) {
                    return Err(replay_changed());
                }
            } else {
                parent.effects_head = Some(handle);
            }
            parent.effects_tail = Some(handle);
            (handle, effect)
        };
        if let Some(result) = effect.result {
            if !effect.replayable {
                return Err(nonreplayable_effect());
            }
            session.statement_replay.set(parent);
            if let Some(values) = effect.after_sequences {
                let values = arena
                    .retained_slice_mut(values)
                    .map_err(|_| replay_changed())?;
                for saved in values.iter() {
                    session.record_setval(saved.slot, saved.created_at, saved.value, true);
                }
            }
            session.restore_last_sequence(effect.after_lastval);
            return match result {
                StatementEffectResult::Values(result) => {
                    restore_effect_values(arena, result, output)
                }
                StatementEffectResult::Error(error) => Err(restore_effect_error(arena, error)?),
            };
        }
        let mut child = effect.children;
        match retry {
            StatementEffectRetry::Restart => {
                child.next = child.head;
                child.effects_next = child.effects_head;
                session.restore_last_sequence(child.initial_lastval);
            }
            StatementEffectRetry::Continue => {
                child.next = None;
                child.effects_next = None;
            }
        }
        session.statement_replay.set(child);
        super::query::protect_statement_effects();
        let attempt = execute();
        effect.children = session.statement_replay.get();
        effect.after_lastval = session.last_sequence();
        effect.replayable = !attempt.rewound_state && !effect.children.nonreplayable;
        parent.nonreplayable |= !effect.replayable;
        session.statement_replay.set(parent);
        let result = match attempt.result {
            Ok(_) if effect.children.next.is_some() || effect.children.effects_next.is_some() => {
                Err(replay_changed())
            }
            Err(error)
                if !effect.replayable
                    && matches!(
                        error.sqlstate.as_str(),
                        sqlstate::INTERNAL_IO_WAIT | sqlstate::INTERNAL_LOCK_WAIT
                    ) =>
            {
                Err(nonreplayable_effect())
            }
            result => result,
        };
        match &result {
            Ok(values) => {
                effect.after_sequences =
                    retain_sequence_session(arena, output, session, effect.children)?;
                effect.result = Some(StatementEffectResult::Values(retained_effect_bytes(
                    arena, values,
                )?));
            }
            Err(error)
                if kind == StatementEffectKind::Cursor
                    && !matches!(
                        error.sqlstate.as_str(),
                        sqlstate::INTERNAL_IO_WAIT
                            | sqlstate::INTERNAL_LOCK_WAIT
                            | sqlstate::INTERNAL_ROUTINE_INVOCATION
                    ) =>
            {
                effect.after_sequences =
                    retain_sequence_session(arena, output, session, effect.children)?;
                effect.result = Some(StatementEffectResult::Error(retain_effect_error(
                    arena, error,
                )?));
            }
            Err(_) => {}
        }
        if !arena.replace_retained(handle, effect) {
            return Err(replay_changed());
        }
        result
    })
}

/// Bounded effect log for an expression stream that may be physically retried
/// while waiting for a mutable SQL routine. Sequence calls are replayed in
/// logical call order, never advanced again by a retry.
pub struct SequenceReplayState {
    first: Cell<Option<ArenaHandle<SequenceReplayChunk>>>,
    last: Cell<Option<ArenaHandle<SequenceReplayChunk>>>,
    next: Cell<Option<SequenceReplayPosition>>,
}

#[derive(Clone, Copy)]
struct SequenceReplayPosition {
    chunk: ArenaHandle<SequenceReplayChunk>,
    index: usize,
}

const SEQUENCE_VALUES_PER_CHUNK: usize = 16;

#[derive(Clone, Copy)]
struct SequenceReplayChunk {
    values: [i64; SEQUENCE_VALUES_PER_CHUNK],
    len: usize,
    next: Option<ArenaHandle<SequenceReplayChunk>>,
}

impl SequenceReplayChunk {
    const EMPTY: Self = Self {
        values: [0; SEQUENCE_VALUES_PER_CHUNK],
        len: 0,
        next: None,
    };
}

impl SequenceReplayState {
    pub const fn empty() -> Self {
        Self {
            first: Cell::new(None),
            last: Cell::new(None),
            next: Cell::new(None),
        }
    }

    pub fn begin_attempt(&self) {
        self.next.set(
            self.first
                .get()
                .map(|chunk| SequenceReplayPosition { chunk, index: 0 }),
        );
    }

    fn invoke(
        &self,
        arena: &Arena,
        action: impl FnOnce() -> Result<i64, SqlError>,
    ) -> Result<i64, SqlError> {
        let stale = || {
            sql_err!(
                sqlstate::SERIALIZATION_FAILURE,
                "volatile sequence replay state was reset or replaced"
            )
        };
        if let Some(position) = self.next.get() {
            let recorded = arena.retained(position.chunk).ok_or_else(stale)?;
            if position.index >= recorded.len {
                return Err(stale());
            }
            let value = recorded.values[position.index];
            self.next.set(if position.index + 1 < recorded.len {
                Some(SequenceReplayPosition {
                    index: position.index + 1,
                    ..position
                })
            } else {
                recorded
                    .next
                    .map(|chunk| SequenceReplayPosition { chunk, index: 0 })
            });
            return Ok(value);
        }
        let previous = self
            .last
            .get()
            .map(|handle| {
                let recorded = arena.retained(handle).ok_or_else(stale)?;
                Ok::<_, SqlError>((handle, recorded))
            })
            .transpose()?;
        // Fixed chunks keep values contiguous without copying a growing list.
        // Reserve their space before issuing an irreversible sequence value.
        let (handle, mut recorded, appended) = match previous {
            Some((handle, recorded)) if recorded.len < SEQUENCE_VALUES_PER_CHUNK => {
                (handle, recorded, false)
            }
            _ => {
                let handle = arena.retain(SequenceReplayChunk::EMPTY).map_err(|_| {
                    sql_err!(
                        sqlstate::PROGRAM_LIMIT_EXCEEDED,
                        "volatile sequence replay exceeds statement memory"
                    )
                })?;
                (handle, SequenceReplayChunk::EMPTY, true)
            }
        };
        let value = action()?;
        recorded.values[recorded.len] = value;
        recorded.len += 1;
        assert!(arena.replace_retained(handle, recorded));
        if appended {
            if let Some((last, recorded)) = previous {
                assert!(arena.replace_retained(
                    last,
                    SequenceReplayChunk {
                        next: Some(handle),
                        ..recorded
                    }
                ));
            } else {
                self.first.set(Some(handle));
            }
            self.last.set(Some(handle));
        }
        Ok(value)
    }
}

#[derive(Clone, Copy)]
pub struct SeqEval<'a> {
    storage: &'a Storage,
    session: &'a SeqSession,
    txid: u32,
    /// A "dry" evaluator resolves names (so errors match) but never advances a
    /// generator or touches session state. Write validation uses it before
    /// executing changes; query materialization evaluates live values once.
    dry: bool,
}

pub struct ReplaySeqEval<'base, 'state, 'arena> {
    base: SeqEval<'base>,
    state: &'state SequenceReplayState,
    arena: &'arena Arena,
}

impl<'base, 'state, 'arena> ReplaySeqEval<'base, 'state, 'arena> {
    pub fn new(
        base: SeqEval<'base>,
        state: &'state SequenceReplayState,
        arena: &'arena Arena,
    ) -> Self {
        Self { base, state, arena }
    }
}

impl<'a> SeqEval<'a> {
    pub fn new(storage: &'a Storage, session: &'a SeqSession, txid: u32) -> Self {
        SeqEval {
            storage,
            session,
            txid,
            dry: false,
        }
    }

    pub fn dry(storage: &'a Storage, session: &'a SeqSession, txid: u32) -> Self {
        SeqEval {
            storage,
            session,
            txid,
            dry: true,
        }
    }

    /// Resolves a `nextval('name')` argument to a live sequence slot. The name
    /// may be schema-qualified (`schema.seq`); resolution otherwise walks the
    /// search path, exactly as relation resolution does.
    fn resolve(&self, name: SequenceReference<'_>) -> Result<usize, SqlError> {
        let reference = match name {
            SequenceReference::Name(name) => crate::sql::identifiers::ObjectReference::parse(name)?,
            SequenceReference::Oid(oid) => crate::sql::identifiers::ObjectReference::Oid(oid),
        };
        let slot = match reference {
            crate::sql::identifiers::ObjectReference::Oid(oid) => {
                let slot = (0..self.storage.sequence_count()).find(|slot| {
                    self.storage.sequence_slot_visible_to(*slot, self.txid)
                        && crate::sql::catalog::sequence_oid(self.storage, *slot) as u32 == oid
                });
                if let Some(slot) = slot {
                    return Ok(slot);
                }
                if crate::sql::catalog::relation_oid_visibility(self.storage, self.txid, oid as i32)
                    .is_some()
                {
                    return Err(sql_err!(
                        sqlstate::WRONG_OBJECT_TYPE,
                        "object {} is not a sequence",
                        oid
                    ));
                }
                return Err(sql_err!(
                    sqlstate::INTERNAL_ERROR,
                    "could not open relation with OID {}",
                    oid
                ));
            }
            crate::sql::identifiers::ObjectReference::Name(parsed) => {
                if parsed.database.is_some_and(|database| {
                    database != self.storage.current_database_name(self.txid)
                }) {
                    return Err(sql_err!(
                        sqlstate::FEATURE_NOT_SUPPORTED,
                        "cross-database sequence references are not supported"
                    ));
                }
                let qualifier = parsed.schema.as_ref().map(crate::storage::SqlName::as_str);
                if let Some(schema) = qualifier
                    && self
                        .storage
                        .find_schema_visible(schema, self.txid)
                        .is_some()
                {
                    self.storage.require_schema_usage(schema, self.txid)?;
                }
                let base = parsed.name.as_str();
                let slot = self.storage.sequence_on_path(qualifier, base, self.txid);
                if slot.is_none()
                    && self
                        .storage
                        .resolve_relation(qualifier, base, self.txid)
                        .is_some()
                {
                    return Err(sql_err!(
                        sqlstate::WRONG_OBJECT_TYPE,
                        "\"{}\" is not a sequence",
                        base
                    ));
                }
                slot
            }
        };
        if let Some(slot) = slot {
            self.storage.require_schema_usage(
                self.storage.sequence_for(slot, self.txid).schema.as_str(),
                self.txid,
            )?;
            return Ok(slot);
        }
        Err(sql_err!(
            sqlstate::UNDEFINED_TABLE,
            "relation \"{}\" does not exist",
            name
        ))
    }

    fn require_any(
        &self,
        slot: usize,
        first: PrivilegeSet,
        second: Option<PrivilegeSet>,
    ) -> Result<(), SqlError> {
        let role = self.storage.current_role_slot(self.txid).ok_or_else(|| {
            sql_err!(
                sqlstate::INSUFFICIENT_PRIVILEGE,
                "current role is not present in the role catalog"
            )
        })?;
        let object = AccessObject {
            class: AccessClass::Sequence,
            slot: slot as u16,
        };
        if self
            .storage
            .has_object_privilege(object, role, first, self.txid)
            || second.is_some_and(|privilege| {
                self.storage
                    .has_object_privilege(object, role, privilege, self.txid)
            })
        {
            return Ok(());
        }
        Err(sql_err!(
            sqlstate::INSUFFICIENT_PRIVILEGE,
            "permission denied for sequence {}",
            self.storage.sequence_for(slot, self.txid).name.as_str()
        ))
    }
}

pub(crate) fn next_cached(
    storage: &Storage,
    session: &SeqSession,
    slot: usize,
    txid: u32,
) -> Result<i64, SqlError> {
    let (source, sequence) = storage.select_sequence_for_replay(slot, txid)?;
    let value = statement_call(
        session,
        SequenceCall {
            operation: SequenceOperation::Next,
            source: Some(source),
        },
        || {
            if let Some(value) = session.take_cached(slot, sequence.cache_identity()) {
                return Ok(value);
            }
            let (value, reserved, prelogged) =
                storage.reserve_sequence_values(slot, txid, sequence.cache)?;
            // Only reserving a new WAL range assigns a full transaction ID.
            if prelogged {
                storage.assign_transaction_identity(txid);
            }
            session.store_cache(slot, value, reserved, &sequence);
            Ok(value)
        },
    )?;
    session.record_nextval(slot, sequence.created_at, value);
    Ok(value)
}

impl SequenceAccess for SeqEval<'_> {
    fn nextval(&self, name: SequenceReference<'_>) -> Result<i64, SqlError> {
        let slot = self.resolve(name)?;
        self.require_any(slot, PrivilegeSet::USAGE, Some(PrivilegeSet::UPDATE))?;
        if self.dry {
            return Ok(self.storage.sequence_value_for(slot, self.txid).0);
        }
        next_cached(self.storage, self.session, slot, self.txid)
    }

    fn currval(&self, name: SequenceReference<'_>) -> Result<i64, SqlError> {
        let slot = self.resolve(name)?;
        self.require_any(slot, PrivilegeSet::USAGE, Some(PrivilegeSet::SELECT))?;
        let (source, seq) = self.storage.select_sequence_for_replay(slot, self.txid)?;
        statement_call(
            self.session,
            SequenceCall {
                operation: SequenceOperation::Current,
                source: Some(source),
            },
            || match self.session.currval(slot, seq.created_at) {
                Some(v) => Ok(v),
                None => Err(sql_err!(
                    sqlstate::OBJECT_NOT_IN_PREREQUISITE_STATE,
                    "currval of sequence \"{}\" is not yet defined in this session",
                    seq.name.as_str()
                )),
            },
        )
    }

    fn lastval(&self) -> Result<i64, SqlError> {
        let undefined = || {
            sql_err!(
                sqlstate::OBJECT_NOT_IN_PREREQUISITE_STATE,
                "lastval is not yet defined in this session"
            )
        };
        let (slot, created_at) = self.session.last_sequence().ok_or_else(undefined)?;
        let (source, definition) = self
            .storage
            .select_sequence_for_replay(slot, self.txid)
            .map_err(|_| undefined())?;
        if definition.created_at != created_at {
            return Err(undefined());
        }
        self.require_any(slot, PrivilegeSet::USAGE, Some(PrivilegeSet::SELECT))?;
        statement_call(
            self.session,
            SequenceCall {
                operation: SequenceOperation::Last,
                source: Some(source),
            },
            || self.session.lastval().ok_or_else(undefined),
        )
    }

    fn setval(
        &self,
        name: SequenceReference<'_>,
        value: i64,
        is_called: bool,
    ) -> Result<i64, SqlError> {
        let slot = self.resolve(name)?;
        self.require_any(slot, PrivilegeSet::UPDATE, None)?;
        let (source, seq) = self.storage.select_sequence_for_replay(slot, self.txid)?;
        if self.dry {
            // Validate the range without moving the generator.
            self.storage.check_sequence_value(slot, self.txid, value)?;
            return Ok(value);
        }
        let result = statement_call(
            self.session,
            SequenceCall {
                operation: SequenceOperation::Set { value, is_called },
                source: Some(source),
            },
            || {
                let result = self
                    .storage
                    .set_sequence_value(slot, self.txid, value, is_called)?;
                self.storage.assign_transaction_identity(self.txid);
                self.session.invalidate_cache(slot);
                Ok(result)
            },
        )?;
        self.session
            .record_setval(slot, seq.created_at, value, is_called);
        Ok(result)
    }
}

impl SequenceAccess for ReplaySeqEval<'_, '_, '_> {
    fn nextval(&self, name: SequenceReference<'_>) -> Result<i64, SqlError> {
        self.state.invoke(self.arena, || self.base.nextval(name))
    }
    fn currval(&self, name: SequenceReference<'_>) -> Result<i64, SqlError> {
        self.state.invoke(self.arena, || self.base.currval(name))
    }
    fn lastval(&self) -> Result<i64, SqlError> {
        self.state.invoke(self.arena, || self.base.lastval())
    }
    fn setval(
        &self,
        name: SequenceReference<'_>,
        value: i64,
        is_called: bool,
    ) -> Result<i64, SqlError> {
        self.state
            .invoke(self.arena, || self.base.setval(name, value, is_called))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mem::budget::Budget;

    #[test]
    fn nested_statement_effects_replay_values_and_children_without_repeating_actions() {
        use crate::sql::types::Datum;
        let mut budget = Budget::new(64 << 10);
        let mut arena =
            Arena::new(&mut budget, "hierarchical statement effects", 64 << 10).unwrap();
        let guc = crate::sql::guc::GucState::new();
        let session = guc.seq_session();
        let actions = Cell::new(0);
        crate::mem::guard::forbid_alloc(|| {
            {
                let _scope = enter_statement(&arena, session, false);
                let values = statement_effect(
                    StatementEffectKind::Cursor,
                    &[Datum::Text("fetch")],
                    StatementEffectRetry::Continue,
                    &arena,
                    || {
                        actions.set(actions.get() + 1);
                        let nested = statement_effect(
                            StatementEffectKind::StatisticsVoid,
                            &[Datum::Int4(2274)],
                            StatementEffectRetry::Restart,
                            &arena,
                            || {
                                actions.set(actions.get() + 1);
                                StatementEffectAttempt {
                                    result: Ok(&[]),
                                    rewound_state: false,
                                }
                            },
                        )
                        .unwrap();
                        assert!(nested.is_empty());
                        let value = statement_call(
                            session,
                            SequenceCall {
                                operation: SequenceOperation::Last,
                                source: None,
                            },
                            || Ok(7),
                        )
                        .unwrap();
                        session.record_nextval(0, 1, value);
                        let values = arena
                            .alloc_slice_copy(&[Datum::Text("cursor output"), Datum::Int8(value)])
                            .unwrap();
                        StatementEffectAttempt {
                            result: Ok(values),
                            rewound_state: false,
                        }
                    },
                )
                .unwrap();
                assert!(matches!(
                    values,
                    [Datum::Text("cursor output"), Datum::Int8(7)]
                ));
                session.record_nextval(1, 2, 99);
            }
            arena.recycle_front();
            {
                let scope = enter_statement(&arena, session, true);
                let values = statement_effect(
                    StatementEffectKind::Cursor,
                    &[Datum::Text("fetch")],
                    StatementEffectRetry::Continue,
                    &arena,
                    || panic!("completed cursor and reset must not repeat"),
                )
                .unwrap();
                assert!(matches!(
                    values,
                    [Datum::Text("cursor output"), Datum::Int8(7)]
                ));
                assert_eq!(session.last_sequence(), Some((0, 1)));
                scope.finish(session).unwrap();
            }
            assert_eq!(actions.get(), 2);
            assert!(!session.statement_replay.get().is_retained());
        });
    }

    #[test]
    fn incomplete_statement_effects_restart_or_continue_their_own_child_journal() {
        use crate::sql::types::Datum;
        let mut budget = Budget::new(64 << 10);
        let mut arena = Arena::new(&mut budget, "incomplete statement effects", 64 << 10).unwrap();
        let guc = crate::sql::guc::GucState::new();
        let session = guc.seq_session();
        let calls = Cell::new(0);
        let call = SequenceCall {
            operation: SequenceOperation::Last,
            source: None,
        };
        crate::mem::guard::forbid_alloc(|| {
            for retry in [
                StatementEffectRetry::Restart,
                StatementEffectRetry::Continue,
            ] {
                {
                    let _scope = enter_statement(&arena, session, false);
                    let error = statement_effect(
                        StatementEffectKind::RoutineInput,
                        &[Datum::Int4(1)],
                        retry,
                        &arena,
                        || {
                            statement_call(session, call, || {
                                calls.set(calls.get() + 1);
                                Ok(1)
                            })
                            .unwrap();
                            StatementEffectAttempt {
                                result: Err(sql_err!(sqlstate::INTERNAL_LOCK_WAIT, "held lock")),
                                rewound_state: false,
                            }
                        },
                    )
                    .unwrap_err();
                    assert_eq!(error.sqlstate, sqlstate::INTERNAL_LOCK_WAIT);
                }
                arena.recycle_front();
                {
                    let scope = enter_statement(&arena, session, true);
                    statement_effect(
                        StatementEffectKind::RoutineInput,
                        &[Datum::Int4(1)],
                        retry,
                        &arena,
                        || {
                            if matches!(retry, StatementEffectRetry::Restart) {
                                assert_eq!(
                                    statement_call(session, call, || panic!("child must replay"))
                                        .unwrap(),
                                    1
                                );
                            }
                            StatementEffectAttempt {
                                result: Ok(&[]),
                                rewound_state: false,
                            }
                        },
                    )
                    .unwrap();
                    scope.finish(session).unwrap();
                }
                arena.reset();
            }
            assert_eq!(calls.get(), 2);
        });
    }

    #[test]
    fn statement_effects_preserve_caught_errors_and_reject_rewound_state() {
        use crate::sql::types::Datum;
        let mut budget = Budget::new(32 << 10);
        let mut arena = Arena::new(&mut budget, "session effect errors", 32 << 10).unwrap();
        let guc = crate::sql::guc::GucState::new();
        let session = guc.seq_session();
        crate::mem::guard::forbid_alloc(|| {
            {
                let _scope = enter_statement(&arena, session, false);
                let error = statement_effect(
                    StatementEffectKind::Cursor,
                    &[Datum::Text("fetch")],
                    StatementEffectRetry::Continue,
                    &arena,
                    || {
                        super::super::eval::stash_diagnostic(
                            crate::util::StackStr::from_str("one row required"),
                            None,
                        );
                        StatementEffectAttempt {
                            result: Err(sql_err!(
                                sqlstate::CARDINALITY_VIOLATION,
                                "too many cursor rows"
                            )),
                            rewound_state: false,
                        }
                    },
                )
                .unwrap_err();
                assert_eq!(error.sqlstate, sqlstate::CARDINALITY_VIOLATION);
                assert_eq!(
                    super::super::eval::take_diagnostic()
                        .unwrap()
                        .detail
                        .as_str(),
                    "one row required"
                );
            }
            arena.recycle_front();
            {
                let scope = enter_statement(&arena, session, true);
                let error = statement_effect(
                    StatementEffectKind::Cursor,
                    &[Datum::Text("fetch")],
                    StatementEffectRetry::Continue,
                    &arena,
                    || panic!("caught fetch cannot advance again"),
                )
                .unwrap_err();
                assert_eq!(error.sqlstate, sqlstate::CARDINALITY_VIOLATION);
                assert_eq!(
                    super::super::eval::take_diagnostic()
                        .unwrap()
                        .detail
                        .as_str(),
                    "one row required"
                );
                scope.finish(session).unwrap();
            }
            arena.reset();
            {
                let scope = enter_statement(&arena, session, false);
                statement_effect(
                    StatementEffectKind::RoutineInput,
                    &[Datum::Int4(2)],
                    StatementEffectRetry::Restart,
                    &arena,
                    || StatementEffectAttempt {
                        result: Ok(&[]),
                        rewound_state: true,
                    },
                )
                .unwrap();
                assert_eq!(
                    ensure_statement_effects_replayable().unwrap_err().sqlstate,
                    sqlstate::FEATURE_NOT_SUPPORTED
                );
                scope.discard(session);
                assert!(ensure_statement_effects_replayable().is_ok());
            }
        });
    }

    #[test]
    fn statement_effects_reject_changed_identity_and_reserve_before_effects() {
        use crate::sql::types::Datum;
        let mut budget = Budget::new(4096);
        let arena = Arena::new(&mut budget, "bounded session effects", 4096).unwrap();
        let guc = crate::sql::guc::GucState::new();
        let session = guc.seq_session();
        let calls = Cell::new(0);
        crate::mem::guard::forbid_alloc(|| {
            {
                let _scope = enter_statement(&arena, session, false);
                statement_effect(
                    StatementEffectKind::StatisticsVoid,
                    &[Datum::Int4(2274)],
                    StatementEffectRetry::Restart,
                    &arena,
                    || {
                        calls.set(calls.get() + 1);
                        StatementEffectAttempt {
                            result: Ok(&[]),
                            rewound_state: false,
                        }
                    },
                )
                .unwrap();
            }
            {
                let scope = enter_statement(&arena, session, true);
                let error = statement_effect(
                    StatementEffectKind::StatisticsVoid,
                    &[Datum::Int4(3775)],
                    StatementEffectRetry::Restart,
                    &arena,
                    || panic!("changed reset target must not run"),
                )
                .unwrap_err();
                assert_eq!(error.sqlstate, sqlstate::SERIALIZATION_FAILURE);
                scope.discard(session);
            }
            let scope = enter_statement(&arena, session, false);
            let occupied = arena
                .alloc_slice_with(arena.capacity() - arena.used(), |_| 0u8)
                .unwrap();
            let error = statement_effect(
                StatementEffectKind::StatisticsVoid,
                &[],
                StatementEffectRetry::Restart,
                &arena,
                || {
                    calls.set(calls.get() + 1);
                    StatementEffectAttempt {
                        result: Ok(&[]),
                        rewound_state: false,
                    }
                },
            )
            .unwrap_err();
            assert_eq!(error.sqlstate, sqlstate::PROGRAM_LIMIT_EXCEEDED);
            assert_eq!(calls.get(), 1);
            assert!(!occupied.is_empty());
            scope.discard(session);
        });
    }

    #[test]
    fn cursor_owned_query_statement_sequence_effects_survive_row_regions_and_command_retries() {
        let mut budget = Budget::new(4096);
        let arena =
            Arena::new(&mut budget, "command sequence effects and row state", 4096).unwrap();
        let guc = crate::sql::guc::GucState::new();
        let session = guc.seq_session();
        let call = SequenceCall {
            operation: SequenceOperation::Last,
            source: None,
        };
        let actions = Cell::new(0);
        crate::mem::guard::forbid_alloc(|| {
            arena
                .with_remaining_region(|row| {
                    // A command can itself start in an embedding caller's scoped
                    // arena. Its retry journal must belong to the durable owner.
                    let _statement = enter_statement(row, session, false);
                    assert_eq!(
                        statement_call(session, call, || {
                            actions.set(actions.get() + 1);
                            Ok(7)
                        })
                        .unwrap(),
                        7
                    );
                    row.with_remaining_region(|nested| {
                        let _nested_statement = enter_statement(nested, session, false);
                        assert_eq!(
                            statement_call(session, call, || {
                                actions.set(actions.get() + 1);
                                Ok(8)
                            })
                            .unwrap(),
                            8
                        );
                    })
                    .unwrap();
                })
                .unwrap();
            for _ in 0..2 {
                let _retry = enter_statement(&arena, session, true);
                arena
                    .with_remaining_region(|row| {
                        row.alloc([0xa5u8; 256]).unwrap();
                        for expected in [7, 8] {
                            assert_eq!(
                                statement_call(session, call, || panic!(
                                    "issued sequence value must replay"
                                ))
                                .unwrap(),
                                expected
                            );
                        }
                    })
                    .unwrap();
            }
            let complete = enter_statement(&arena, session, true);
            for expected in [7, 8] {
                assert_eq!(
                    statement_call(session, call, || panic!(
                        "issued sequence value must replay"
                    ))
                    .unwrap(),
                    expected
                );
            }
            complete.finish(session).unwrap();
            assert_eq!(actions.get(), 2);
            assert!(arena.high_water() <= arena.capacity());
        });
    }

    #[test]
    fn statement_replay_preserves_calls_through_nested_scopes_and_front_rewinds() {
        let mut budget = Budget::new(1 << 16);
        let arena = Arena::new(&mut budget, "statement sequence effects", 1 << 16).unwrap();
        let guc = crate::sql::guc::GucState::new();
        let session = guc.seq_session();
        let call = SequenceCall {
            operation: SequenceOperation::Last,
            source: None,
        };
        let actions = Cell::new(0);
        crate::mem::guard::forbid_alloc(|| {
            {
                let _statement = enter_statement(&arena, session, false);
                let mark = arena.mark();
                assert_eq!(
                    statement_call(session, call, || {
                        actions.set(actions.get() + 1);
                        Ok(7)
                    })
                    .unwrap(),
                    7
                );
                let _nested = enter_statement(&arena, session, false);
                assert_eq!(
                    statement_call(session, call, || {
                        actions.set(actions.get() + 1);
                        Ok(8)
                    })
                    .unwrap(),
                    8
                );
                unsafe { arena.rewind_to(mark) };
            }
            for _ in 0..3 {
                let _statement = enter_statement(&arena, session, true);
                for expected in [7, 8] {
                    assert_eq!(
                        statement_call(session, call, || panic!(
                            "a resumed call must not run again"
                        ))
                        .unwrap(),
                        expected
                    );
                }
            }
            let statement = enter_statement(&arena, session, true);
            assert_eq!(
                statement_call(session, call, || panic!("replayed first call")).unwrap(),
                7
            );
            assert_eq!(
                statement.finish(session).unwrap_err().sqlstate,
                sqlstate::SERIALIZATION_FAILURE
            );
            assert_eq!(actions.get(), 2);
        });
    }

    #[test]
    fn statement_replay_rejects_reset_and_changed_calls_and_reserves_before_effects() {
        let mut budget = Budget::new(4096);
        let mut arena = Arena::new(&mut budget, "bounded statement effects", 4096).unwrap();
        let guc = crate::sql::guc::GucState::new();
        let session = guc.seq_session();
        let call = SequenceCall {
            operation: SequenceOperation::Last,
            source: None,
        };
        let actions = Cell::new(0);
        crate::mem::guard::forbid_alloc(|| {
            {
                let _statement = enter_statement(&arena, session, false);
                arena
                    .with_remaining_region(|row| {
                        let live = row.alloc([0xa5u8; 128]).unwrap();
                        loop {
                            let before = actions.get();
                            match statement_call(session, call, || {
                                actions.set(before + 1);
                                Ok(before)
                            }) {
                                Ok(value) => assert_eq!(value, before),
                                Err(error) => {
                                    assert_eq!(error.sqlstate, sqlstate::PROGRAM_LIMIT_EXCEEDED);
                                    assert_eq!(actions.get(), before);
                                    break;
                                }
                            }
                            assert_eq!(live, &[0xa5u8; 128]);
                        }
                        assert!(actions.get() > 0);
                        assert_eq!(live, &[0xa5u8; 128]);
                    })
                    .unwrap();
            }
            {
                let statement = enter_statement(&arena, session, true);
                let changed = SequenceCall {
                    operation: SequenceOperation::Set {
                        value: 5,
                        is_called: false,
                    },
                    source: None,
                };
                let error =
                    statement_call(session, changed, || panic!("changed call must be rejected"))
                        .unwrap_err();
                assert_eq!(error.sqlstate, sqlstate::SERIALIZATION_FAILURE);
                // Keep the original records to also exercise a stale arena.
                drop(statement);
            }
            arena.reset();
            let statement = enter_statement(&arena, session, true);
            let error = statement_call(session, call, || panic!("reset arena must be rejected"))
                .unwrap_err();
            assert_eq!(error.sqlstate, sqlstate::SERIALIZATION_FAILURE);
            statement.discard(session);
        });
    }

    #[test]
    fn replay_state_scales_with_the_statement_arena_and_replays_exactly() {
        let mut budget = Budget::new(1 << 20);
        let arena = Arena::new(&mut budget, "sequence replay", 1 << 16).unwrap();
        let replay = SequenceReplayState::empty();
        let row_mark = arena.mark();
        let calls = Cell::new(0i64);
        crate::mem::guard::forbid_alloc(|| {
            for expected in 1..=1_100i64 {
                let _row_scratch = arena.alloc_slice_with(16, |_| 0xa5u8).unwrap();
                assert_eq!(
                    replay
                        .invoke(&arena, || {
                            calls.set(calls.get() + 1);
                            Ok(calls.get())
                        })
                        .unwrap(),
                    expected
                );
                unsafe { arena.rewind_to(row_mark) };
            }
            replay.begin_attempt();
            for expected in 1..=1_100i64 {
                let _row_scratch = arena.alloc_slice_with(16, |_| 0x5au8).unwrap();
                assert_eq!(
                    replay
                        .invoke(&arena, || panic!("a replayed call must not run again"))
                        .unwrap(),
                    expected
                );
                unsafe { arena.rewind_to(row_mark) };
            }
        });
        assert_eq!(calls.get(), 1_100);
    }

    #[test]
    fn replay_state_reports_statement_memory_exhaustion() {
        let mut budget = Budget::new(1024);
        let arena = Arena::new(&mut budget, "small sequence replay", 256).unwrap();
        let replay = SequenceReplayState::empty();
        let calls = Cell::new(0i64);
        crate::mem::guard::forbid_alloc(|| {
            loop {
                let before = calls.get();
                match replay.invoke(&arena, || {
                    calls.set(before + 1);
                    Ok(before)
                }) {
                    Ok(value) => assert_eq!(value, before),
                    Err(error) => {
                        assert_eq!(error.sqlstate, sqlstate::PROGRAM_LIMIT_EXCEEDED);
                        assert!(error.message.as_str().contains("statement memory"));
                        assert_eq!(calls.get(), before);
                        break;
                    }
                }
            }
            assert!(calls.get() > 0);
            replay.begin_attempt();
            for expected in 0..calls.get() {
                assert_eq!(
                    replay
                        .invoke(&arena, || panic!("replay must not issue another value"))
                        .unwrap(),
                    expected
                );
            }
        });
    }
    #[test]
    fn cursor_owned_query_sequence_positions_survive_moves_and_retries() {
        let mut budget = Budget::new(32 << 10);
        let mut query = crate::sql::cursor::CursorQuery::new(&mut budget, 16 << 10).unwrap();
        let calls = Cell::new(0);
        let prefix = SEQUENCE_VALUES_PER_CHUNK as i64 + 3;
        crate::mem::guard::forbid_alloc(|| {
            query
                .bind("SELECT $1", &[crate::sql::types::Datum::Null], &[])
                .unwrap();
            query
                .with_query(|_, _, arena| {
                    for expected in 1..=prefix {
                        let value = query
                            .sequence_replay
                            .invoke(arena, || {
                                calls.set(calls.get() + 1);
                                Ok(calls.get())
                            })
                            .unwrap();
                        assert_eq!(value, expected);
                    }
                })
                .unwrap();
        });
        let mut moved = query;
        crate::mem::guard::forbid_alloc(|| {
            moved.sequence_replay.begin_attempt();
            moved
                .with_query(|_, _, arena| {
                    for expected in 1..=prefix {
                        assert_eq!(
                            moved
                                .sequence_replay
                                .invoke(arena, || panic!("issued value must not repeat"))
                                .unwrap(),
                            expected
                        );
                    }
                    assert_eq!(
                        moved
                            .sequence_replay
                            .invoke(arena, || {
                                calls.set(calls.get() + 1);
                                Ok(calls.get())
                            })
                            .unwrap(),
                        prefix + 1
                    );
                    moved.sequence_replay.begin_attempt();
                    for expected in 1..=prefix + 1 {
                        assert_eq!(
                            moved
                                .sequence_replay
                                .invoke(arena, || panic!("resumed values must not repeat"))
                                .unwrap(),
                            expected
                        );
                    }
                })
                .unwrap();
            assert_eq!(calls.get(), prefix + 1);
            moved.clear();
            moved.bind("SELECT 1", &[], &[]).unwrap();
            moved
                .with_query(|_, _, arena| {
                    assert_eq!(
                        moved
                            .sequence_replay
                            .invoke(arena, || {
                                calls.set(calls.get() + 1);
                                Ok(calls.get())
                            })
                            .unwrap(),
                        prefix + 2
                    );
                })
                .unwrap();
        });
    }

    #[test]
    fn cursor_owned_query_sequence_positions_reject_reset_or_replaced_arena() {
        let mut budget = Budget::new(8192);
        let arena = Arena::new(&mut budget, "sequence positions", 4096).unwrap();
        let other = Arena::new(&mut budget, "other sequence positions", 4096).unwrap();
        let replay = SequenceReplayState::empty();
        crate::mem::guard::forbid_alloc(|| {
            replay.invoke(&arena, || Ok(1)).unwrap();
            replay.begin_attempt();
            let error = replay
                .invoke(&other, || panic!("wrong arena must be rejected"))
                .unwrap_err();
            assert_eq!(error.sqlstate, sqlstate::SERIALIZATION_FAILURE);
            let mut moved = arena;
            assert_eq!(
                replay
                    .invoke(&moved, || panic!("a move keeps issued values"))
                    .unwrap(),
                1
            );
            replay.begin_attempt();
            moved.reset();
            let error = replay
                .invoke(&moved, || panic!("reset must be rejected"))
                .unwrap_err();
            assert_eq!(error.sqlstate, sqlstate::SERIALIZATION_FAILURE);
        });
    }
}
