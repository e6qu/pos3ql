//! Per-connection transaction state.
//!
//! Semantics: READ COMMITTED, fail-fast. A statement outside an explicit
//! block runs in an implicit transaction spanning its whole simple-query
//! message (so an error rolls the entire message back, as PostgreSQL
//! does). Writers see their own changes; everyone else sees the last
//! committed image; a write conflict raises 40001 immediately instead of
//! blocking (single-threaded execution cannot wait).

use crate::mem::budget::{Budget, BudgetError};
use crate::mem::fixed_vec::FixedVec;
use crate::sql_err;
use crate::storage::RowLoc;
use crate::util::StackStr;

use super::eval::SqlError;

/// A row's pending image before a write, as returned by `write_pending`:
/// `None` = no pending existed; `Some(loc)` = a pending change with that loc.
pub type PriorPending = Option<Option<RowLoc>>;

/// A named savepoint: the transaction's undo marks when it was established.
#[derive(Clone)]
pub struct Savepoint {
    pub name: StackStr<63>,
    pub touched_mark: usize,
    pub ddl_mark: usize,
    pub wal_mark: usize,
    /// The `failed` flag at savepoint time, restored on ROLLBACK TO.
    pub failed: bool,
}

pub const MAX_SAVEPOINTS: usize = 16;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TxnMode {
    /// No transaction in progress.
    Idle,
    /// Started automatically for a statement/message.
    Implicit,
    /// BEGIN was issued.
    Explicit,
}

pub struct TxnState {
    pub mode: TxnMode,
    /// An error occurred inside an explicit block: everything until
    /// COMMIT/ROLLBACK fails with 25P02.
    pub failed: bool,
    pub txid: u32,
    /// Every row write, in order: (table slot, rowid, pending image before the
    /// write). Recorded per write (not per row) so `ROLLBACK TO SAVEPOINT` can
    /// reverse-replay to any earlier point.
    touched: FixedVec<(u32, u64, PriorPending)>,
    /// DDL performed in this transaction, for rollback.
    ddl: FixedVec<DdlUndo>,
    /// Active savepoints, innermost last.
    savepoints: FixedVec<Savepoint>,
    /// WAL buffer mark taken when the transaction started.
    pub wal_mark: usize,
}

/// How to undo one DDL statement.
#[derive(Debug, Clone, Copy)]
pub enum DdlUndo {
    /// CREATE TABLE at this slot — undo by dropping it.
    Created(u32),
    /// DROP TABLE at this slot (rows retained until commit) — undo by
    /// reviving it (and its indexes).
    Dropped(u32),
    /// CREATE VIEW at this slot — undo by dropping it.
    ViewCreated(u32),
    /// DROP VIEW at this slot (or the superseded view of an OR REPLACE) —
    /// undo by reviving it.
    ViewDropped(u32),
    /// CREATE INDEX at this slot — undo by dropping it.
    IndexCreated(u32),
    /// DROP INDEX at this slot — undo by reviving it.
    IndexDropped(u32),
}

pub const MAX_TXN_DDL: usize = 16;

impl TxnState {
    pub fn new(budget: &mut Budget, capacity: usize) -> Result<Self, BudgetError> {
        Ok(Self {
            mode: TxnMode::Idle,
            failed: false,
            txid: 0,
            touched: FixedVec::new(budget, "txn_touched", capacity)?,
            ddl: FixedVec::new(budget, "txn_ddl", MAX_TXN_DDL)?,
            savepoints: FixedVec::new(budget, "txn_savepoints", MAX_SAVEPOINTS)?,
            wal_mark: 0,
        })
    }

    pub fn is_active(&self) -> bool {
        self.mode != TxnMode::Idle
    }

    pub fn is_explicit(&self) -> bool {
        self.mode == TxnMode::Explicit
    }

    /// The ReadyForQuery status byte: idle / in transaction / failed.
    pub fn status_byte(&self) -> u8 {
        match (self.mode, self.failed) {
            (TxnMode::Explicit, true) => b'E',
            (TxnMode::Explicit, false) => b'T',
            _ => b'I',
        }
    }

    pub fn touch(
        &mut self,
        table_slot: u32,
        rowid: u64,
        prior: PriorPending,
    ) -> Result<(), SqlError> {
        self.touched.push((table_slot, rowid, prior)).map_err(|_| {
            sql_err!(
                "54000",
                "transaction touches more than {} rows (txn_rows)",
                self.touched.capacity()
            )
        })
    }

    pub fn touched(&self) -> &[(u32, u64, PriorPending)] {
        &self.touched
    }

    /// Establishes a savepoint at the current undo position. A duplicate name
    /// is allowed (PostgreSQL shadows the older one).
    pub fn savepoint(&mut self, name: &str, wal_mark: usize) -> Result<(), SqlError> {
        let sp = Savepoint {
            name: {
                let mut s = StackStr::new();
                let _ = core::fmt::Write::write_str(&mut s, name);
                s
            },
            touched_mark: self.touched.len(),
            ddl_mark: self.ddl.len(),
            wal_mark,
            failed: self.failed,
        };
        self.savepoints.push(sp).map_err(|_| {
            sql_err!("54000", "more than {} active savepoints", MAX_SAVEPOINTS)
        })
    }

    /// Index of the most recent savepoint with this name.
    pub fn savepoint_index(&self, name: &str) -> Option<usize> {
        self.savepoints
            .as_slice()
            .iter()
            .rposition(|s| s.name.as_str() == name)
    }

    pub fn savepoint_at(&self, index: usize) -> Savepoint {
        self.savepoints.as_slice()[index].clone()
    }

    /// Drops the savepoint at `index` and every one nested inside it (for
    /// `RELEASE SAVEPOINT`; the changes themselves are kept).
    pub fn release_savepoints_from(&mut self, index: usize) {
        while self.savepoints.len() > index {
            self.savepoints.pop();
        }
    }

    /// Drops savepoints nested strictly inside `index`, keeping `index` itself (for
    /// `ROLLBACK TO SAVEPOINT`, which leaves the target reusable).
    pub fn rollback_savepoints_after(&mut self, index: usize) {
        while self.savepoints.len() > index + 1 {
            self.savepoints.pop();
        }
    }

    /// Truncates the undo logs back to the given marks, returning the removed
    /// touched entries (newest first) so the caller can reverse them.
    pub fn rewind_touched(&mut self, touched_mark: usize) {
        while self.touched.len() > touched_mark {
            self.touched.pop();
        }
    }

    pub fn rewind_ddl(&mut self, ddl_mark: usize) {
        while self.ddl.len() > ddl_mark {
            self.ddl.pop();
        }
    }

    pub fn record_ddl(&mut self, undo: DdlUndo) -> Result<(), SqlError> {
        self.ddl.push(undo).map_err(|_| {
            sql_err!(
                "54000",
                "more than {} DDL statements in one transaction",
                MAX_TXN_DDL
            )
        })
    }

    pub fn ddl(&self) -> &[DdlUndo] {
        &self.ddl
    }

    pub fn clear(&mut self) {
        self.mode = TxnMode::Idle;
        self.failed = false;
        self.touched.clear();
        self.ddl.clear();
        self.savepoints.clear();
    }
}
