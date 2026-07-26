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

use crate::sql::eval::{sqlstate, SequenceAccess, SqlError};
use crate::sql::guc::SeqSession;
use crate::sql_err;
use crate::storage::Storage;

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

impl<'a> SeqEval<'a> {
    pub fn new(storage: &'a Storage, session: &'a SeqSession, txid: u32) -> Self {
        SeqEval { storage, session, txid, dry: false }
    }

    pub fn dry(storage: &'a Storage, session: &'a SeqSession, txid: u32) -> Self {
        SeqEval { storage, session, txid, dry: true }
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
            return Ok(slot);
        }
        // Match PostgreSQL's phrasing: a relation of another kind is a type
        // error; nothing at all is an undefined relation.
        if self.storage.resolve_relation(qualifier, base, self.txid).is_some() {
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
}

impl SequenceAccess for SeqEval<'_> {
    fn nextval(&self, name: &str) -> Result<i64, SqlError> {
        let slot = self.resolve(name)?;
        let seq = self.storage.sequence(slot);
        if self.dry {
            return Ok(seq.last_value.get());
        }
        let value = seq.next_value()?;
        self.session.record_nextval(slot, seq.created_at, value);
        Ok(value)
    }

    fn currval(&self, name: &str) -> Result<i64, SqlError> {
        let slot = self.resolve(name)?;
        let seq = self.storage.sequence(slot);
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
        let seq = self.storage.sequence(slot);
        if self.dry {
            // Validate the range (so the error surfaces in the counting pass too)
            // without moving the generator.
            seq.check_setval(value)?;
            return Ok(value);
        }
        let result = seq.set_value(value, is_called)?;
        self.session.record_setval(slot, seq.created_at, value);
        Ok(result)
    }
}
