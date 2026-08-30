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

use crate::sql::eval::{SequenceAccess, SqlError, sqlstate};
use crate::sql::guc::SeqSession;
use crate::sql_err;
use crate::storage::{AccessClass, AccessObject, PrivilegeSet, Storage};
use core::cell::Cell;

pub const MAX_REPLAYED_SEQUENCE_CALLS: usize = 1024;

/// Bounded effect log for an expression stream that may be physically retried
/// while waiting for a mutable SQL routine. Sequence calls are replayed in
/// logical call order, never advanced again by a retry.
pub struct SequenceReplayState {
    next: Cell<usize>,
    completed: Cell<usize>,
    values: [Cell<i64>; MAX_REPLAYED_SEQUENCE_CALLS],
}

impl SequenceReplayState {
    pub fn new() -> Self {
        Self {
            next: Cell::new(0),
            completed: Cell::new(0),
            values: [const { Cell::new(0) }; MAX_REPLAYED_SEQUENCE_CALLS],
        }
    }

    pub fn begin_attempt(&self) {
        self.next.set(0);
    }

    fn cursor(&self) -> usize {
        self.next.get()
    }

    fn restore_cursor(&self, cursor: usize) {
        self.next.set(cursor);
    }

    fn invoke(&self, action: impl FnOnce() -> Result<i64, SqlError>) -> Result<i64, SqlError> {
        let ordinal = self.next.get();
        if ordinal == MAX_REPLAYED_SEQUENCE_CALLS {
            return Err(sql_err!(
                sqlstate::PROGRAM_LIMIT_EXCEEDED,
                "SQL statement exceeds {} volatile sequence calls",
                MAX_REPLAYED_SEQUENCE_CALLS
            ));
        }
        self.next.set(ordinal + 1);
        if ordinal < self.completed.get() {
            return Ok(self.values[ordinal].get());
        }
        let value = action()?;
        self.values[ordinal].set(value);
        self.completed.set(ordinal + 1);
        Ok(value)
    }
}

impl Default for SequenceReplayState {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Clone, Copy)]
pub struct SeqEval<'a> {
    storage: &'a Storage,
    session: &'a SeqSession,
    txid: u32,
    /// A "dry" evaluator resolves names (so errors match) but never advances a
    /// generator or touches session state. The INSERT ... SELECT counting pass
    /// uses it so a `nextval` in the projection fires exactly once — in the
    /// second, materializing pass — not twice.
    dry: bool,
}

pub struct ReplaySeqEval<'base, 'state> {
    base: SeqEval<'base>,
    state: &'state SequenceReplayState,
}

impl<'base, 'state> ReplaySeqEval<'base, 'state> {
    pub fn new(base: SeqEval<'base>, state: &'state SequenceReplayState) -> Self {
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
        let (qualifier, base) = match name.rsplit_once('.') {
            Some((q, b)) => (Some(q), b),
            None => (None, name),
        };
        if let Some(slot) = self.storage.sequence_on_path(qualifier, base, self.txid) {
            self.storage.require_schema_usage(
                self.storage.sequence_for(slot, self.txid).schema.as_str(),
                self.txid,
            )?;
            return Ok(slot);
        }
        // Match PostgreSQL's phrasing: a relation of another kind is a type
        // error; nothing at all is an undefined relation.
        if self
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
    let sequence = storage.sequence_for(slot, txid);
    if let Some(value) = session.take_cached(slot, sequence.cache_identity()) {
        session.record_nextval(slot, sequence.created_at, value);
        return Ok(value);
    }
    let (value, reserved) = storage.reserve_sequence_values(slot, txid, sequence.cache)?;
    session.store_cache(slot, value, reserved, &sequence);
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
        let seq = self.storage.sequence_for(slot, self.txid);
        match self.session.currval(slot, seq.created_at) {
            Some(v) => Ok(v),
            None => Err(sql_err!(
                sqlstate::OBJECT_NOT_IN_PREREQUISITE_STATE,
                "currval of sequence \"{}\" is not yet defined in this session",
                seq.name.as_str()
            )),
        }
    }

    fn lastval(&self) -> Result<i64, SqlError> {
        self.session.lastval().ok_or_else(|| {
            sql_err!(
                sqlstate::OBJECT_NOT_IN_PREREQUISITE_STATE,
                "lastval is not yet defined in this session"
            )
        })
    }

    fn setval(&self, name: &str, value: i64, is_called: bool) -> Result<i64, SqlError> {
        let slot = self.resolve(name)?;
        self.require_any(slot, PrivilegeSet::UPDATE, None)?;
        let seq = self.storage.sequence_for(slot, self.txid);
        if self.dry {
            // Validate the range (so the error surfaces in the counting pass too)
            // without moving the generator.
            self.storage.check_sequence_value(slot, self.txid, value)?;
            return Ok(value);
        }
        let result = self
            .storage
            .set_sequence_value(slot, self.txid, value, is_called)?;
        self.session.record_setval(slot, seq.created_at, value);
        Ok(result)
    }

    fn dry_nextval(&self, name: &str) -> Result<i64, SqlError> {
        Self { dry: true, ..*self }.nextval(name)
    }

    fn dry_currval(&self, name: &str) -> Result<i64, SqlError> {
        Self { dry: true, ..*self }.currval(name)
    }

    fn dry_lastval(&self) -> Result<i64, SqlError> {
        Self { dry: true, ..*self }.lastval()
    }

    fn dry_setval(&self, name: &str, value: i64, is_called: bool) -> Result<i64, SqlError> {
        Self { dry: true, ..*self }.setval(name, value, is_called)
    }
}

impl SequenceAccess for ReplaySeqEval<'_, '_> {
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
    fn dry_nextval(&self, name: &str) -> Result<i64, SqlError> {
        self.base.dry_nextval(name)
    }
    fn dry_currval(&self, name: &str) -> Result<i64, SqlError> {
        self.base.dry_currval(name)
    }
    fn dry_lastval(&self) -> Result<i64, SqlError> {
        self.base.dry_lastval()
    }
    fn dry_setval(&self, name: &str, value: i64, is_called: bool) -> Result<i64, SqlError> {
        self.base.dry_setval(name, value, is_called)
    }
    fn statement_cursor(&self) -> Option<usize> {
        Some(self.state.cursor())
    }
    fn restore_statement_cursor(&self, cursor: usize) {
        self.state.restore_cursor(cursor);
    }
}
