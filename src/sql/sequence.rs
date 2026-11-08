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
use core::cell::{Cell, RefCell};
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
    next: Option<EffectHandle>,
}

#[derive(Clone, Copy)]
struct ReadyInputEffect {
    plan: super::type_input_plans::PlanId,
    next: Option<EffectHandle>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum EffectHandle {
    Full(ArenaHandle<StatementEffect>),
    ReadyInput(ArenaHandle<ReadyInputEffect>),
}

impl EffectHandle {
    fn next(self, arena: &Arena) -> Result<Option<Self>, SqlError> {
        match self {
            Self::Full(handle) => arena.retained(handle).map(|node| node.next),
            Self::ReadyInput(handle) => arena.retained(handle).map(|node| node.next),
        }
        .ok_or_else(replay_changed)
    }

    fn set_next(self, arena: &Arena, next: Self) -> Result<(), SqlError> {
        let replaced = match self {
            Self::Full(handle) => {
                let node = arena.retained(handle).ok_or_else(replay_changed)?;
                arena.replace_retained(
                    handle,
                    StatementEffect {
                        next: Some(next),
                        ..node
                    },
                )
            }
            Self::ReadyInput(handle) => {
                let node = arena.retained(handle).ok_or_else(replay_changed)?;
                arena.replace_retained(
                    handle,
                    ReadyInputEffect {
                        next: Some(next),
                        ..node
                    },
                )
            }
        };
        if replaced {
            Ok(())
        } else {
            Err(replay_changed())
        }
    }
}

#[derive(Clone, Copy)]
pub(crate) enum StatementEffectRetry {
    Restart,
    Continue,
}

#[derive(Clone, Copy, PartialEq, Eq)]
// The persistent arena handle distinguishes statements even when the same
// input site is admitted repeatedly or the arena later reuses an offset.
pub(crate) struct StatementInputAttempt(ArenaHandle<u8>);

#[derive(Clone, Copy)]
pub(super) struct StatementSequenceReplay {
    input_attempt: Option<StatementInputAttempt>,
    journal: Option<EffectHandle>,
    head: Option<ArenaHandle<SequenceEffect>>,
    tail: Option<ArenaHandle<SequenceEffect>>,
    next: Option<ArenaHandle<SequenceEffect>>,
    initial_lastval: Option<(usize, u64)>,
    effects_head: Option<EffectHandle>,
    effects_tail: Option<EffectHandle>,
    effects_next: Option<EffectHandle>,
    nonreplayable: bool,
}

impl StatementSequenceReplay {
    pub(super) fn is_retained(self) -> bool {
        self.input_attempt.is_some() || self.head.is_some() || self.effects_head.is_some()
    }

    pub(super) const EMPTY: Self = Self {
        input_attempt: None,
        journal: None,
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
    static STATEMENT_ADVISORY: RefCell<Option<crate::storage::AdvisoryJournal>> = const { RefCell::new(None) };
}

pub(super) struct StatementSequenceScope<'arena, 'session> {
    previous: *const Arena,
    previous_session: *const SeqSession,
    previous_advisory: Option<crate::storage::AdvisoryJournal>,
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
        if self.previous.is_null() {
            STATEMENT_ADVISORY.with(|active| {
                *active.borrow_mut() = self.previous_advisory.take();
            });
        }
        STATEMENT_ARENA.with(|active| active.set(self.previous));
        STATEMENT_SESSION.with(|active| active.set(self.previous_session));
    }
}

pub(super) fn enter_statement<'arena, 'session>(
    arena: &'arena Arena,
    session: &'session SeqSession,
    resumed: bool,
    advisory: Option<crate::storage::AdvisoryJournal>,
) -> StatementSequenceScope<'arena, 'session> {
    let arena = arena.retention_owner();
    let previous = STATEMENT_ARENA.with(|active| active.get());
    if !previous.is_null() {
        return StatementSequenceScope {
            previous,
            previous_session: STATEMENT_SESSION.with(Cell::get),
            previous_advisory: None,
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
        previous_advisory: STATEMENT_ADVISORY.with(|active| active.replace(advisory)),
        arena: PhantomData,
    }
}

pub(crate) fn finish_statement(session: &SeqSession) {
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
        "a finished operation cannot be repeated after a statement wait"
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

pub(crate) fn record_caught_cancellation(session: &SeqSession) {
    let mut replay = session.statement_replay.get();
    replay.nonreplayable = true;
    session.statement_replay.set(replay);
    super::query::protect_statement_effects();
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

pub(crate) fn input_attempt() -> Option<StatementInputAttempt> {
    with_statement_context(|context| {
        context.and_then(|(_, session)| session.statement_replay.get().input_attempt)
    })
}

pub(crate) fn retains_input_attempt(attempt: StatementInputAttempt) -> bool {
    with_statement_context(|context| {
        context.is_some_and(|(arena, session)| {
            session.statement_replay.get().input_attempt == Some(attempt)
                && arena.retained(attempt.0).is_some()
        })
    })
}

#[derive(Clone, Copy)]
enum ReplayPosition<T: Copy> {
    Start,
    Before(ArenaHandle<T>),
    After(ArenaHandle<T>),
}

#[derive(Clone, Copy)]
enum EffectPosition {
    Start,
    Before(EffectHandle),
    After(EffectHandle),
}

impl EffectPosition {
    fn capture(next: Option<EffectHandle>, tail: Option<EffectHandle>) -> Self {
        match (next, tail) {
            (Some(next), _) => Self::Before(next),
            (None, Some(tail)) => Self::After(tail),
            (None, None) => Self::Start,
        }
    }
}

impl<T: Copy> ReplayPosition<T> {
    fn capture(next: Option<ArenaHandle<T>>, tail: Option<ArenaHandle<T>>) -> Self {
        match (next, tail) {
            (Some(next), _) => Self::Before(next),
            (None, Some(tail)) => Self::After(tail),
            (None, None) => Self::Start,
        }
    }
}

#[derive(Clone, Copy)]
pub(crate) struct StatementReplayStart {
    attempt: StatementInputAttempt,
    journal: Option<EffectHandle>,
    sequences: ReplayPosition<SequenceEffect>,
    effects: EffectPosition,
    lastval: Option<(usize, u64)>,
    advisory: Option<super::lock::AdvisoryStatementPosition>,
}

#[derive(Clone, Copy)]
pub(crate) struct StatementReplayCursor(Option<StatementReplayEnd>);

/// A paused invocation's position in its connection-owned statement journal.
/// The statement arena and sequence session remain with that connection.
pub(crate) struct SavedStatementPosition(Option<StatementReplayStart>);

pub(crate) struct StatementPositionAdmission<'session> {
    session: Option<&'session SeqSession>,
    prior: StatementSequenceReplay,
    lastval: Option<(usize, u64)>,
    committed: bool,
    advisory: Option<crate::storage::AdvisoryPositionAdmission>,
}

impl StatementPositionAdmission<'_> {
    /// Commit only after every part of the invocation has attached, before SQL
    /// can change the journal. Failed attachment restores the caller's cursor.
    pub(crate) fn commit(&mut self) {
        self.committed = true;
        if let Some(advisory) = &mut self.advisory {
            advisory.commit();
        }
    }
}

impl Drop for StatementPositionAdmission<'_> {
    fn drop(&mut self) {
        if !self.committed
            && let Some(session) = self.session
        {
            session.statement_replay.set(self.prior);
            session.restore_last_sequence(self.lastval);
        }
    }
}

impl SavedStatementPosition {
    pub(crate) fn capture() -> Result<Self, SqlError> {
        begin_replay_cursor().map(Self)
    }

    pub(crate) fn take(&mut self) -> Self {
        Self(self.0.take())
    }

    #[expect(
        clippy::result_large_err,
        reason = "failed admission returns its original unattached owner without allocating"
    )]
    pub(crate) fn with_resume<T, R>(
        &self,
        owner: T,
        tail: super::lock::AdvisoryPositionTail,
        execute: impl for<'session> FnOnce(T, &mut StatementPositionAdmission<'session>) -> R,
    ) -> Result<R, (T, SqlError)> {
        with_statement_context(|context| {
            let mut admission = StatementPositionAdmission {
                session: None,
                prior: StatementSequenceReplay::EMPTY,
                lastval: None,
                committed: false,
                advisory: None,
            };
            match (self.0, context) {
                (None, None) => {}
                (Some(saved), Some((arena, session))) => {
                    let mut replay = session.statement_replay.get();
                    let positions = (|| {
                        validate_cursor(arena, replay, saved)?;
                        Ok((
                            sequence_position(arena, replay, saved.sequences)?,
                            effect_position(arena, replay, saved.effects)?,
                        ))
                    })();
                    let (next, effects_next) = match positions {
                        Ok(positions) => positions,
                        Err(error) => return Err((owner, error)),
                    };
                    admission.advisory = match admit_advisory(saved.advisory, tail) {
                        Ok(advisory) => advisory,
                        Err(error) => return Err((owner, error)),
                    };
                    admission.session = Some(session);
                    admission.prior = replay;
                    admission.lastval = session.last_sequence();
                    replay.next = next;
                    replay.effects_next = effects_next;
                    session.statement_replay.set(replay);
                    session.restore_last_sequence(saved.lastval);
                }
                _ => return Err((owner, replay_changed())),
            }
            Ok(execute(owner, &mut admission))
        })
    }
}

impl StatementReplayCursor {
    pub(crate) const EMPTY: Self = Self(None);

    pub(crate) const fn is_empty(self) -> bool {
        self.0.is_none()
    }
}

#[derive(Clone, Copy)]
struct StatementReplayEnd {
    boundary: StatementReplayStart,
    values: Option<ArenaSliceHandle<SequenceSessionValue>>,
}

fn cursor_boundary(
    session: &SeqSession,
    replay: StatementSequenceReplay,
) -> Result<StatementReplayStart, SqlError> {
    Ok(StatementReplayStart {
        attempt: replay.input_attempt.ok_or_else(replay_changed)?,
        journal: replay.journal,
        sequences: ReplayPosition::capture(replay.next, replay.tail),
        effects: EffectPosition::capture(replay.effects_next, replay.effects_tail),
        lastval: session.last_sequence(),
        advisory: STATEMENT_ADVISORY
            .with(|active| active.borrow().as_ref().map(|journal| journal.position())),
    })
}

fn admit_advisory(
    saved: Option<super::lock::AdvisoryStatementPosition>,
    tail: super::lock::AdvisoryPositionTail,
) -> Result<Option<crate::storage::AdvisoryPositionAdmission>, SqlError> {
    STATEMENT_ADVISORY.with(|active| match (saved, active.borrow().as_ref()) {
        (None, None) => Ok(None),
        (Some(saved), Some(journal)) => journal.resume_position(saved, tail).map(Some),
        _ => Err(replay_changed()),
    })
}

fn validate_cursor(
    arena: &Arena,
    replay: StatementSequenceReplay,
    boundary: StatementReplayStart,
) -> Result<(), SqlError> {
    if replay.input_attempt != Some(boundary.attempt)
        || replay.journal != boundary.journal
        || arena.retained(boundary.attempt.0).is_none()
    {
        return Err(replay_changed());
    }
    Ok(())
}

fn sequence_position(
    arena: &Arena,
    replay: StatementSequenceReplay,
    position: ReplayPosition<SequenceEffect>,
) -> Result<Option<ArenaHandle<SequenceEffect>>, SqlError> {
    match position {
        ReplayPosition::Start => Ok(replay.head),
        ReplayPosition::Before(handle) => arena
            .retained(handle)
            .map(|_| Some(handle))
            .ok_or_else(replay_changed),
        ReplayPosition::After(handle) => arena
            .retained(handle)
            .map(|effect| effect.next)
            .ok_or_else(replay_changed),
    }
}

fn effect_position(
    arena: &Arena,
    replay: StatementSequenceReplay,
    position: EffectPosition,
) -> Result<Option<EffectHandle>, SqlError> {
    match position {
        EffectPosition::Start => Ok(replay.effects_head),
        EffectPosition::Before(handle) => handle.next(arena).map(|_| Some(handle)),
        EffectPosition::After(handle) => handle.next(arena),
    }
}

pub(crate) fn begin_replay_cursor() -> Result<Option<StatementReplayStart>, SqlError> {
    with_statement_context(|context| {
        let Some((arena, session)) = context else {
            return Ok(None);
        };
        let mut replay = session.statement_replay.get();
        if replay.input_attempt.is_none() {
            replay.input_attempt = Some(StatementInputAttempt(
                arena.retain(0u8).map_err(|_| effect_full())?,
            ));
            session.statement_replay.set(replay);
        }
        let boundary = cursor_boundary(session, replay)?;
        validate_cursor(arena, replay, boundary)?;
        Ok(Some(boundary))
    })
}

pub(crate) fn finish_replay_cursor(
    start: Option<StatementReplayStart>,
    scratch: &Arena,
) -> Result<StatementReplayCursor, SqlError> {
    with_statement_context(|context| {
        let (Some(start), Some((arena, session))) = (start, context) else {
            return if start.is_none() && context.is_none() {
                Ok(StatementReplayCursor(None))
            } else {
                Err(replay_changed())
            };
        };
        let replay = session.statement_replay.get();
        validate_cursor(arena, replay, start)?;
        let boundary = cursor_boundary(session, replay)?;
        let mut values = ArenaList::new(scratch);
        {
            let mut add = |slot, created_at| -> Result<(), SqlError> {
                if !values
                    .borrowed_slice()
                    .iter()
                    .any(|value: &SequenceSessionValue| {
                        value.slot == slot && value.created_at == created_at
                    })
                    && let Some(value) = session.currval(slot, created_at)
                {
                    values
                        .push(SequenceSessionValue {
                            slot,
                            created_at,
                            value,
                        })
                        .map_err(|_| effect_full())?;
                }
                Ok(())
            };
            let stop = sequence_position(arena, replay, boundary.sequences)?;
            let mut next = sequence_position(arena, replay, start.sequences)?;
            while next != stop {
                let effect = arena
                    .retained(next.ok_or_else(replay_changed)?)
                    .ok_or_else(replay_changed)?;
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
                    add(slot, created_at)?;
                }
                next = effect.next;
            }
            let stop = effect_position(arena, replay, boundary.effects)?;
            let mut next = effect_position(arena, replay, start.effects)?;
            while next != stop {
                let handle = next.ok_or_else(replay_changed)?;
                if let EffectHandle::Full(full) = handle {
                    let effect = arena.retained(full).ok_or_else(replay_changed)?;
                    if let Some(saved) = effect.after_sequences {
                        let saved = arena
                            .retained_slice_mut(saved)
                            .map_err(|_| replay_changed())?;
                        for value in saved.iter() {
                            add(value.slot, value.created_at)?;
                        }
                    }
                }
                next = handle.next(arena)?;
            }
        }
        let values = if values.is_empty() {
            None
        } else {
            let values = values.as_slice();
            Some(
                arena
                    .retain_slice_with(values.len(), |index| values[index])
                    .map_err(|_| effect_full())?,
            )
        };
        Ok(StatementReplayCursor(Some(StatementReplayEnd {
            boundary,
            values,
        })))
    })
}

pub(crate) fn rewind_replay_cursor(start: Option<StatementReplayStart>) -> Result<(), SqlError> {
    with_statement_context(|context| {
        let (Some(start), Some((arena, session))) = (start, context) else {
            return if start.is_none() && context.is_none() {
                Ok(())
            } else {
                Err(replay_changed())
            };
        };
        let mut replay = session.statement_replay.get();
        validate_cursor(arena, replay, start)?;
        replay.next = sequence_position(arena, replay, start.sequences)?;
        replay.effects_next = effect_position(arena, replay, start.effects)?;
        let mut advisory =
            admit_advisory(start.advisory, super::lock::AdvisoryPositionTail::Appended)?;
        session.restore_last_sequence(start.lastval);
        session.statement_replay.set(replay);
        if let Some(advisory) = &mut advisory {
            advisory.commit();
        }
        Ok(())
    })
}

pub(crate) fn restore_replay_cursor(cursor: StatementReplayCursor) -> Result<(), SqlError> {
    with_statement_context(|context| {
        let (Some(end), Some((arena, session))) = (cursor.0, context) else {
            return if cursor.0.is_none() && context.is_none() {
                Ok(())
            } else {
                Err(replay_changed())
            };
        };
        let mut replay = session.statement_replay.get();
        validate_cursor(arena, replay, end.boundary)?;
        replay.next = sequence_position(arena, replay, end.boundary.sequences)?;
        replay.effects_next = effect_position(arena, replay, end.boundary.effects)?;
        let mut advisory = admit_advisory(
            end.boundary.advisory,
            super::lock::AdvisoryPositionTail::Appended,
        )?;
        if let Some(values) = end.values {
            let values = arena
                .retained_slice_mut(values)
                .map_err(|_| replay_changed())?;
            for value in values.iter() {
                session.record_setval(value.slot, value.created_at, value.value, true);
            }
        }
        session.restore_last_sequence(end.boundary.lastval);
        session.statement_replay.set(replay);
        if let Some(advisory) = &mut advisory {
            advisory.commit();
        }
        Ok(())
    })
}

#[derive(Clone, Copy)]
pub(crate) enum ExpectedInput {
    None,
    Plan(super::type_input_plans::PlanId),
    OtherEffect,
}

pub(crate) fn expected_input() -> Result<ExpectedInput, SqlError> {
    with_statement_context(|context| {
        let Some((arena, session)) = context else {
            return Ok(ExpectedInput::None);
        };
        let Some(handle) = session.statement_replay.get().effects_next else {
            return Ok(ExpectedInput::None);
        };
        let handle = match handle {
            EffectHandle::ReadyInput(handle) => {
                return arena
                    .retained(handle)
                    .map(|effect| ExpectedInput::Plan(effect.plan))
                    .ok_or_else(replay_changed);
            }
            EffectHandle::Full(handle) => handle,
        };
        let effect = arena.retained(handle).ok_or_else(replay_changed)?;
        if effect.kind != StatementEffectKind::RoutineInput {
            return Ok(ExpectedInput::OtherEffect);
        }
        let key = arena
            .retained_slice_mut(effect.key)
            .map_err(|_| replay_changed())?;
        match super::exec::decode_projected_col_record(&key, 0, arena)? {
            super::types::Datum::Bytea(bytes) => {
                super::type_input_plans::PlanId::from_key_bytes(bytes).map(ExpectedInput::Plan)
            }
            _ => Err(replay_changed()),
        }
    })
}

pub(crate) fn replayed_input_plan() -> Result<Option<super::type_input_plans::PlanId>, SqlError> {
    match expected_input()? {
        ExpectedInput::None => Ok(None),
        ExpectedInput::Plan(plan) => Ok(Some(plan)),
        ExpectedInput::OtherEffect => Err(replay_changed()),
    }
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
            diagnostic
                .detail
                .map(|detail| copy(detail.as_str()))
                .transpose()?,
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
    let detail = if let Some(detail) = saved.detail {
        let detail = arena
            .retained_slice_mut(detail)
            .map_err(|_| replay_changed())?;
        Some(crate::util::StackStr::from_str(
            core::str::from_utf8(&detail).map_err(|_| replay_changed())?,
        ))
    } else {
        None
    };
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
    if detail.is_some() || hint.is_some() {
        super::eval::stash_diagnostic(detail, hint);
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
            if let EffectHandle::Full(full) = handle {
                let effect = arena.retained(full).ok_or_else(replay_changed)?;
                pending.push(effect.children).map_err(|_| effect_full())?;
            }
            nested = handle.next(arena)?;
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

/// Ready images run no conversion or CHECK. Their compact receipts retain the
/// exact generation; a replay may still encounter its original full admission.
pub(crate) fn ready_routine_input(
    plan: super::type_input_plans::PlanId,
    output: &Arena,
) -> Result<(), SqlError> {
    with_statement_context(|context| {
        let Some((arena, session)) = context else {
            return Ok(());
        };
        let mut replay = session.statement_replay.get();
        if let Some(handle) = replay.effects_next {
            match handle {
                EffectHandle::Full(_) => {
                    let bytes = plan.key_bytes();
                    return statement_effect(
                        StatementEffectKind::RoutineInput,
                        &[super::types::Datum::Bytea(&bytes)],
                        StatementEffectRetry::Restart,
                        output,
                        || StatementEffectAttempt {
                            result: Err(replay_changed()),
                            rewound_state: false,
                        },
                    )
                    .map(|_| ());
                }
                EffectHandle::ReadyInput(handle) => {
                    let effect = arena.retained(handle).ok_or_else(replay_changed)?;
                    if effect.plan != plan {
                        return Err(replay_changed());
                    }
                    replay.effects_next = effect.next;
                }
            }
        } else {
            if replay.input_attempt.is_none() {
                replay.input_attempt = Some(StatementInputAttempt(
                    arena.retain(0u8).map_err(|_| effect_full())?,
                ));
            }
            let handle = EffectHandle::ReadyInput(
                arena
                    .retain(ReadyInputEffect { plan, next: None })
                    .map_err(|_| effect_full())?,
            );
            if let Some(tail) = replay.effects_tail {
                tail.set_next(arena, handle)?;
            } else {
                replay.effects_head = Some(handle);
            }
            replay.effects_tail = Some(handle);
        }
        session.statement_replay.set(replay);
        Ok(())
    })
}

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
        if kind == StatementEffectKind::RoutineInput && parent.input_attempt.is_none() {
            parent.input_attempt = Some(StatementInputAttempt(
                arena.retain(0u8).map_err(|_| effect_full())?,
            ));
        }
        let (handle, mut effect) = if let Some(handle) = parent.effects_next {
            let EffectHandle::Full(handle) = handle else {
                return Err(replay_changed());
            };
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
                    input_attempt: parent.input_attempt,
                    initial_lastval: session.last_sequence(),
                    ..StatementSequenceReplay::EMPTY
                },
                after_lastval: None,
                after_sequences: None,
                next: None,
            };
            let handle = arena.retain(effect).map_err(|_| effect_full())?;
            if let Some(tail) = parent.effects_tail {
                tail.set_next(arena, EffectHandle::Full(handle))?;
            } else {
                parent.effects_head = Some(EffectHandle::Full(handle));
            }
            parent.effects_tail = Some(EffectHandle::Full(handle));
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
        child.journal = Some(EffectHandle::Full(handle));
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
        parent.input_attempt = parent.input_attempt.or(effect.children.input_attempt);
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
                        error.sqlstate.internal_control(),
                        Some(sqlstate::INTERNAL_IO_WAIT | sqlstate::INTERNAL_LOCK_WAIT)
                    ) =>
            {
                Err(nonreplayable_effect())
            }
            result => result,
        };
        let recorded = (|| {
            match &result {
                Ok(values) => {
                    effect.after_sequences =
                        retain_sequence_session(arena, output, session, effect.children)?;
                    effect.result = Some(StatementEffectResult::Values(retained_effect_bytes(
                        arena, values,
                    )?));
                }
                Err(error)
                    if matches!(
                        kind,
                        StatementEffectKind::Cursor | StatementEffectKind::RoutineInput
                    ) && !matches!(
                        error.sqlstate.internal_control(),
                        Some(
                            sqlstate::INTERNAL_IO_WAIT
                                | sqlstate::INTERNAL_LOCK_WAIT
                                | sqlstate::INTERNAL_ROUTINE_INVOCATION
                        )
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
            Ok(())
        })();
        if let Err(error) = recorded {
            parent.nonreplayable = true;
            session.statement_replay.set(parent);
            return Err(error);
        }
        result
    })
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mem::budget::Budget;

    #[test]
    fn retained_statement_position_restores_nested_advisory_receipts_atomically() {
        use crate::sql::lock::{AdvisoryDecision, AdvisoryKey, AdvisoryMode, AdvisoryPositionTail};
        let mut config = crate::storage::tests::test_config();
        config.max_tables = 1;
        config.max_catalog_versions_per_object = 1;
        config.txn_rows = 16;
        config.max_ddl_per_transaction = 2;
        config.row_history_slots = 16;
        config.row_overlay_slots = 16;
        config.catalog_history_bytes = 4096;
        config.definition_image_slots = 4;
        config.table_metadata_bytes = 64 << 10;
        config.table_metadata_leases = 64;
        config.execution_control_bytes = 64 << 10;
        config.execution_control_leases = 64;
        config.row_work_bytes = 64 << 10;
        config.row_work_leases = 32;
        let mut budget = crate::storage::tests::test_budget(&config);
        let storage = Storage::new(&config, &mut budget, 0).unwrap();
        let arena = Arena::new(&mut budget, "combined statement receipts", 16 << 10).unwrap();
        let foreign = Arena::new(&mut budget, "foreign statement receipts", 4096).unwrap();
        let guc = crate::sql::guc::GucState::new();
        let prior = crate::sql::execution_identity();
        crate::sql::set_execution_connection_id(51);
        crate::mem::guard::forbid_alloc(|| {
            storage.begin_advisory_statement().unwrap();
            let scope = enter_statement(
                &arena,
                guc.seq_session(),
                false,
                Some(storage.advisory_journal()),
            );
            let start = begin_replay_cursor().unwrap();
            let advisory_start = storage.advisory_journal().position();
            let acquire = |key| {
                assert_eq!(
                    storage
                        .acquire_advisory_lock(
                            100,
                            AdvisoryKey::from_bigint(key),
                            AdvisoryMode::Exclusive,
                            false,
                            false
                        )
                        .unwrap(),
                    AdvisoryDecision::Acquired
                );
            };
            acquire(1);
            let completed = finish_replay_cursor(start, &arena).unwrap();
            acquire(2);
            rewind_replay_cursor(start).unwrap();
            storage
                .advisory_journal()
                .resume_position(advisory_start, AdvisoryPositionTail::Appended)
                .unwrap()
                .commit();
            let before = storage.advisory_statement_mark();
            let mut broken = completed;
            broken.0.as_mut().unwrap().values = Some(
                foreign
                    .retain_slice_with(1, |_| SequenceSessionValue {
                        slot: 0,
                        created_at: 1,
                        value: 7,
                    })
                    .unwrap(),
            );
            assert_eq!(
                restore_replay_cursor(broken).unwrap_err().sqlstate,
                sqlstate::SERIALIZATION_FAILURE
            );
            assert_eq!(storage.advisory_statement_mark(), before);
            restore_replay_cursor(completed).unwrap();
            acquire(2);
            for key in [1, 2] {
                assert!(
                    storage
                        .unlock_advisory_session(
                            AdvisoryKey::from_bigint(key),
                            AdvisoryMode::Exclusive
                        )
                        .unwrap()
                );
                assert!(
                    !storage
                        .unlock_advisory_session(
                            AdvisoryKey::from_bigint(key),
                            AdvisoryMode::Exclusive
                        )
                        .unwrap()
                );
            }
            scope.finish(guc.seq_session()).unwrap();
            drop(scope);
            storage.begin_advisory_statement().unwrap();
            let scope = enter_statement(
                &arena,
                guc.seq_session(),
                false,
                Some(storage.advisory_journal()),
            );
            let before = storage.advisory_statement_mark();
            assert_eq!(
                restore_replay_cursor(completed).unwrap_err().sqlstate,
                sqlstate::SERIALIZATION_FAILURE
            );
            assert_eq!(storage.advisory_statement_mark(), before);
            scope.finish(guc.seq_session()).unwrap();
        });
        crate::sql::publish_execution_identity(prior);
    }

    #[test]
    fn retained_statement_position_admission_keeps_receipts_and_owners() {
        struct Owner<'a>(&'a Cell<usize>);
        impl Drop for Owner<'_> {
            fn drop(&mut self) {
                self.0.set(self.0.get() + 1);
            }
        }
        let mut budget = Budget::new(96 << 10);
        let arena = Arena::new(&mut budget, "statement position journal", 64 << 10).unwrap();
        let other_arena = Arena::new(&mut budget, "foreign position journal", 16 << 10).unwrap();
        let output = Arena::new(&mut budget, "statement position output", 16 << 10).unwrap();
        let guc = crate::sql::guc::GucState::new();
        let other = crate::sql::guc::GucState::new();
        let session = guc.seq_session();
        let calls = Cell::new(0);
        let drops = Cell::new(0);
        let mut plan_bytes = [0; 16];
        plan_bytes[0] = 1;
        plan_bytes[8] = 1;
        let plan = super::super::type_input_plans::PlanId::from_key_bytes(&plan_bytes).unwrap();
        let saved = crate::mem::guard::forbid_alloc(|| {
            let empty = SavedStatementPosition::capture().unwrap();
            assert_eq!(
                empty
                    .with_resume(
                        17,
                        super::super::lock::AdvisoryPositionTail::Unchanged,
                        |owner, admission| {
                            admission.commit();
                            owner
                        }
                    )
                    .unwrap(),
                17
            );
            let _scope = enter_statement(&arena, session, false, None);
            assert_eq!(
                statement_call(
                    session,
                    SequenceCall {
                        operation: SequenceOperation::Next,
                        source: None,
                    },
                    || {
                        calls.set(calls.get() + 1);
                        session.record_nextval(0, 1, 7);
                        Ok(7)
                    }
                )
                .unwrap(),
                7
            );
            ready_routine_input(plan, &output).unwrap();
            SavedStatementPosition::capture().unwrap()
        });
        let moved = arena;
        crate::mem::guard::forbid_alloc(|| {
            {
                let _scope = enter_statement(&other_arena, other.seq_session(), false, None);
                let (owner, error) = saved
                    .with_resume(
                        Owner(&drops),
                        super::super::lock::AdvisoryPositionTail::Unchanged,
                        |_, _| -> () { panic!("foreign statement cannot attach") },
                    )
                    .err()
                    .unwrap();
                assert_eq!(error.sqlstate, sqlstate::SERIALIZATION_FAILURE);
                assert_eq!(drops.get(), 0);
                drop(owner);
                assert_eq!(drops.get(), 1);
            }
            let scope = enter_statement(&moved, session, true, None);
            let before = session.statement_replay.get();
            let lastval = session.last_sequence();
            saved
                .with_resume(
                    (),
                    super::super::lock::AdvisoryPositionTail::Unchanged,
                    |(), _admission| {
                        assert!(session.statement_replay.get().next.is_none());
                        assert!(session.statement_replay.get().effects_next.is_none());
                        assert_eq!(session.last_sequence(), Some((0, 1)));
                    },
                )
                .unwrap();
            assert!(session.statement_replay.get().next == before.next);
            assert!(session.statement_replay.get().effects_next == before.effects_next);
            assert_eq!(session.last_sequence(), lastval);
            saved
                .with_resume(
                    (),
                    super::super::lock::AdvisoryPositionTail::Unchanged,
                    |(), admission| {
                        admission.commit();
                        assert!(session.statement_replay.get().next.is_none());
                        assert!(session.statement_replay.get().effects_next.is_none());
                        assert_eq!(session.currval(0, 1), Some(7));
                        assert_eq!(session.last_sequence(), Some((0, 1)));
                        assert_eq!(
                            statement_call(
                                session,
                                SequenceCall {
                                    operation: SequenceOperation::Next,
                                    source: None,
                                },
                                || {
                                    calls.set(calls.get() + 1);
                                    session.record_nextval(0, 1, 8);
                                    Ok(8)
                                }
                            )
                            .unwrap(),
                            8
                        );
                        ready_routine_input(plan, &output).unwrap();
                    },
                )
                .unwrap();
            assert_eq!(calls.get(), 2);
            assert_eq!(session.currval(0, 1), Some(8));
            scope.finish(session).unwrap();
            assert_eq!(
                saved
                    .with_resume(
                        (),
                        super::super::lock::AdvisoryPositionTail::Unchanged,
                        |(), _| ()
                    )
                    .err()
                    .unwrap()
                    .1
                    .sqlstate,
                sqlstate::SERIALIZATION_FAILURE
            );
        });
    }

    #[test]
    fn compact_ready_inputs_mix_with_full_errors_waits_and_scoped_cursors() {
        use crate::sql::{type_input_plans::PlanId, types::Datum};
        fn plan(slot: u64, generation: u64) -> PlanId {
            let mut bytes = [0; 16];
            bytes[..8].copy_from_slice(&slot.to_le_bytes());
            bytes[8..].copy_from_slice(&generation.to_le_bytes());
            PlanId::from_key_bytes(&bytes).unwrap()
        }
        assert!(core::mem::size_of::<ReadyInputEffect>() <= 64);
        let mut budget = Budget::new(64 << 10);
        let arena = Arena::new(&mut budget, "compact ready input receipts", 64 << 10).unwrap();
        let guc = crate::sql::guc::GucState::new();
        let session = guc.seq_session();
        let source = crate::storage::SequenceReplaySource::test_source(0, 1);
        let cold = plan(0, 1);
        let rejected = plan(1, 1);
        let waiting = plan(2, 1);
        let changed = plan(0, 2);
        let actions = Cell::new(0);
        crate::mem::guard::forbid_alloc(|| {
            let ready_range;
            {
                let _scope = enter_statement(&arena, session, false, None);
                let bytes = cold.key_bytes();
                statement_effect(
                    StatementEffectKind::RoutineInput,
                    &[Datum::Bytea(&bytes)],
                    StatementEffectRetry::Restart,
                    &arena,
                    || {
                        let value = statement_call(
                            session,
                            SequenceCall {
                                operation: SequenceOperation::Next,
                                source: Some(source),
                            },
                            || {
                                actions.set(actions.get() + 1);
                                Ok(7)
                            },
                        )
                        .unwrap();
                        session.record_nextval(0, 1, value);
                        StatementEffectAttempt {
                            result: Ok(&[]),
                            rewound_state: false,
                        }
                    },
                )
                .unwrap();
                let before = begin_replay_cursor().unwrap();
                for _ in 0..1100 {
                    ready_routine_input(cold, &arena).unwrap();
                }
                ready_range = finish_replay_cursor(before, &arena).unwrap();
                let bytes = rejected.key_bytes();
                let error = statement_effect(
                    StatementEffectKind::RoutineInput,
                    &[Datum::Bytea(&bytes)],
                    StatementEffectRetry::Restart,
                    &arena,
                    || {
                        let value = statement_call(
                            session,
                            SequenceCall {
                                operation: SequenceOperation::Next,
                                source: Some(source),
                            },
                            || {
                                actions.set(actions.get() + 1);
                                Ok(8)
                            },
                        )
                        .unwrap();
                        session.record_nextval(0, 1, value);
                        super::super::eval::stash_diagnostic(
                            Some(crate::util::StackStr::from_str("caught literal detail")),
                            None,
                        );
                        StatementEffectAttempt {
                            result: Err(sql_err!(sqlstate::CHECK_VIOLATION, "caught literal")),
                            rewound_state: false,
                        }
                    },
                )
                .unwrap_err();
                assert_eq!(error.sqlstate, sqlstate::CHECK_VIOLATION);
                assert_eq!(
                    super::super::eval::take_diagnostic()
                        .unwrap()
                        .detail
                        .unwrap()
                        .as_str(),
                    "caught literal detail"
                );
                let bytes = waiting.key_bytes();
                let error = statement_effect(
                    StatementEffectKind::RoutineInput,
                    &[Datum::Bytea(&bytes)],
                    StatementEffectRetry::Restart,
                    &arena,
                    || StatementEffectAttempt {
                        result: Err(sql_err!(sqlstate::INTERNAL_LOCK_WAIT, "row lock")),
                        rewound_state: false,
                    },
                )
                .unwrap_err();
                assert_eq!(error.sqlstate, sqlstate::INTERNAL_LOCK_WAIT);
            }
            {
                let scope = enter_statement(&arena, session, true, None);
                ready_routine_input(cold, &arena).unwrap();
                assert_eq!(session.currval(0, 1), Some(7));
                assert_eq!(replayed_input_plan().unwrap(), Some(cold));
                assert!(ready_routine_input(changed, &arena).is_err());
                restore_replay_cursor(ready_range).unwrap();
                assert_eq!(replayed_input_plan().unwrap(), Some(rejected));
                let bytes = rejected.key_bytes();
                let error = statement_effect(
                    StatementEffectKind::RoutineInput,
                    &[Datum::Bytea(&bytes)],
                    StatementEffectRetry::Restart,
                    &arena,
                    || panic!("the caught input error must be restored"),
                )
                .unwrap_err();
                assert_eq!(error.sqlstate, sqlstate::CHECK_VIOLATION);
                assert_eq!(session.currval(0, 1), Some(8));
                assert_eq!(
                    super::super::eval::take_diagnostic()
                        .unwrap()
                        .detail
                        .unwrap()
                        .as_str(),
                    "caught literal detail"
                );
                let bytes = waiting.key_bytes();
                statement_effect(
                    StatementEffectKind::RoutineInput,
                    &[Datum::Bytea(&bytes)],
                    StatementEffectRetry::Restart,
                    &arena,
                    || StatementEffectAttempt {
                        result: Ok(&[]),
                        rewound_state: false,
                    },
                )
                .unwrap();
                scope.finish(session).unwrap();
            }
            assert_eq!(actions.get(), 2);
            {
                let scope = enter_statement(&arena, session, false, None);
                ready_routine_input(cold, &arena).unwrap();
                scope.discard(session);
            }
            {
                let _scope = enter_statement(&arena, session, false, None);
                assert!(replayed_input_plan().unwrap().is_none());
                assert!(restore_replay_cursor(ready_range).is_err());
                ready_routine_input(changed, &arena).unwrap();
            }
            {
                let scope = enter_statement(&arena, session, true, None);
                assert_eq!(replayed_input_plan().unwrap(), Some(changed));
                assert!(ready_routine_input(cold, &arena).is_err());
                ready_routine_input(changed, &arena).unwrap();
                scope.finish(session).unwrap();
            }
        });
    }

    #[test]
    fn cursor_only_statements_keep_exact_identity_across_waits_and_finish() {
        let mut budget = Budget::new(8192);
        let mut arena = Arena::new(&mut budget, "cursor-only statement", 8192).unwrap();
        let guc = crate::sql::guc::GucState::new();
        let session = guc.seq_session();
        crate::mem::guard::forbid_alloc(|| {
            let cursor;
            {
                let _statement = enter_statement(&arena, session, false, None);
                cursor = finish_replay_cursor(begin_replay_cursor().unwrap(), &arena).unwrap();
                assert!(session.statement_replay.get().is_retained());
            }
            for _ in 0..3 {
                let _statement = enter_statement(&arena, session, true, None);
                restore_replay_cursor(cursor).unwrap();
            }
            {
                let statement = enter_statement(&arena, session, true, None);
                restore_replay_cursor(cursor).unwrap();
                statement.finish(session).unwrap();
                assert!(!session.statement_replay.get().is_retained());
            }
            {
                let statement = enter_statement(&arena, session, false, None);
                assert_eq!(
                    restore_replay_cursor(cursor).unwrap_err().sqlstate,
                    sqlstate::SERIALIZATION_FAILURE
                );
                let fresh = finish_replay_cursor(begin_replay_cursor().unwrap(), &arena).unwrap();
                restore_replay_cursor(fresh).unwrap();
                statement.finish(session).unwrap();
            }
            arena.reset();
            let statement = enter_statement(&arena, session, false, None);
            let fresh = finish_replay_cursor(begin_replay_cursor().unwrap(), &arena).unwrap();
            assert_eq!(
                restore_replay_cursor(cursor).unwrap_err().sqlstate,
                sqlstate::SERIALIZATION_FAILURE
            );
            restore_replay_cursor(fresh).unwrap();
            statement.finish(session).unwrap();
        });
    }

    #[test]
    fn cached_value_cursors_skip_exact_effect_ranges_and_restore_sparse_sessions() {
        use crate::sql::types::Datum;
        let mut budget = Budget::new(128 << 10);
        let mut arena = Arena::new(&mut budget, "cached value replay cursors", 96 << 10).unwrap();
        let wrong = Arena::new(&mut budget, "foreign cached value cursor", 32 << 10).unwrap();
        let guc = crate::sql::guc::GucState::new();
        let session = guc.seq_session();
        let source = crate::storage::SequenceReplaySource::test_source(0, 1);
        let nested_source = crate::storage::SequenceReplaySource::test_source(1, 2);
        let calls = Cell::new(0);
        crate::mem::guard::forbid_alloc(|| {
            let absent = finish_replay_cursor(begin_replay_cursor().unwrap(), &arena).unwrap();
            restore_replay_cursor(absent).unwrap();
            let first;
            let second;
            let start;
            {
                let scope = enter_statement(&arena, session, false, None);
                assert!(restore_replay_cursor(absent).is_err());
                start = begin_replay_cursor().unwrap();
                let value = statement_call(
                    session,
                    SequenceCall {
                        operation: SequenceOperation::Next,
                        source: Some(source),
                    },
                    || {
                        calls.set(calls.get() + 1);
                        Ok(10)
                    },
                )
                .unwrap();
                session.record_nextval(0, 1, value);
                first = finish_replay_cursor(start, &arena).unwrap();
                let before = begin_replay_cursor().unwrap();
                let value = statement_call(
                    session,
                    SequenceCall {
                        operation: SequenceOperation::Next,
                        source: Some(source),
                    },
                    || {
                        calls.set(calls.get() + 1);
                        Ok(11)
                    },
                )
                .unwrap();
                session.record_nextval(0, 1, value);
                statement_effect(
                    StatementEffectKind::Cursor,
                    &[Datum::Text("nested")],
                    StatementEffectRetry::Restart,
                    &arena,
                    || {
                        assert!(restore_replay_cursor(first).is_err());
                        let value = statement_call(
                            session,
                            SequenceCall {
                                operation: SequenceOperation::Next,
                                source: Some(nested_source),
                            },
                            || {
                                calls.set(calls.get() + 1);
                                Ok(21)
                            },
                        )
                        .unwrap();
                        session.record_nextval(1, 2, value);
                        StatementEffectAttempt {
                            result: Ok(&[]),
                            rewound_state: false,
                        }
                    },
                )
                .unwrap();
                second = finish_replay_cursor(before, &arena).unwrap();
                assert_eq!(
                    statement_call(
                        session,
                        SequenceCall {
                            operation: SequenceOperation::Current,
                            source: Some(source)
                        },
                        || session.currval(0, 1).ok_or_else(replay_changed)
                    )
                    .unwrap(),
                    11
                );
                rewind_replay_cursor(start).unwrap();
                restore_replay_cursor(first).unwrap();
                assert_eq!(session.currval(0, 1), Some(10));
                restore_replay_cursor(second).unwrap();
                assert_eq!(session.currval(0, 1), Some(11));
                assert_eq!(session.currval(1, 2), Some(21));
                assert_eq!(session.last_sequence(), Some((1, 2)));
                assert_eq!(
                    statement_call(
                        session,
                        SequenceCall {
                            operation: SequenceOperation::Current,
                            source: Some(source)
                        },
                        || panic!("the later sequence call keeps its own receipt")
                    )
                    .unwrap(),
                    11
                );
                scope.finish(session).unwrap();
            }
            assert_eq!(calls.get(), 3);
            {
                let scope = enter_statement(&wrong, session, false, None);
                begin_replay_cursor().unwrap();
                assert!(restore_replay_cursor(first).is_err());
                scope.discard(session);
            }
            arena.reset();
            {
                let scope = enter_statement(&arena, session, false, None);
                begin_replay_cursor().unwrap();
                assert!(restore_replay_cursor(first).is_err());
                scope.finish(session).unwrap();
            }
        });
    }

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
                let _scope = enter_statement(&arena, session, false, None);
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
                let scope = enter_statement(&arena, session, true, None);
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
                    let _scope = enter_statement(&arena, session, false, None);
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
                    let scope = enter_statement(&arena, session, true, None);
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
                let _scope = enter_statement(&arena, session, false, None);
                let error = statement_effect(
                    StatementEffectKind::Cursor,
                    &[Datum::Text("fetch")],
                    StatementEffectRetry::Continue,
                    &arena,
                    || {
                        super::super::eval::stash_diagnostic(
                            Some(crate::util::StackStr::from_str("one row required")),
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
                        .unwrap()
                        .as_str(),
                    "one row required"
                );
            }
            arena.recycle_front();
            {
                let scope = enter_statement(&arena, session, true, None);
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
                        .unwrap()
                        .as_str(),
                    "one row required"
                );
                scope.finish(session).unwrap();
            }
            for detail in [None, Some("")] {
                arena.reset();
                {
                    let _scope = enter_statement(&arena, session, false, None);
                    statement_effect(
                        StatementEffectKind::Cursor,
                        &[Datum::Text("optional fields")],
                        StatementEffectRetry::Continue,
                        &arena,
                        || {
                            super::super::eval::stash_diagnostic(
                                detail.map(crate::util::StackStr::from_str),
                                Some(crate::util::StackStr::from_str("hint without detail")),
                            );
                            StatementEffectAttempt {
                                result: Err(sql_err!(sqlstate::INSUFFICIENT_PRIVILEGE, "denied")),
                                rewound_state: false,
                            }
                        },
                    )
                    .unwrap_err();
                    let diagnostic = super::super::eval::take_diagnostic().unwrap();
                    assert_eq!(diagnostic.detail.as_ref().map(|text| text.as_str()), detail);
                }
                {
                    let scope = enter_statement(&arena, session, true, None);
                    statement_effect(
                        StatementEffectKind::Cursor,
                        &[Datum::Text("optional fields")],
                        StatementEffectRetry::Continue,
                        &arena,
                        || panic!("terminal diagnostic cannot execute twice"),
                    )
                    .unwrap_err();
                    let diagnostic = super::super::eval::take_diagnostic().unwrap();
                    assert_eq!(diagnostic.detail.as_ref().map(|text| text.as_str()), detail);
                    assert_eq!(diagnostic.hint.unwrap().as_str(), "hint without detail");
                    scope.finish(session).unwrap();
                }
            }
            arena.reset();
            {
                let scope = enter_statement(&arena, session, false, None);
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
                let _scope = enter_statement(&arena, session, false, None);
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
                let scope = enter_statement(&arena, session, true, None);
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
            let scope = enter_statement(&arena, session, false, None);
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
                    let _statement = enter_statement(row, session, false, None);
                    assert_eq!(
                        statement_call(session, call, || {
                            actions.set(actions.get() + 1);
                            Ok(7)
                        })
                        .unwrap(),
                        7
                    );
                    row.with_remaining_region(|nested| {
                        let _nested_statement = enter_statement(nested, session, false, None);
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
                let _retry = enter_statement(&arena, session, true, None);
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
            let complete = enter_statement(&arena, session, true, None);
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
                let _statement = enter_statement(&arena, session, false, None);
                let mark = arena.mark();
                assert_eq!(
                    statement_call(session, call, || {
                        actions.set(actions.get() + 1);
                        Ok(7)
                    })
                    .unwrap(),
                    7
                );
                let _nested = enter_statement(&arena, session, false, None);
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
                let _statement = enter_statement(&arena, session, true, None);
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
            let statement = enter_statement(&arena, session, true, None);
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
                let _statement = enter_statement(&arena, session, false, None);
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
                let statement = enter_statement(&arena, session, true, None);
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
            let statement = enter_statement(&arena, session, true, None);
            let error = statement_call(session, call, || panic!("reset arena must be rejected"))
                .unwrap_err();
            assert_eq!(error.sqlstate, sqlstate::SERIALIZATION_FAILURE);
            statement.discard(session);
        });
    }

    #[test]
    fn cursor_owned_query_sequence_positions_survive_moves_and_retries() {
        let mut budget = Budget::new(32 << 10);
        let mut query = crate::sql::cursor::CursorQuery::new(&mut budget, 16 << 10).unwrap();
        let guc = crate::sql::guc::GucState::new();
        let session = guc.seq_session();
        let calls = Cell::new(0);
        let call = SequenceCall {
            operation: SequenceOperation::Last,
            source: None,
        };
        let prefix = 19;
        crate::mem::guard::forbid_alloc(|| {
            query
                .bind(
                    "SELECT $1",
                    &[crate::sql::types::Datum::Null],
                    &[],
                    &[],
                    crate::sql::type_input_plans::Owner::Cursor(1),
                    |_| Ok(()),
                )
                .unwrap();
            query
                .with_query(|_, _, arena| {
                    let _statement = enter_statement(arena, session, false, None);
                    for expected in 1..=prefix {
                        assert_eq!(
                            statement_call(session, call, || {
                                calls.set(calls.get() + 1);
                                Ok(calls.get())
                            })
                            .unwrap(),
                            expected
                        );
                    }
                })
                .unwrap();
        });
        let mut moved = query;
        crate::mem::guard::forbid_alloc(|| {
            moved
                .with_query(|_, _, arena| {
                    let _statement = enter_statement(arena, session, true, None);
                    for expected in 1..=prefix {
                        assert_eq!(
                            statement_call(session, call, || panic!(
                                "issued value must not repeat"
                            ))
                            .unwrap(),
                            expected
                        );
                    }
                    assert_eq!(
                        statement_call(session, call, || {
                            calls.set(calls.get() + 1);
                            Ok(calls.get())
                        })
                        .unwrap(),
                        prefix + 1
                    );
                })
                .unwrap();
            moved
                .with_query(|_, _, arena| {
                    let statement = enter_statement(arena, session, true, None);
                    for expected in 1..=prefix + 1 {
                        assert_eq!(
                            statement_call(session, call, || panic!(
                                "resumed values must not repeat"
                            ))
                            .unwrap(),
                            expected
                        );
                    }
                    statement.finish(session).unwrap();
                })
                .unwrap();
            assert_eq!(calls.get(), prefix + 1);
            moved.clear();
            moved
                .bind(
                    "SELECT 1",
                    &[],
                    &[],
                    &[],
                    crate::sql::type_input_plans::Owner::Cursor(1),
                    |_| Ok(()),
                )
                .unwrap();
            moved
                .with_query(|_, _, arena| {
                    let statement = enter_statement(arena, session, false, None);
                    assert_eq!(
                        statement_call(session, call, || {
                            calls.set(calls.get() + 1);
                            Ok(calls.get())
                        })
                        .unwrap(),
                        prefix + 2
                    );
                    statement.finish(session).unwrap();
                })
                .unwrap();
        });
    }

    #[test]
    fn cursor_owned_query_sequence_positions_reject_reset_or_replaced_arena() {
        let mut budget = Budget::new(8192);
        let arena = Arena::new(&mut budget, "sequence positions", 4096).unwrap();
        let other = Arena::new(&mut budget, "other sequence positions", 4096).unwrap();
        let guc = crate::sql::guc::GucState::new();
        let session = guc.seq_session();
        let call = SequenceCall {
            operation: SequenceOperation::Last,
            source: None,
        };
        crate::mem::guard::forbid_alloc(|| {
            {
                let _statement = enter_statement(&arena, session, false, None);
                statement_call(session, call, || Ok(1)).unwrap();
            }
            {
                let _statement = enter_statement(&other, session, true, None);
                let error =
                    statement_call(session, call, || panic!("wrong arena must be rejected"))
                        .unwrap_err();
                assert_eq!(error.sqlstate, sqlstate::SERIALIZATION_FAILURE);
            }
            let mut moved = arena;
            {
                let _statement = enter_statement(&moved, session, true, None);
                assert_eq!(
                    statement_call(session, call, || panic!("a move keeps issued values")).unwrap(),
                    1
                );
            }
            moved.reset();
            let statement = enter_statement(&moved, session, true, None);
            let error =
                statement_call(session, call, || panic!("reset must be rejected")).unwrap_err();
            assert_eq!(error.sqlstate, sqlstate::SERIALIZATION_FAILURE);
            statement.discard(session);
        });
    }
}
