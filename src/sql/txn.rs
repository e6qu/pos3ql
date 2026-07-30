//! Per-connection transaction state.
//!
//! Semantics: READ COMMITTED, fail-fast. A statement outside an explicit
//! block runs in an implicit transaction spanning its whole simple-query
//! message (so an error rolls the entire message back, as PostgreSQL
//! does). Writers see their own changes; everyone else sees the last
//! committed image; a write conflict raises 40001 immediately instead of
//! blocking (single-threaded execution cannot wait).

use crate::mem::budget::{Budget, BudgetError};
use crate::mem::buffer::FixedBuf;
use crate::mem::fixed_vec::FixedVec;
use crate::sql::eval::sqlstate;
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
    /// Pending-notification and pending-listen-op lengths (and the notification
    /// payload-buffer offset) at savepoint time, so ROLLBACK TO discards the
    /// notifications/registrations a rolled-back subtransaction produced, as
    /// PostgreSQL does.
    pub notify_mark: usize,
    pub notify_payload_mark: usize,
    pub listen_mark: usize,
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IsolationLevel {
    ReadCommitted,
    RepeatableRead,
}

pub struct TxnState {
    pub mode: TxnMode,
    /// An error occurred inside an explicit block: everything until
    /// COMMIT/ROLLBACK fails with 25P02.
    pub failed: bool,
    pub txid: u32,
    /// The isolation level promised to this transaction. READ COMMITTED takes
    /// a fresh durable-LSN snapshot per statement; REPEATABLE READ pins the
    /// first data statement's snapshot until commit or rollback.
    pub isolation: IsolationLevel,
    pub read_only: bool,
    pub deferrable: bool,
    snapshot_lsn: Option<u64>,
    snapshot_taken: bool,
    /// PostgreSQL's command-id: a monotonically increasing counter bumped once
    /// per statement. A row write records the command that made it, so a query
    /// snapshotting at command K sees writes from commands `< K` but not its own
    /// (K) — the mechanism that keeps a data-modifying CTE's changes invisible
    /// to the same statement's main query. Starts at 1 so any stored `cid: 0`
    /// (a restored pre-savepoint change) is always visible.
    command_id: u32,
    /// Every row write, in order: (table slot, rowid, pending image before the
    /// write). Recorded per write (not per row) so `ROLLBACK TO SAVEPOINT` can
    /// reverse-replay to any earlier point.
    touched: FixedVec<(u32, u64, PriorPending)>,
    /// DDL performed in this transaction, for rollback.
    ddl: FixedVec<DdlUndo>,
    /// Active savepoints, innermost last.
    savepoints: FixedVec<Savepoint>,
    /// NOTIFY raised in this transaction, delivered at commit (discarded on
    /// rollback), each stamped with the raising connection's PID. Payload bytes
    /// live in `notify_payloads`, referenced by offset/length so the entry pool
    /// stays compact.
    pending_notifies: FixedVec<crate::sql::notify::BufferedNotify>,
    /// Backing bytes for `pending_notifies` payloads.
    notify_payloads: FixedBuf,
    /// LISTEN/UNLISTEN performed in this transaction, applied to the shared
    /// registry at commit (discarded on rollback).
    pending_listen_ops: FixedVec<crate::sql::notify::ListenOp>,
}

/// How to undo one DDL statement.
#[derive(Debug, Clone, Copy)]
#[allow(
    clippy::large_enum_variant,
    reason = "CommentSet carries its payload inline (no heap after startup); the undo list is a small fixed pool"
)]
pub enum DdlUndo {
    /// CREATE TABLE at this slot — undo by dropping it.
    Created(u32),
    /// DROP TABLE at this slot (rows retained until commit) — undo by
    /// reviving it (and its indexes).
    Dropped(u32),
    /// ALTER TABLE appended one pending table-definition version. Row-image
    /// undo is carried by the ordinary touched-row log.
    TableAltered(u32),
    /// CREATE VIEW at this slot — undo by dropping it.
    ViewCreated(u32),
    /// DROP VIEW at this slot (or the superseded view of an OR REPLACE) —
    /// undo by reviving it.
    ViewDropped(u32),
    /// CREATE MATERIALIZED VIEW at this slot — undo by dropping it.
    MatviewCreated(u32),
    /// DROP MATERIALIZED VIEW at this slot — undo by reviving it.
    MatviewDropped(u32),
    /// CREATE SEQUENCE at this slot — undo by dropping it. (Its *value* state is
    /// not transactional; only existence rolls back.)
    SequenceCreated(u32),
    /// DROP SEQUENCE at this slot — undo by reviving it.
    SequenceDropped(u32),
    /// CREATE DOMAIN at this slot — undo by dropping it.
    DomainCreated(u32),
    /// DROP DOMAIN at this slot — undo by reviving it.
    DomainDropped(u32),
    /// ALTER DOMAIN changed nullability.
    DomainNullabilityAltered { slot: u32, prior: bool },
    /// ALTER DOMAIN changed its default expression.
    DomainDefaultAltered {
        slot: u32,
        prior: Option<StackStr<{ crate::storage::DEFAULT_EXPR_MAX }>>,
    },
    /// ALTER DOMAIN appended a CHECK constraint.
    DomainCheckAdded { slot: u32, prior_count: u8 },
    /// ALTER DOMAIN removed a CHECK constraint.
    DomainCheckDropped {
        slot: u32,
        index: u8,
        prior: crate::storage::CheckConstraint,
    },
    /// CREATE TYPE (enum) at this slot — undo by dropping it.
    EnumCreated(u32),
    /// DROP TYPE (enum) at this slot — undo by reviving it.
    EnumDropped(u32),
    /// ALTER TYPE appended an enum member.
    EnumValueAdded { slot: u32, prior_count: u8 },
    /// ALTER TYPE renamed one enum label.
    EnumValueRenamed {
        slot: u32,
        index: u8,
        prior: crate::storage::SqlName,
    },
    /// ALTER TYPE renamed the type itself.
    EnumRenamed {
        slot: u32,
        prior: crate::storage::SqlName,
    },
    /// CREATE INDEX at this slot — undo by dropping it.
    IndexCreated(u32),
    /// DROP INDEX at this slot — undo by reviving it.
    IndexDropped(u32),
    /// TRUNCATE ... RESTART IDENTITY reset one column's sequence — undo by
    /// restoring the prior counter. (A plain advance is *not* undone: a
    /// rolled-back insert still consumes its number, as PostgreSQL has it.)
    SequenceReset { table: u32, column: u16, prior: i64 },
    /// TRUNCATE ... RESTART IDENTITY reset a catalog sequence. Sequence value
    /// changes ordinarily survive rollback; this explicit restart is the one
    /// transactional exception PostgreSQL defines.
    OwnedSequenceReset {
        sequence: u32,
        prior: i64,
        prior_called: bool,
    },
    /// CREATE SCHEMA at this slot — undo by dropping it.
    SchemaCreated(u32),
    /// DROP SCHEMA at this slot — undo by reviving it.
    SchemaDropped(u32),
    /// A `COMMENT ON` set or removed an object's comment — undo by restoring
    /// the slot's prior uncommitted overlay. On commit, the overlay is
    /// promoted and journaled.
    CommentSet {
        slot: u32,
        prior: Option<crate::storage::PendingComment>,
    },
}

/// Sized for a DROP SCHEMA CASCADE closure: every contained table, view and
/// transaction-versioned inbound foreign key takes one undo entry.
pub const MAX_TXN_DDL: usize = 64;

impl TxnState {
    pub fn new(budget: &mut Budget, capacity: usize) -> Result<Self, BudgetError> {
        Ok(Self {
            mode: TxnMode::Idle,
            failed: false,
            txid: 0,
            isolation: IsolationLevel::ReadCommitted,
            read_only: false,
            deferrable: false,
            snapshot_lsn: None,
            snapshot_taken: false,
            command_id: 1,
            touched: FixedVec::new(budget, "txn_touched", capacity)?,
            ddl: FixedVec::new(budget, "txn_ddl", MAX_TXN_DDL)?,
            savepoints: FixedVec::new(budget, "txn_savepoints", MAX_SAVEPOINTS)?,
            pending_notifies: FixedVec::new(
                budget,
                "txn_pending_notifies",
                crate::sql::notify::PER_TXN,
            )?,
            notify_payloads: FixedBuf::new(
                budget,
                "txn_notify_payloads",
                crate::sql::notify::PER_TXN_PAYLOAD_BYTES,
            )?,
            pending_listen_ops: FixedVec::new(
                budget,
                "txn_pending_listen_ops",
                crate::sql::notify::LISTEN_OPS_PER_TXN,
            )?,
        })
    }

    pub fn is_active(&self) -> bool {
        self.mode != TxnMode::Idle
    }

    pub fn is_explicit(&self) -> bool {
        self.mode == TxnMode::Explicit
    }

    /// The ReadyForQuery status byte: idle / in transaction / failed.
    /// The current command-id, stamped on this statement's row writes and used
    /// as the read snapshot within the statement.
    pub fn command_id(&self) -> u32 {
        self.command_id
    }

    /// Advances to the next command. Called once at the start of each statement,
    /// so all of a statement's sub-parts (a WITH clause's data-modifying CTEs
    /// and its main query) share one command-id and therefore one snapshot.
    pub fn begin_command(&mut self) {
        self.command_id = self.command_id.saturating_add(1);
    }

    pub fn set_characteristics(
        &mut self,
        isolation: IsolationLevel,
        read_only: bool,
        deferrable: bool,
    ) {
        self.isolation = isolation;
        self.read_only = read_only;
        self.deferrable = deferrable;
    }

    /// Selects this statement's durable commit snapshot. A repeatable-read
    /// transaction pins its first data statement; READ COMMITTED follows the
    /// latest committed LSN on every statement.
    pub fn statement_snapshot(&mut self, current_lsn: u64) -> u64 {
        self.snapshot_taken = true;
        match self.isolation {
            IsolationLevel::ReadCommitted => current_lsn,
            IsolationLevel::RepeatableRead => *self.snapshot_lsn.get_or_insert(current_lsn),
        }
    }

    pub fn snapshot_lsn(&self) -> Option<u64> {
        self.snapshot_lsn
    }

    pub fn snapshot_taken(&self) -> bool {
        self.snapshot_taken
    }

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
                crate::sql::eval::sqlstate::PROGRAM_LIMIT_EXCEEDED,
                "transaction touches more than {} rows (txn_rows)",
                self.touched.capacity()
            )
        })
    }

    pub fn touched(&self) -> &[(u32, u64, PriorPending)] {
        &self.touched
    }

    /// Buffers a NOTIFY, collapsing an identical (channel, payload) already
    /// buffered in this transaction (PostgreSQL deduplicates within a
    /// transaction). The payload is copied into the companion byte buffer.
    pub fn buffer_notify(
        &mut self,
        pid: i32,
        channel: crate::sql::notify::Channel,
        payload: &str,
    ) -> Result<(), SqlError> {
        let bytes = self.notify_payloads.readable();
        let duplicate = self.pending_notifies.as_slice().iter().any(|existing| {
            existing.pid == pid
                && existing.channel.as_str() == channel.as_str()
                && &bytes[existing.payload_offset..existing.payload_offset + existing.payload_len]
                    == payload.as_bytes()
        });
        if duplicate {
            return Ok(());
        }
        let payload_offset = self.notify_payloads.mark();
        if !self.notify_payloads.append(payload.as_bytes()) {
            return Err(sql_err!(
                sqlstate::PROGRAM_LIMIT_EXCEEDED,
                "transaction's notification payloads exceed {} bytes (txn_notify_payloads)",
                self.notify_payloads.capacity()
            ));
        }
        self.pending_notifies
            .push(crate::sql::notify::BufferedNotify {
                pid,
                channel,
                payload_offset,
                payload_len: payload.len(),
            })
            .map_err(|_| {
                sql_err!(
                    sqlstate::PROGRAM_LIMIT_EXCEEDED,
                    "transaction raises more than {} notifications (txn_pending_notifies)",
                    self.pending_notifies.capacity()
                )
            })
    }

    /// Buffers a LISTEN/UNLISTEN to apply at commit.
    pub fn buffer_listen_op(&mut self, op: crate::sql::notify::ListenOp) -> Result<(), SqlError> {
        self.pending_listen_ops.push(op).map_err(|_| {
            sql_err!(
                sqlstate::PROGRAM_LIMIT_EXCEEDED,
                "transaction performs more than {} LISTEN/UNLISTEN operations (txn_pending_listen_ops)",
                self.pending_listen_ops.capacity()
            )
        })
    }

    pub fn pending_notify_count(&self) -> usize {
        self.pending_notifies.len()
    }

    /// Reconstructs the `i`th buffered notification (payload from the companion
    /// buffer) for delivery at commit.
    pub fn pending_notification(&self, i: usize) -> crate::sql::notify::Notification {
        let entry = self.pending_notifies.as_slice()[i];
        let bytes = &self.notify_payloads.readable()
            [entry.payload_offset..entry.payload_offset + entry.payload_len];
        crate::sql::notify::Notification::from_bytes(entry.pid, entry.channel, bytes)
    }

    pub fn pending_listen_ops(&self) -> &[crate::sql::notify::ListenOp] {
        &self.pending_listen_ops
    }

    /// Discards this transaction's buffered notifications and listen ops (at
    /// commit after they are applied, and at rollback).
    pub fn clear_pending_notifications(&mut self) {
        self.pending_notifies.clear();
        self.notify_payloads.clear();
        self.pending_listen_ops.clear();
    }

    /// Rewinds the notification buffers to a savepoint's marks (ROLLBACK TO).
    pub fn rewind_notifications(
        &mut self,
        notify_mark: usize,
        payload_mark: usize,
        listen_mark: usize,
    ) {
        while self.pending_notifies.len() > notify_mark {
            self.pending_notifies.pop();
        }
        self.notify_payloads.truncate_to(payload_mark);
        while self.pending_listen_ops.len() > listen_mark {
            self.pending_listen_ops.pop();
        }
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
            notify_mark: self.pending_notifies.len(),
            notify_payload_mark: self.notify_payloads.mark(),
            listen_mark: self.pending_listen_ops.len(),
            failed: self.failed,
        };
        self.savepoints.push(sp).map_err(|_| {
            sql_err!(
                sqlstate::PROGRAM_LIMIT_EXCEEDED,
                "more than {} active savepoints",
                MAX_SAVEPOINTS
            )
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
                crate::sql::eval::sqlstate::PROGRAM_LIMIT_EXCEEDED,
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
        self.isolation = IsolationLevel::ReadCommitted;
        self.read_only = false;
        self.deferrable = false;
        self.snapshot_lsn = None;
        self.snapshot_taken = false;
        self.touched.clear();
        self.ddl.clear();
        self.savepoints.clear();
        // Commit flushes these before clearing; rollback drops them here.
        self.pending_notifies.clear();
        self.notify_payloads.clear();
        self.pending_listen_ops.clear();
    }
}
