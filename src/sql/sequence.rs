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

use crate::mem::arena::{Arena, ArenaHandle, ArenaList};
use crate::sql::eval::{SequenceAccess, SqlError, sqlstate};
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

#[derive(Clone, Copy)]
pub(super) struct StatementSequenceReplay {
    head: Option<ArenaHandle<SequenceEffect>>,
    tail: Option<ArenaHandle<SequenceEffect>>,
    next: Option<ArenaHandle<SequenceEffect>>,
    initial_lastval: Option<(usize, u64)>,
}

impl StatementSequenceReplay {
    pub(super) const EMPTY: Self = Self {
        head: None,
        tail: None,
        next: None,
        initial_lastval: None,
    };
}

thread_local! {
    static STATEMENT_ARENA: Cell<*const Arena> = const { Cell::new(core::ptr::null()) };
}

pub(super) struct StatementSequenceScope<'arena> {
    previous: *const Arena,
    arena: PhantomData<&'arena Arena>,
}

impl StatementSequenceScope<'_> {
    pub(super) fn finish(&self, session: &SeqSession) -> Result<(), SqlError> {
        if !self.previous.is_null() {
            return Ok(());
        }
        let complete = session.statement_replay.get().next.is_none();
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

impl Drop for StatementSequenceScope<'_> {
    fn drop(&mut self) {
        STATEMENT_ARENA.with(|active| active.set(self.previous));
    }
}

pub(super) fn enter_statement<'arena>(
    arena: &'arena Arena,
    session: &SeqSession,
    resumed: bool,
) -> StatementSequenceScope<'arena> {
    let previous = STATEMENT_ARENA.with(|active| active.get());
    if !previous.is_null() {
        return StatementSequenceScope {
            previous,
            arena: PhantomData,
        };
    }
    let retained = session.statement_replay.get();
    let mut replay = if resumed && retained.head.is_some() {
        session.restore_last_sequence(retained.initial_lastval);
        retained
    } else {
        StatementSequenceReplay {
            initial_lastval: session.last_sequence(),
            ..StatementSequenceReplay::EMPTY
        }
    };
    replay.next = replay.head;
    session.statement_replay.set(replay);
    StatementSequenceScope {
        previous: STATEMENT_ARENA.with(|active| active.replace(arena)),
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

/// Bounded effect log for an expression stream that may be physically retried
/// while waiting for a mutable SQL routine. Sequence calls are replayed in
/// logical call order, never advanced again by a retry.
pub struct SequenceReplayState<'a> {
    next: Cell<usize>,
    completed: Cell<usize>,
    values: RefCell<ArenaList<'a, i64>>,
}

impl<'a> SequenceReplayState<'a> {
    pub fn new(arena: &'a Arena) -> Self {
        Self {
            next: Cell::new(0),
            completed: Cell::new(0),
            values: RefCell::new(ArenaList::new_persistent(arena)),
        }
    }

    pub fn begin_attempt(&self) {
        self.next.set(0);
    }

    fn invoke(&self, action: impl FnOnce() -> Result<i64, SqlError>) -> Result<i64, SqlError> {
        let ordinal = self.next.get();
        self.next.set(ordinal + 1);
        if ordinal < self.completed.get() {
            return Ok(self.values.borrow().as_slice()[ordinal]);
        }
        debug_assert_eq!(ordinal, self.completed.get());
        let value = action()?;
        self.values.borrow_mut().push(value).map_err(|_| {
            sql_err!(
                sqlstate::PROGRAM_LIMIT_EXCEEDED,
                "volatile sequence replay exceeds statement memory"
            )
        })?;
        self.completed.set(ordinal + 1);
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
    state: &'state SequenceReplayState<'arena>,
}

impl<'base, 'state, 'arena> ReplaySeqEval<'base, 'state, 'arena> {
    pub fn new(base: SeqEval<'base>, state: &'state SequenceReplayState<'arena>) -> Self {
        Self { base, state }
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
    fn resolve(&self, name: &str) -> Result<usize, SqlError> {
        let slot = match crate::sql::identifiers::ObjectReference::parse(name)? {
          crate::sql::identifiers::ObjectReference::Oid(oid) => {
            let slot = (0..self.storage.sequence_count()).find(|slot|
                self.storage.sequence_slot_visible_to(*slot, self.txid)
                    && crate::sql::catalog::sequence_oid(*slot) as u32 == oid);
            if slot.is_none() && i32::try_from(oid).ok().is_some_and(|oid|
                crate::sql::catalog::relation_oid_is_visible(self.storage, self.txid, oid)) {
                return Err(sql_err!(sqlstate::WRONG_OBJECT_TYPE, "object {} is not a sequence", oid));
            }
            slot
          }
          crate::sql::identifiers::ObjectReference::Name(parsed) => {
            if parsed.database.is_some_and(|database| database != self.storage.current_database_name(self.txid)) {
                return Err(sql_err!(sqlstate::FEATURE_NOT_SUPPORTED, "cross-database sequence references are not supported"));
            }
            let qualifier = parsed.schema.as_ref().map(crate::storage::SqlName::as_str);
            let base = parsed.name.as_str();
            let slot = self.storage.sequence_on_path(qualifier, base, self.txid);
            if slot.is_none() && self.storage.resolve_relation(qualifier, base, self.txid).is_some() {
                return Err(sql_err!(sqlstate::WRONG_OBJECT_TYPE, "\"{}\" is not a sequence", base));
            }
            slot
          }
        };
        if let Some(slot) = slot {
            self.storage.require_schema_usage(
                self.storage.sequence_for(slot, self.txid).schema.as_str(), self.txid)?;
            return Ok(slot);
        }
        Err(sql_err!(sqlstate::UNDEFINED_TABLE, "relation \"{}\" does not exist", name))
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
    fn nextval(&self, name: &str) -> Result<i64, SqlError> {
        let slot = self.resolve(name)?;
        self.require_any(slot, PrivilegeSet::USAGE, Some(PrivilegeSet::UPDATE))?;
        if self.dry {
            return Ok(self.storage.sequence_value_for(slot, self.txid).0);
        }
        next_cached(self.storage, self.session, slot, self.txid)
    }

    fn currval(&self, name: &str) -> Result<i64, SqlError> {
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

    fn setval(&self, name: &str, value: i64, is_called: bool) -> Result<i64, SqlError> {
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
    fn nextval(&self, name: &str) -> Result<i64, SqlError> {
        self.state.invoke(|| self.base.nextval(name))
    }
    fn currval(&self, name: &str) -> Result<i64, SqlError> {
        self.state.invoke(|| self.base.currval(name))
    }
    fn lastval(&self) -> Result<i64, SqlError> {
        self.state.invoke(|| self.base.lastval())
    }
    fn setval(&self, name: &str, value: i64, is_called: bool) -> Result<i64, SqlError> {
        self.state
            .invoke(|| self.base.setval(name, value, is_called))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mem::budget::Budget;

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
                }
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
        let replay = SequenceReplayState::new(&arena);
        let row_mark = arena.mark();
        let calls = Cell::new(0i64);
        crate::mem::guard::forbid_alloc(|| {
            for expected in 1..=1_100i64 {
                let _row_scratch = arena.alloc_slice_with(16, |_| 0xa5u8).unwrap();
                assert_eq!(
                    replay
                        .invoke(|| {
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
                        .invoke(|| panic!("a replayed call must not run again"))
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
        let arena = Arena::new(&mut budget, "small sequence replay", 64).unwrap();
        let replay = SequenceReplayState::new(&arena);
        for value in 0..4 {
            assert_eq!(replay.invoke(|| Ok(value)).unwrap(), value);
        }
        let error = replay.invoke(|| Ok(4)).unwrap_err();
        assert_eq!(error.sqlstate, sqlstate::PROGRAM_LIMIT_EXCEEDED);
        assert!(error.message.as_str().contains("statement memory"));
    }
}
