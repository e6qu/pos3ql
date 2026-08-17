//! Per-connection transaction state.
//!
//! Semantics: PostgreSQL MVCC snapshots plus bounded transaction locking. A
//! statement outside an explicit
//! block runs in an implicit transaction spanning its whole simple-query
//! message (so an error rolls the entire message back, as PostgreSQL
//! does). Writers see their own changes; everyone else sees the last
//! committed image; conflicting row locks park the connection without
//! blocking the single-threaded reactor.

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
    pub truncate_mark: usize,
    pub ddl_mark: usize,
    pub statistics_mark: usize,
    pub wal_mark: usize,
    /// Shared table/row lock acquisition clock at savepoint creation.
    pub lock_mark: u64,
    /// Pending-notification and pending-listen-op lengths (and the notification
    /// payload-buffer offset) at savepoint time, so ROLLBACK TO discards the
    /// notifications/registrations a rolled-back subtransaction produced, as
    /// PostgreSQL does.
    pub notify_mark: usize,
    pub notify_payload_mark: usize,
    pub listen_mark: usize,
    pub subscription_advance_mark: usize,
    /// The `failed` flag at savepoint time, restored on ROLLBACK TO.
    pub failed: bool,
}

/// Undo positions captured at a statement boundary. Unlike a SQL savepoint it
/// has no name and does not consume the savepoint pool.
#[derive(Clone, Copy)]
pub(crate) struct StatementMark {
    pub touched: usize,
    pub truncates: usize,
    pub ddl: usize,
    pub statistics: usize,
    pub wal: usize,
    pub lock: u64,
    pub notifications: usize,
    pub notification_payload: usize,
    pub listen_ops: usize,
    pub subscription_advances: usize,
}

pub const MAX_SAVEPOINTS: usize = 16;
pub const MAX_TRIGGER_NESTING: u16 = 32;
pub const MAX_TRUNCATE_TABLES: usize = 16;
pub const MAX_TRUNCATE_WAL_TABLE_BYTES: usize = MAX_TRUNCATE_TABLES * 128;

/// One SQL TRUNCATE command, retained until commit so durable logical
/// decoding preserves its statement-level semantics instead of inferring them
/// from the row deletes used by the heap.
#[derive(Clone, Copy)]
pub(crate) struct TruncateEvent {
    pub command_id: u32,
    pub tables: [u16; MAX_TRUNCATE_TABLES],
    pub table_count: usize,
    pub cascade: bool,
    pub restart_identity: bool,
}

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
    Serializable,
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
    /// Nested trigger-side SQL is bounded independently of row and arena
    /// capacity so a self-referential trigger cannot consume the call stack.
    trigger_depth: u16,
    /// Every row write, in order: (table slot, rowid, pending image before the
    /// write). Recorded per write (not per row) so `ROLLBACK TO SAVEPOINT` can
    /// reverse-replay to any earlier point.
    touched: FixedVec<(u32, u64, PriorPending)>,
    truncates: FixedVec<TruncateEvent>,
    truncate_wal_tables: FixedBuf,
    /// DDL performed in this transaction, for rollback.
    ddl: FixedVec<DdlUndo>,
    /// Transaction-private ANALYZE versions, kept outside `DdlUndo`. The
    /// images themselves live in Storage's startup-sized slab; undo needs only
    /// the table whose latest version is popped.
    statistics_undo: FixedVec<StatisticsUndo>,
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
    /// Publisher positions staged with the local transaction that applied
    /// them.  They are journaled immediately before the transaction marker.
    subscription_advances: FixedVec<crate::storage::SubscriptionAdvance>,
}

/// How to undo one DDL statement.
#[derive(Debug, Clone, Copy)]
#[allow(
    clippy::large_enum_variant,
    reason = "CommentSet carries its payload inline (no heap after startup); the undo list is a small fixed pool"
)]
pub(crate) enum DdlUndo {
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
    /// CREATE FUNCTION at this slot — undo by dropping its pending definition.
    RoutineCreated(u32),
    RoutineDropped(u32),
    TriggerCreated(u32),
    TriggerDropped(u32),
    TriggerAltered {
        slot: u32,
        prior: Option<crate::storage::PendingTriggerDefinition>,
    },
    RoutineIdentityAltered {
        slot: u32,
        prior: Option<crate::storage::PendingRoutineIdentity>,
    },
    /// CREATE PUBLICATION at this slot.
    PublicationCreated(u32),
    /// DROP PUBLICATION at this slot.
    PublicationDropped(u32),
    /// ALTER PUBLICATION staged a definition visible only to this transaction.
    PublicationAltered {
        slot: u32,
        prior: crate::storage::PublicationAlteration,
    },
    PublicationOwnerChanged {
        slot: u32,
        prior: Option<crate::storage::PendingOwnership>,
    },
    PublicationRenamed {
        slot: u32,
        prior: Option<crate::storage::PendingPublicationName>,
    },
    SubscriptionCreated(u32),
    SubscriptionDropped(u32),
    SubscriptionEnabled {
        slot: u32,
        prior: Option<crate::storage::PendingSubscriptionEnabled>,
    },
    SubscriptionDefinitionChanged {
        slot: u32,
        prior: Option<crate::storage::PendingSubscriptionDefinition>,
    },
    /// CREATE MATERIALIZED VIEW at this slot — undo by dropping it.
    MatviewCreated(u32),
    /// DROP MATERIALIZED VIEW at this slot — undo by reviving it.
    MatviewDropped(u32),
    /// CREATE SEQUENCE at this slot — undo by dropping it. (Its *value* state is
    /// not transactional; only existence rolls back.)
    SequenceCreated(u32),
    /// DROP SEQUENCE at this slot — undo by reviving it.
    SequenceDropped(u32),
    SequenceAltered {
        slot: u32,
        prior: Option<crate::storage::PendingSequenceDefinition>,
    },
    /// CREATE DOMAIN at this slot — undo by dropping it.
    DomainCreated(u32),
    /// DROP DOMAIN at this slot — undo by reviving it.
    DomainDropped(u32),
    DomainAltered {
        slot: u32,
        prior: Option<crate::storage::PendingDomainDefinition>,
    },
    /// CREATE TYPE (enum) at this slot — undo by dropping it.
    EnumCreated(u32),
    /// CREATE TYPE (...): named composites are distinct from anonymous records.
    CompositeCreated(u32),
    /// ALTER TYPE on a named composite staged a private physical layout.
    CompositeAltered {
        slot: u32,
        prior: Option<crate::storage::PendingCompositeDefinition>,
    },
    /// DROP TYPE (...): revive the physical type slot on rollback.
    CompositeDropped(u32),
    /// DROP TYPE (enum) at this slot — undo by reviving it.
    EnumDropped(u32),
    /// ALTER TYPE staged a definition visible only to its transaction.
    EnumAltered {
        slot: u32,
        prior: Option<crate::storage::PendingEnumDefinition>,
    },
    /// CREATE INDEX at this slot — undo by dropping it.
    IndexCreated(u32),
    /// DROP INDEX at this slot — undo by reviving it.
    IndexDropped(u32),
    IndexRenamed {
        slot: u32,
        prior: Option<crate::storage::PendingIndexName>,
    },
    /// TRUNCATE ... RESTART IDENTITY reset one column's sequence — undo by
    /// restoring the prior counter. (A plain advance is *not* undone: a
    /// rolled-back insert still consumes its number, as PostgreSQL has it.)
    SequenceReset {
        table: u32,
        column: u16,
        prior: i64,
    },
    /// TRUNCATE ... RESTART IDENTITY reset a catalog sequence. Sequence value
    /// changes ordinarily survive rollback; this explicit restart is the one
    /// transactional exception PostgreSQL defines.
    OwnedSequenceReset {
        sequence: u32,
        prior: crate::storage::SequenceValueState,
    },
    /// CREATE SCHEMA at this slot — undo by dropping it.
    SchemaCreated(u32),
    /// DROP SCHEMA at this slot — undo by reviving it.
    SchemaDropped(u32),
    /// CREATE/ALTER/DROP ROLE changed one transaction-private role overlay.
    /// Restoring the prior overlay makes repeated changes and savepoints exact.
    RoleChanged {
        slot: u32,
        prior: Option<crate::storage::PendingRole>,
    },
    RoleMembershipChanged {
        slot: u32,
        prior: Option<crate::storage::PendingRoleMembership>,
    },
    ObjectOwnerChanged {
        object: crate::storage::AccessObject,
        prior: Option<crate::storage::PendingOwnership>,
    },
    ObjectAclChanged {
        slot: u32,
        prior: Option<crate::storage::PendingAcl>,
    },
    DefaultAclChanged {
        slot: u32,
        prior: Option<crate::storage::PendingDefaultAcl>,
    },
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
pub const MAX_TXN_ANALYZE: usize = 64;
const SUBSCRIPTION_ADVANCES_PER_TXN: usize = 1;

#[derive(Debug, Clone, Copy)]
pub(crate) struct StatisticsUndo {
    pub(crate) table: u32,
}

impl TxnState {
    pub const fn budget_bytes(capacity: usize) -> usize {
        capacity * core::mem::size_of::<(u32, u64, PriorPending)>()
            + MAX_TXN_DDL * core::mem::size_of::<TruncateEvent>()
            + MAX_TRUNCATE_WAL_TABLE_BYTES
            + MAX_TXN_DDL * core::mem::size_of::<DdlUndo>()
            + MAX_TXN_ANALYZE * core::mem::size_of::<StatisticsUndo>()
            + MAX_SAVEPOINTS * core::mem::size_of::<Savepoint>()
            + crate::sql::notify::PER_TXN
                * core::mem::size_of::<crate::sql::notify::BufferedNotify>()
            + crate::sql::notify::PER_TXN_PAYLOAD_BYTES
            + crate::sql::notify::LISTEN_OPS_PER_TXN
                * core::mem::size_of::<crate::sql::notify::ListenOp>()
            + SUBSCRIPTION_ADVANCES_PER_TXN
                * core::mem::size_of::<crate::storage::SubscriptionAdvance>()
    }

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
            trigger_depth: 0,
            touched: FixedVec::new(budget, "txn_touched", capacity)?,
            truncates: FixedVec::new(budget, "txn_truncates", MAX_TXN_DDL)?,
            truncate_wal_tables: FixedBuf::new(
                budget,
                "txn_truncate_wal_tables",
                MAX_TRUNCATE_WAL_TABLE_BYTES,
            )?,
            ddl: FixedVec::new(budget, "txn_ddl", MAX_TXN_DDL)?,
            statistics_undo: FixedVec::new(budget, "txn_statistics_undo", MAX_TXN_ANALYZE)?,
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
            subscription_advances: FixedVec::new(
                budget,
                "txn_subscription_advances",
                SUBSCRIPTION_ADVANCES_PER_TXN,
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

    pub fn enter_trigger_sql(&mut self) -> Result<(), SqlError> {
        if self.trigger_depth == MAX_TRIGGER_NESTING {
            return Err(sql_err!(
                sqlstate::PROGRAM_LIMIT_EXCEEDED,
                "trigger nesting exceeds {} levels",
                MAX_TRIGGER_NESTING
            ));
        }
        self.trigger_depth += 1;
        Ok(())
    }

    pub fn leave_trigger_sql(&mut self) {
        self.trigger_depth = self
            .trigger_depth
            .checked_sub(1)
            .expect("trigger depth is paired");
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
            IsolationLevel::RepeatableRead | IsolationLevel::Serializable => {
                *self.snapshot_lsn.get_or_insert(current_lsn)
            }
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

    pub(crate) fn record_truncate(&mut self, event: TruncateEvent) -> Result<(), SqlError> {
        self.truncates.push(event).map_err(|_| {
            sql_err!(
                crate::sql::eval::sqlstate::PROGRAM_LIMIT_EXCEEDED,
                "transaction contains more than {} TRUNCATE commands",
                MAX_TXN_DDL
            )
        })
    }

    pub(crate) fn truncates(&self) -> &[TruncateEvent] {
        &self.truncates
    }

    pub(crate) fn truncate_wal_tables(&mut self) -> &mut FixedBuf {
        &mut self.truncate_wal_tables
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

    /// An apply transaction belongs to exactly one subscription. A later
    /// frontier for that subscription replaces the prior one before WAL is
    /// staged; a second identity is a protocol-boundary error.
    pub(crate) fn record_subscription_advance(
        &mut self,
        advance: crate::storage::SubscriptionAdvance,
    ) -> Result<(), SqlError> {
        if let Some(existing) = self.subscription_advances.iter_mut().next() {
            if existing.name() == advance.name() {
                *existing = advance;
                return Ok(());
            }
            return Err(sql_err!(
                sqlstate::PROGRAM_LIMIT_EXCEEDED,
                "one local transaction cannot apply multiple subscriptions"
            ));
        }
        self.subscription_advances.push(advance).map_err(|_| {
            sql_err!(
                sqlstate::PROGRAM_LIMIT_EXCEEDED,
                "one local transaction cannot apply multiple subscriptions"
            )
        })
    }

    pub(crate) fn subscription_advances(&self) -> &[crate::storage::SubscriptionAdvance] {
        &self.subscription_advances
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
    pub fn savepoint(
        &mut self,
        name: &str,
        wal_mark: usize,
        lock_mark: u64,
    ) -> Result<(), SqlError> {
        let sp = Savepoint {
            name: {
                let mut s = StackStr::new();
                let _ = core::fmt::Write::write_str(&mut s, name);
                s
            },
            touched_mark: self.touched.len(),
            truncate_mark: self.truncates.len(),
            ddl_mark: self.ddl.len(),
            statistics_mark: self.statistics_undo.len(),
            wal_mark,
            lock_mark,
            notify_mark: self.pending_notifies.len(),
            notify_payload_mark: self.notify_payloads.mark(),
            listen_mark: self.pending_listen_ops.len(),
            subscription_advance_mark: self.subscription_advances.len(),
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

    pub(crate) fn statement_mark(&self, wal: usize, lock: u64) -> StatementMark {
        StatementMark {
            touched: self.touched.len(),
            truncates: self.truncates.len(),
            ddl: self.ddl.len(),
            statistics: self.statistics_undo.len(),
            wal,
            lock,
            notifications: self.pending_notifies.len(),
            notification_payload: self.notify_payloads.mark(),
            listen_ops: self.pending_listen_ops.len(),
            subscription_advances: self.subscription_advances.len(),
        }
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

    pub(crate) fn rewind_truncates(&mut self, truncate_mark: usize) {
        while self.truncates.len() > truncate_mark {
            self.truncates.pop();
        }
    }

    pub fn rewind_ddl(&mut self, ddl_mark: usize) {
        while self.ddl.len() > ddl_mark {
            self.ddl.pop();
        }
    }

    pub(crate) fn record_statistics(&mut self, table: u32) -> Result<(), SqlError> {
        self.statistics_undo
            .push(StatisticsUndo { table })
            .map_err(|_| {
                sql_err!(
                    crate::sql::eval::sqlstate::PROGRAM_LIMIT_EXCEEDED,
                    "more than {} ANALYZE targets in one transaction",
                    MAX_TXN_ANALYZE
                )
            })
    }

    pub(crate) fn statistics_undo(&self) -> &[StatisticsUndo] {
        &self.statistics_undo
    }

    pub(crate) fn rewind_statistics(&mut self, mark: usize) {
        while self.statistics_undo.len() > mark {
            self.statistics_undo.pop();
        }
    }

    pub(crate) fn rewind_subscription_advances(&mut self, mark: usize) {
        while self.subscription_advances.len() > mark {
            self.subscription_advances.pop();
        }
    }

    pub(crate) fn record_ddl(&mut self, undo: DdlUndo) -> Result<(), SqlError> {
        self.ddl.push(undo).map_err(|_| {
            sql_err!(
                crate::sql::eval::sqlstate::PROGRAM_LIMIT_EXCEEDED,
                "more than {} DDL statements in one transaction",
                MAX_TXN_DDL
            )
        })
    }

    pub(crate) fn ddl(&self) -> &[DdlUndo] {
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
        self.truncates.clear();
        self.truncate_wal_tables.clear();
        self.ddl.clear();
        self.statistics_undo.clear();
        self.savepoints.clear();
        // Commit flushes these before clearing; rollback drops them here.
        self.pending_notifies.clear();
        self.notify_payloads.clear();
        self.pending_listen_ops.clear();
        self.subscription_advances.clear();
    }
}
