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
use crate::storage::{ColumnSet, Storage};
use crate::util::StackStr;

use super::ast::TransactionIsolation;
use super::eval::SqlError;

/// The immutable pending append returned by `write_pending`.
pub type PriorPending = crate::storage::PendingWriteUndo;

/// A named savepoint: the transaction's undo marks when it was established.
#[derive(Clone)]
pub struct Savepoint {
    pub name: StackStr<63>,
    pub read_only: bool,
    pub read_only_source: bool,
    pub touched_mark: usize,
    pub truncate_mark: usize,
    pub ddl_mark: usize,
    pub statistics_mark: usize,
    pub wal_mark: usize,
    /// Shared table/row lock acquisition clock at savepoint creation.
    pub lock_mark: u64,
    pub(crate) cursor_mark: Option<super::cursor::CursorCreationMark>,
    /// Pending-notification and pending-listen-op lengths (and the notification
    /// payload-buffer offset) at savepoint time, so ROLLBACK TO discards the
    /// notifications/registrations a rolled-back subtransaction produced, as
    /// PostgreSQL does.
    pub notify_mark: usize,
    pub notify_payload_mark: usize,
    pub listen_mark: usize,
    pub subscription_advance_mark: usize,
    pub constraint_obligation_mark: usize,
    pub constraint_completion_mark: usize,
    pub constraint_mode_mark: usize,
    pub constraint_rename_mark: usize,
    pub deferred_trigger_mark: usize,
    pub deferred_trigger_completion_mark: usize,
    pub deferred_trigger_bytes_mark: usize,
    /// The `failed` flag at savepoint time, restored on ROLLBACK TO.
    pub failed: bool,
}

/// Undo positions captured at a statement boundary. Unlike a SQL savepoint it
/// has no name and does not consume the savepoint pool.
#[derive(Clone, Copy)]
pub(crate) struct StatementMark {
    txid: u32,
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
    pub constraint_obligations: usize,
    pub constraint_completions: usize,
    pub constraint_modes: usize,
    pub constraint_renames: usize,
    pub deferred_triggers: usize,
    pub deferred_trigger_completions: usize,
    pub deferred_trigger_bytes: usize,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) struct TransactionEffectMark([usize; 15]);

#[derive(Clone, Copy)]
pub(super) struct WriteCompletionBoundary {
    pub(super) mark: StatementMark,
    pub(super) command: u32,
    pub(super) depth: u16,
}

pub(crate) fn establish_savepoint(
    storage: &Storage,
    txn: &mut TxnState,
    guc: &super::guc::GucState,
    name: &str,
    wal_mark: usize,
    cursor_mark: Option<super::cursor::CursorCreationMark>,
) -> Result<usize, SqlError> {
    txn.savepoint(name, wal_mark, storage.lock_mark(), cursor_mark)?;
    let index = txn.savepoint_index(name).expect("new savepoint is present");
    if let Err(error) = super::foreign::savepoint(storage, txn.txid, name) {
        txn.release_savepoints_from(index);
        return Err(error);
    }
    storage.set_cumulative_transaction_nest_level(txn.txid, index + 2);
    guc.savepoint();
    txn.publish_foreign_statement_context(storage)?;
    Ok(index)
}

pub(crate) fn release_savepoint(
    storage: &Storage,
    txn: &mut TxnState,
    guc: &super::guc::GucState,
    index: usize,
) -> Result<(), SqlError> {
    let savepoint = txn.savepoint_at(index);
    super::foreign::release_savepoint(storage, txn.txid, savepoint.name.as_str())?;
    storage.finish_cumulative_subtransactions(txn.txid, index + 2, true);
    storage.set_cumulative_transaction_nest_level(txn.txid, index + 1);
    txn.release_savepoints_from(index);
    guc.release_savepoints_from(index);
    txn.publish_foreign_statement_context(storage)?;
    Ok(())
}

pub const DEFAULT_MAX_SAVEPOINTS: usize = 16;
pub const DEFAULT_MAX_LARGE_OBJECT_DESCRIPTORS: usize = 64;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct LargeObjectDescriptorMode {
    pub readable: bool,
    pub writable: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct LargeObjectDescriptor {
    pub oid: crate::storage::LargeObjectOid,
    pub position: i64,
    pub mode: LargeObjectDescriptorMode,
    opened_savepoint_depth: usize,
    active: bool,
}

impl LargeObjectDescriptor {
    const EMPTY: Self = Self {
        oid: crate::storage::LargeObjectOid::parse(1).expect("one is a valid OID"),
        position: 0,
        mode: LargeObjectDescriptorMode {
            readable: false,
            writable: false,
        },
        opened_savepoint_depth: 0,
        active: false,
    };
}
// Trigger routines recurse through the bounded statement executor. Keep the
// SQL limit below the smallest supported thread stack's native-frame limit.
pub const MAX_TRIGGER_NESTING: u16 = 16;
pub const MAX_RULE_NESTING: usize = 16;
const DEFAULT_MAX_TRUNCATE_TABLES: usize = 16;

/// One SQL TRUNCATE command, retained until commit so durable logical
/// decoding preserves its statement-level semantics instead of inferring them
/// from the row deletes used by the heap.
#[derive(Clone)]
pub(crate) struct TruncateEvent {
    scope: crate::storage::RuntimeInputScope,
    pub command_id: u32,
    targets: crate::mem::lease_pool::SharedSlice<crate::storage::TruncateTarget>,
    pub cascade: bool,
    pub restart_identity: bool,
}

impl TruncateEvent {
    fn plan_event(&self, created_at: u64) -> super::plan_invalidations::PlanInvalidationEvent {
        super::plan_invalidations::PlanInvalidationEvent {
            scope: self.scope,
            change: super::plan_invalidations::CatalogPlanChange {
                global: false,
                roles: false,
            },
            selected: Some(crate::storage::SelectedPlanTarget::Relation { created_at }),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TxnMode {
    /// No transaction in progress.
    Idle,
    /// Started automatically for a statement/message.
    Implicit,
    /// An extended Execute completed; later commands share the block until Sync.
    ImplicitBlock,
    /// BEGIN was issued.
    Explicit,
}

#[derive(Clone, Copy, Default)]
struct TransactionSources {
    isolation: bool,
    read_only: bool,
    deferrable: bool,
}

#[derive(Clone, Copy)]
pub(crate) struct TransactionConfiguration {
    isolation: TransactionIsolation,
    read_only: bool,
    deferrable: bool,
    sources: TransactionSources,
}

#[must_use]
pub(crate) struct TriggerSqlScope<'txn> {
    transaction: &'txn mut TxnState,
    parent_command: u32,
    parent_depth: u16,
    _snapshot: crate::storage::ReadSnapshotScope,
}

/// One unfinished nested command, independent of its worker and stack frame.
pub(crate) struct SuspendedTriggerSql {
    transaction: u32,
    identity: super::ExecutionIdentity,
    parent_command: u32,
    parent_depth: u16,
    command: u32,
    depth: u16,
    snapshot: crate::storage::SavedReadSnapshot,
}

impl TriggerSqlScope<'_> {
    pub(crate) fn suspend(self) -> SuspendedTriggerSql {
        SuspendedTriggerSql {
            transaction: self.transaction.txid,
            identity: super::execution_identity(),
            parent_command: self.parent_command,
            parent_depth: self.parent_depth,
            command: self.transaction.command_id,
            depth: self.transaction.trigger_depth,
            snapshot: self._snapshot.capture_current(),
        }
    }
}

impl SuspendedTriggerSql {
    #[expect(
        clippy::result_large_err,
        reason = "failed attachment returns the original command owner without allocating"
    )]
    pub(crate) fn resume(
        self,
        transaction: &mut TxnState,
    ) -> Result<TriggerSqlScope<'_>, (Self, SqlError)> {
        if transaction.failed
            || transaction.mode == TxnMode::Idle
            || transaction.txid != self.transaction
            || super::execution_identity() != self.identity
            || transaction.command_id != self.parent_command
            || transaction.trigger_depth != self.parent_depth
            || transaction.command_counter < self.command
        {
            return Err((
                self,
                sql_err!(
                    sqlstate::SERIALIZATION_FAILURE,
                    "suspended nested SQL belongs to a different command"
                ),
            ));
        }
        transaction.command_id = self.command;
        transaction.trigger_depth = self.depth;
        Ok(TriggerSqlScope {
            transaction,
            parent_command: self.parent_command,
            parent_depth: self.parent_depth,
            _snapshot: self.snapshot.resume(),
        })
    }
}

impl core::ops::Deref for TriggerSqlScope<'_> {
    type Target = TxnState;
    fn deref(&self) -> &TxnState {
        self.transaction
    }
}

impl core::ops::DerefMut for TriggerSqlScope<'_> {
    fn deref_mut(&mut self) -> &mut TxnState {
        self.transaction
    }
}

impl Drop for TriggerSqlScope<'_> {
    fn drop(&mut self) {
        self.transaction.command_id = self.parent_command;
        self.transaction.trigger_depth = self.parent_depth;
    }
}

// Utility commands retain their invocation context even without a row snapshot.
pub(super) struct SuspendedStatement {
    pub(super) lease: Option<crate::storage::StatementSnapshotLease>,
    command: u32,
    clock: super::datetime::StatementClock,
    deadline: i64,
    context: super::exec::PlpgsqlTransactionContext,
}

pub(crate) fn validate_isolation_mode(
    mode: crate::config::TransactionMode,
    isolation: super::ast::TransactionIsolation,
) -> Result<(), super::eval::SqlError> {
    if mode == crate::config::TransactionMode::Locking
        && matches!(
            isolation,
            super::ast::TransactionIsolation::RepeatableRead
                | super::ast::TransactionIsolation::Serializable
        )
    {
        return Err(crate::sql_err!(
            super::eval::sqlstate::FEATURE_NOT_SUPPORTED,
            "transaction_mode = locking supports READ COMMITTED and READ UNCOMMITTED only"
        ));
    }
    Ok(())
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum CommitTarget {
    Ordinary,
    Utility(super::UtilityCommand),
    ConcurrentIndex {
        command: u64,
        phase: super::concurrent_index::CommitPhase,
    },
    Prepare {
        slot: usize,
        gid: super::ast::PreparedTransactionId,
    },
    CommitPrepared {
        slot: usize,
        gid: super::ast::PreparedTransactionId,
    },
    RollbackPrepared {
        slot: usize,
        gid: super::ast::PreparedTransactionId,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum CommitBarrier {
    NoJournal,
    Journal,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct StagedCommit {
    pub lsn: u64,
    pub barrier: CommitBarrier,
    pub queue: Option<super::commit::Ticket>,
    pub target: CommitTarget,
}

#[derive(Clone, Copy)]
struct CommitAdmission {
    target: CommitTarget,
    clock: super::datetime::StatementClock,
    deadline: i64,
    prepared_locks_staged: bool,
    prepared_resolution_staged: bool,
    wal_prefix: usize,
}

#[derive(Clone, Copy)]
pub(super) struct CommitControl {
    mode: TxnMode,
    txid: u32,
    isolation: TransactionIsolation,
    read_only: bool,
    deferrable: bool,
    replication_apply: bool,
    snapshot_lsn: Option<u64>,
    snapshot_taken: bool,
    transaction_start: Option<i64>,
    command_id: u32,
    command_counter: u32,
}

pub(super) struct CacheCapacityWait {
    pub(super) failure_epoch: Option<u64>,
    pub(super) clock: super::datetime::StatementClock,
    pub(super) deadline: i64,
    pub(super) context: super::exec::PlpgsqlTransactionContext,
}

pub struct TxnState {
    plan_invalidations: Option<std::sync::Arc<super::plan_invalidations::LocalPlanInvalidations>>,
    staged_commit: Option<StagedCommit>,
    commit_admission: Option<CommitAdmission>,
    pub(super) catalog_history: Option<crate::storage::CatalogHistoryBatch>,
    pub mode: TxnMode,
    /// An error occurred inside an explicit block: everything until
    /// COMMIT/ROLLBACK fails with 25P02.
    pub failed: bool,
    // One SQL command can commit several transactions. A caught cancellation
    // stays consumed until the command ends, including across those commits.
    statement_timeout_consumed: bool,
    pub txid: u32,
    /// The isolation level promised to this transaction. READ COMMITTED takes
    /// a fresh durable-LSN snapshot per statement; REPEATABLE READ pins the
    /// first data statement's snapshot until commit or rollback.
    pub isolation: TransactionIsolation,
    pub read_only: bool,
    pub deferrable: bool,
    sources: TransactionSources,
    /// True only while pgoutput applies one remote transaction. Trigger
    /// dispatch reads this typed execution origin instead of guessing from a
    /// connection or statement string.
    pub replication_apply: bool,
    extension_script: bool,
    snapshot_lsn: Option<u64>,
    snapshot_taken: bool,
    transaction_start: Option<i64>,
    suspended_statement: Option<SuspendedStatement>,
    pub(super) write_completion_boundary: Option<WriteCompletionBoundary>,
    pub(super) pending_write_completion: Option<super::exec::SuspendedWriteCompletion>,
    pub(super) cache_capacity_wait: Option<CacheCapacityWait>,
    pub(super) checkpoint_request: Option<crate::checkpoint::CheckpointRequest>,
    /// Current statement identity. Snapshot K sees own writes with identifiers
    /// below K. Nested SQL temporarily uses a separately issued identity.
    command_id: u32,
    /// The greatest issued identity, including nested commands whose caller
    /// context has already been restored. Identities are never reused.
    command_counter: u32,
    /// Nested trigger-side SQL is bounded independently of row and arena
    /// capacity so a self-referential trigger cannot consume the call stack.
    trigger_depth: u16,
    rule_stack: [i32; MAX_RULE_NESTING],
    rule_depth: u8,
    /// A concurrent partition detach published its first durable phase. The
    /// protocol executor commits that phase before it parks for finalization.
    concurrent_partition_detach_pending: bool,
    pub(super) concurrent_index: Option<super::concurrent_index::Command>,
    pub(super) immediate_index_utility: Option<super::UtilityCommand>,
    /// Every row write, in order: (table slot, rowid, pending image before the
    /// write). Recorded per write (not per row) so `ROLLBACK TO SAVEPOINT` can
    /// reverse-replay to any earlier point.
    touched: FixedVec<(u32, u64, PriorPending)>,
    truncates: FixedVec<TruncateEvent>,
    truncate_wal_tables: FixedBuf,
    /// DDL performed in this transaction, for rollback.
    ddl: FixedVec<DdlUndo>,
    ddl_origins: FixedVec<u32>,
    ddl_plan_events: FixedVec<super::plan_invalidations::PlanInvalidationEvent>,
    ddl_origin: u32,
    next_ddl_origin: u32,
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
    deferred_constraints: FixedVec<ConstraintObligation>,
    completed_constraints: FixedVec<usize>,
    constraint_modes: FixedVec<ConstraintModeChange>,
    constraint_renames: FixedVec<ConstraintLifecycle>,
    deferred_triggers: FixedVec<DeferredTriggerEvent>,
    completed_deferred_triggers: FixedVec<usize>,
    deferred_trigger_bytes: FixedBuf,
    large_object_descriptors: FixedVec<LargeObjectDescriptor>,
}

/// How to undo one DDL statement.
#[derive(Debug, Clone)]
#[allow(
    clippy::large_enum_variant,
    reason = "CommentSet carries its payload inline (no heap after startup); the undo list is a small fixed pool"
)]
pub(crate) enum DdlUndo {
    TemporaryNamespaceCreated(crate::storage::TemporaryNamespaceIdentity),
    /// CREATE TABLE at this slot — undo by dropping it.
    Created(u32),
    /// DROP TABLE at this slot (rows retained until commit) — undo by
    /// reviving it (and its indexes).
    Dropped(crate::storage::CatalogDdlUndo),
    /// ALTER TABLE appended one pending table-definition version. Row-image
    /// undo is carried by the ordinary touched-row log.
    TableAltered(crate::storage::TableDefinitionChange),
    ViewChanged(crate::storage::ViewPublication),
    ViewDropped(crate::storage::CatalogDdlUndo),
    MatviewPopulationChanged(crate::storage::MatviewPopulationUndo),
    MatviewQueryChanged(crate::storage::MatviewQueryUndo),
    RuleChanged {
        publication: crate::storage::RulePublication,
        prior_table_rule_txid: Option<u32>,
    },
    RuleAltered(crate::storage::RuleAlterUndo),
    RuleDropped(crate::storage::CatalogDdlUndo),
    RoutineChanged(crate::storage::RoutineChange),
    CastCreated(u32),
    CastDropped(u32),
    OperatorCreated(u32),
    OperatorAltered {
        slot: u32,
        prior: Option<crate::storage::PendingOperatorDefinition>,
    },
    OperatorDropped(u32),
    OperatorFamilyCreated(u32),
    OperatorFamilyAltered {
        slot: u32,
        prior: Option<crate::storage::PendingOperatorFamilyDefinition>,
    },
    OperatorFamilyDropped(u32),
    OperatorClassCreated(u32),
    OperatorClassAltered {
        slot: u32,
        prior: Option<crate::storage::PendingOperatorClassDefinition>,
    },
    OperatorClassDropped(u32),
    CollationCreated(u32),
    CollationAltered {
        slot: u32,
        prior: Option<crate::storage::PendingCollationDefinition>,
    },
    CollationDropped(u32),
    ConversionCreated(u32),
    ConversionAltered {
        slot: u32,
        prior: Option<crate::storage::PendingConversionDefinition>,
    },
    ConversionDropped(u32),
    TextSearchCreated(u32),
    TextSearchAltered {
        slot: u32,
        prior: Option<crate::storage::PendingTextSearchDefinition>,
    },
    TextSearchDropped(u32),
    EventTriggerCreated(u32),
    EventTriggerAltered {
        slot: u32,
        prior: Option<crate::storage::PendingEventTriggerDefinition>,
    },
    EventTriggerDropped(u32),
    TriggerCreated(u32),
    TriggerDropped(u32),
    TriggerAltered {
        slot: u32,
        prior: Option<crate::storage::PendingTriggerDefinition>,
    },
    PartitionTriggerAltered {
        slot: u32,
        prior: crate::storage::PartitionTriggerState,
    },
    PolicyCreated(u32),
    PolicyDropped(u32),
    PolicyAltered {
        slot: u32,
        prior: Option<crate::storage::PendingPolicyDefinition>,
    },
    StatisticsCreated(u32),
    StatisticsDropped(u32),
    StatisticsAltered {
        slot: u32,
        prior: Option<crate::storage::PendingExtendedStatisticsDefinition>,
    },
    StatisticsKeysAltered {
        slot: u32,
        prior: Option<crate::storage::PendingExtendedStatisticsKeys>,
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
    ForeignDataWrapperCreated(u32),
    ForeignDataWrapperAltered {
        slot: u32,
        prior: Option<
            crate::storage::foreign::PendingForeignDefinition<
                crate::storage::foreign::ForeignDataWrapperDefinition,
            >,
        >,
    },
    ForeignDataWrapperDropped(u32),
    ForeignServerCreated(u32),
    ForeignServerAltered {
        slot: u32,
        prior: Option<
            crate::storage::foreign::PendingForeignDefinition<
                crate::storage::foreign::ForeignServerDefinition,
            >,
        >,
    },
    ForeignServerDropped(u32),
    UserMappingCreated(u32),
    UserMappingAltered {
        slot: u32,
        prior: Option<
            crate::storage::foreign::PendingForeignDefinition<
                crate::storage::foreign::UserMappingDefinition,
            >,
        >,
    },
    UserMappingDropped(u32),
    ForeignTableCreated(u32),
    ForeignTableAltered {
        slot: u32,
        prior: Option<
            crate::storage::foreign::PendingForeignDefinition<
                crate::storage::foreign::ForeignTableDefinition,
            >,
        >,
    },
    ForeignTableDropped(u32),
    ForeignOwnerChanged {
        class: crate::storage::foreign::ForeignObjectClass,
        slot: u32,
        prior: Option<crate::storage::PendingOwnership>,
    },
    SubscriptionCreated(u32),
    SubscriptionDropped(u32),
    SubscriptionEnabled {
        slot: u32,
        prior: Option<crate::storage::PendingSubscriptionEnabled>,
    },
    SubscriptionBootstrapChanged {
        slot: u32,
        prior: Option<crate::storage::PendingSubscriptionBootstrap>,
    },
    SubscriptionRelationsChanged,
    SubscriptionDefinitionChanged {
        slot: u32,
        prior: Option<crate::storage::PendingSubscriptionDefinition>,
    },
    SubscriptionOwnerChanged {
        slot: u32,
        prior: Option<crate::storage::PendingOwnership>,
    },
    SubscriptionRenamed {
        slot: u32,
        prior: Option<crate::storage::PendingSubscriptionName>,
    },
    /// CREATE MATERIALIZED VIEW at this slot — undo by dropping it.
    MatviewChanged(crate::storage::MatviewDdlUndo),
    /// DROP MATERIALIZED VIEW at this slot — undo by reviving it.
    /// Sequence definitions and transactional restarts retain their selected source.
    SequenceChanged(crate::storage::SequenceChange),
    LargeObjectCreated(u32),
    LargeObjectDropped(u32),
    /// CREATE DOMAIN retains its source identity and transaction owner.
    DomainCreated(crate::storage::TypeDdlUndo),
    /// DROP DOMAIN retains its source identity and transaction owner.
    DomainDropped(crate::storage::TypeDdlUndo),
    DomainAltered(crate::storage::DomainAlterUndo),
    /// CREATE TYPE (enum) retains its source identity and transaction owner.
    EnumCreated(crate::storage::TypeDdlUndo),
    /// CREATE TYPE (...): named composites are distinct from anonymous records.
    CompositeCreated(crate::storage::TypeDdlUndo),
    /// ALTER TYPE on a named composite staged a private physical layout.
    CompositeAltered(crate::storage::CompositeAlterUndo),
    /// DROP TYPE retains its source identity and transaction owner.
    CompositeDropped(crate::storage::TypeDdlUndo),
    /// DROP TYPE (enum) retains its source identity and transaction owner.
    EnumDropped(crate::storage::TypeDdlUndo),
    /// ALTER TYPE staged a definition visible only to its transaction.
    EnumAltered(crate::storage::EnumAlterUndo),
    /// CREATE INDEX at this slot — undo by dropping it.
    IndexCreated(u32),
    /// DROP INDEX at this slot — undo by reviving it.
    IndexDropped(u32),
    IndexRenamed {
        slot: u32,
        prior: Option<crate::storage::PendingIndexName>,
    },
    IndexAltered {
        slot: u32,
        prior: Option<crate::storage::PendingIndexDefinition>,
    },
    TablespaceCreated(u32),
    TablespaceAltered {
        slot: u32,
        prior_definition: Option<crate::storage::PendingTablespaceDefinition>,
        prior_owner: Option<crate::storage::PendingOwnership>,
    },
    TablespaceDropped(u32),
    AccessMethodCreated(u32),
    AccessMethodDropped(u32),
    DatabaseCreated(u32),
    DatabaseAltered {
        slot: u32,
        prior_definition: Option<crate::storage::PendingDatabaseDefinition>,
        prior_owner: Option<crate::storage::PendingOwnership>,
    },
    DatabaseDropped(u32),
    /// CREATE SCHEMA at this slot — undo by dropping it.
    SchemaCreated(u32),
    /// DROP SCHEMA at this slot — undo by reviving it.
    SchemaDropped(u32),
    /// ALTER SCHEMA ... RENAME TO changed a schema identity in place.
    SchemaRenamed(crate::storage::SchemaNameUndo),
    ExtensionCreated(u32),
    ExtensionDropped(u32),
    ExtensionAltered {
        slot: u32,
        prior: Option<crate::storage::PendingExtensionDefinition>,
    },
    ExtensionDependencyChanged {
        slot: u32,
        prior: Option<crate::storage::PendingExtensionDependency>,
    },
    ExtensionConfigChanged {
        slot: u32,
        prior: Option<crate::storage::PendingExtensionConfig>,
    },
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
    RoleSettingChanged {
        slot: u32,
        prior: Option<crate::storage::PendingRoleSetting>,
    },
    SystemSettingChanged {
        slot: u32,
        prior: Option<crate::storage::PendingRoleSetting>,
    },
    ObjectOwnerChanged {
        object: crate::storage::AccessObject,
        prior: Option<crate::storage::PendingOwnership>,
    },
    ObjectAclChanged {
        slot: u32,
        prior: crate::storage::ObjectAclUndo,
    },
    ColumnAclChanged {
        slot: u32,
        prior: Option<crate::storage::PendingAcl>,
    },
    DefaultAclChanged {
        slot: u32,
        prior: Option<crate::storage::PendingDefaultAcl>,
    },
    ParameterAclChanged {
        prior: crate::storage::ParameterAclUndo,
    },
    /// A `COMMENT ON` set or removed an object's comment — undo by restoring
    /// the slot's prior uncommitted overlay. On commit, the overlay is
    /// promoted and journaled.
    CommentSet {
        slot: u32,
        prior: Option<crate::storage::PendingComment>,
    },
    /// A table-constraint rename changed a comment's transaction-local name.
    CommentIdentityChanged {
        slot: u32,
        prior: Option<crate::storage::PendingCommentIdentity>,
    },
}

const DEFAULT_DDL_CAPACITY: usize = 64;
const DEFAULT_MAX_TXN_ANALYZE: usize = 64;
const SUBSCRIPTION_ADVANCES_PER_TXN: usize = 1;
const DEFAULT_MAX_DEFERRED_CONSTRAINTS: usize = 128;

/// A table constraint is versioned across DROP/recreate while a constraint
/// trigger has a stable catalog slot. Keeping these identities distinct means
/// renaming a trigger cannot orphan an already queued firing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ConstraintIdentity {
    Table {
        table: u32,
        name: crate::storage::SqlName,
        generation: u16,
    },
    Trigger {
        table: u32,
        slot: u16,
    },
}

/// One row whose transaction-visible state must satisfy a deferred
/// constraint at the next applicable boundary.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ConstraintObligation {
    pub(crate) constraint: ConstraintIdentity,
    pub(crate) rowid: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ConstraintModeChange {
    /// `None` represents `SET CONSTRAINTS ALL` without expanding catalog state
    /// into the transaction pool.
    pub(crate) constraint: Option<ConstraintIdentity>,
    pub(crate) mode: crate::sql::ast::ConstraintMode,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct DeferredTriggerTuple {
    pub(crate) offset: u32,
    pub(crate) length: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct DeferredTriggerEvent {
    pub(crate) kind: DeferredTriggerKind,
    pub(crate) event: u8,
    pub(crate) updated_columns: ColumnSet,
    pub(crate) old: Option<DeferredTriggerTuple>,
    pub(crate) new: Option<DeferredTriggerTuple>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DeferredTriggerKind {
    Constraint {
        identity: ConstraintIdentity,
        effective_table: u16,
        trigger_depth: u16,
    },
    AfterRow {
        trigger_slot: u16,
        effective_table: u16,
        trigger_depth: u16,
    },
}

pub const DEFERRED_TRIGGER_BYTES: usize = 256 * 1024;

#[derive(Clone, Copy)]
pub(crate) struct TransactionCapacity {
    rows: usize,
    ddl: usize,
    descriptors: usize,
    savepoints: usize,
    deferred_constraints: usize,
    deferred_trigger_bytes: usize,
    analyze: usize,
    tables: usize,
}

impl TransactionCapacity {
    pub(crate) const fn from_config(config: &crate::config::Config) -> Self {
        Self {
            rows: config.txn_rows,
            ddl: config.max_ddl_per_transaction,
            descriptors: config.max_large_object_descriptors,
            savepoints: config.max_savepoints_per_transaction,
            deferred_constraints: config.max_deferred_constraints_per_transaction,
            deferred_trigger_bytes: config.deferred_trigger_bytes,
            analyze: config.max_analyze_per_transaction,
            tables: config.max_tables.saturating_add(1),
        }
    }

    const fn with_defaults(rows: usize, descriptors: usize, ddl: usize) -> Self {
        Self {
            rows,
            ddl,
            descriptors,
            savepoints: DEFAULT_MAX_SAVEPOINTS,
            deferred_constraints: DEFAULT_MAX_DEFERRED_CONSTRAINTS,
            deferred_trigger_bytes: DEFERRED_TRIGGER_BYTES,
            analyze: DEFAULT_MAX_TXN_ANALYZE,
            tables: DEFAULT_MAX_TRUNCATE_TABLES,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ConstraintLifecycle {
    Rename {
        identity: ConstraintIdentity,
        to: crate::storage::SqlName,
    },
    Drop {
        identity: ConstraintIdentity,
        next_generation: u16,
    },
}

#[derive(Debug, Clone, Copy)]
pub(crate) enum StatisticsUndo {
    Table {
        table: u32,
        event: Option<super::plan_invalidations::PlanInvalidationEvent>,
    },
    Extended(u32),
}

/// An eligible transaction cannot advance while its import is authenticated.
pub(crate) struct SnapshotImport<'txn> {
    transaction: &'txn mut TxnState,
}

impl SnapshotImport<'_> {
    pub(crate) fn transaction(&self) -> u32 {
        self.transaction.txid
    }

    pub(crate) fn accept(self, snapshot: u64) -> Result<(), SqlError> {
        if self.transaction.isolation == TransactionIsolation::Serializable {
            return Err(sql_err!(
                sqlstate::FEATURE_NOT_SUPPORTED,
                "a serializable transaction cannot import a snapshot from a non-serializable transaction"
            ));
        }
        self.transaction.snapshot_lsn = Some(snapshot);
        self.transaction.snapshot_taken = true;
        Ok(())
    }
}

impl TxnState {
    pub const fn budget_bytes(capacity: usize) -> usize {
        Self::budget_bytes_with_large_objects(
            capacity,
            DEFAULT_MAX_LARGE_OBJECT_DESCRIPTORS,
            DEFAULT_DDL_CAPACITY,
        )
    }

    pub const fn budget_bytes_with_large_objects(
        capacity: usize,
        descriptor_capacity: usize,
        ddl_capacity: usize,
    ) -> usize {
        Self::budget_bytes_with_capacity(TransactionCapacity::with_defaults(
            capacity,
            descriptor_capacity,
            ddl_capacity,
        ))
    }

    pub(crate) const fn budget_bytes_with_config(config: &crate::config::Config) -> usize {
        Self::budget_bytes_with_capacity(TransactionCapacity::from_config(config))
    }

    const fn budget_bytes_with_capacity(capacity: TransactionCapacity) -> usize {
        capacity
            .rows
            .saturating_mul(core::mem::size_of::<(u32, u64, PriorPending)>())
            .saturating_add(
                capacity
                    .ddl
                    .saturating_mul(core::mem::size_of::<TruncateEvent>()),
            )
            .saturating_add(capacity.tables.saturating_mul(128))
            .saturating_add(capacity.ddl.saturating_mul(core::mem::size_of::<DdlUndo>()))
            .saturating_add(capacity.ddl.saturating_mul(core::mem::size_of::<u32>()))
            .saturating_add(capacity.ddl.saturating_mul(core::mem::size_of::<
                super::plan_invalidations::PlanInvalidationEvent,
            >()))
            .saturating_add(
                capacity
                    .analyze
                    .saturating_mul(core::mem::size_of::<StatisticsUndo>()),
            )
            .saturating_add(
                capacity
                    .savepoints
                    .saturating_mul(core::mem::size_of::<Savepoint>()),
            )
            .saturating_add(
                crate::sql::notify::PER_TXN
                    .saturating_mul(core::mem::size_of::<crate::sql::notify::BufferedNotify>()),
            )
            .saturating_add(crate::sql::notify::PER_TXN_PAYLOAD_BYTES)
            .saturating_add(
                crate::sql::notify::LISTEN_OPS_PER_TXN
                    .saturating_mul(core::mem::size_of::<crate::sql::notify::ListenOp>()),
            )
            .saturating_add(
                SUBSCRIPTION_ADVANCES_PER_TXN
                    .saturating_mul(core::mem::size_of::<crate::storage::SubscriptionAdvance>()),
            )
            .saturating_add(
                capacity
                    .deferred_constraints
                    .saturating_mul(core::mem::size_of::<ConstraintObligation>()),
            )
            .saturating_add(
                capacity
                    .deferred_constraints
                    .saturating_mul(core::mem::size_of::<usize>()),
            )
            .saturating_add(
                capacity
                    .deferred_constraints
                    .saturating_mul(core::mem::size_of::<ConstraintModeChange>()),
            )
            .saturating_add(
                capacity
                    .deferred_constraints
                    .saturating_mul(core::mem::size_of::<ConstraintLifecycle>()),
            )
            .saturating_add(
                capacity
                    .deferred_constraints
                    .saturating_mul(core::mem::size_of::<DeferredTriggerEvent>()),
            )
            .saturating_add(
                capacity
                    .deferred_constraints
                    .saturating_mul(core::mem::size_of::<usize>()),
            )
            .saturating_add(capacity.deferred_trigger_bytes)
            .saturating_add(
                capacity
                    .descriptors
                    .saturating_mul(core::mem::size_of::<LargeObjectDescriptor>()),
            )
            .saturating_add(core::mem::size_of::<
                Option<std::sync::Arc<super::plan_invalidations::LocalPlanInvalidations>>,
            >())
            .saturating_add(core::mem::size_of::<Option<super::concurrent_index::Command>>())
            .saturating_add(core::mem::size_of::<Option<super::UtilityCommand>>())
    }

    pub fn new(budget: &mut Budget, capacity: usize) -> Result<Self, BudgetError> {
        Self::new_with_large_objects(
            budget,
            capacity,
            DEFAULT_MAX_LARGE_OBJECT_DESCRIPTORS,
            DEFAULT_DDL_CAPACITY,
        )
    }

    #[cfg(test)]
    pub(crate) fn new_with_ddl_capacity(
        budget: &mut Budget,
        capacity: usize,
        ddl_capacity: usize,
    ) -> Result<Self, BudgetError> {
        Self::new_with_large_objects(
            budget,
            capacity,
            DEFAULT_MAX_LARGE_OBJECT_DESCRIPTORS,
            ddl_capacity,
        )
    }

    pub fn new_with_large_objects(
        budget: &mut Budget,
        capacity: usize,
        descriptor_capacity: usize,
        ddl_capacity: usize,
    ) -> Result<Self, BudgetError> {
        Self::new_with_capacity(
            budget,
            TransactionCapacity::with_defaults(capacity, descriptor_capacity, ddl_capacity),
        )
    }

    pub fn new_with_config(
        budget: &mut Budget,
        config: &crate::config::Config,
    ) -> Result<Self, BudgetError> {
        Self::new_with_capacity(budget, TransactionCapacity::from_config(config))
    }

    fn new_with_capacity(
        budget: &mut Budget,
        capacity: TransactionCapacity,
    ) -> Result<Self, BudgetError> {
        budget.draw(
            core::mem::size_of::<
                Option<std::sync::Arc<super::plan_invalidations::LocalPlanInvalidations>>,
            >(),
            "transaction plan invalidation owner",
        )?;
        budget.draw(
            core::mem::size_of::<Option<super::concurrent_index::Command>>()
                + core::mem::size_of::<Option<super::UtilityCommand>>(),
            "concurrent index command",
        )?;
        let mut large_object_descriptors =
            FixedVec::new(budget, "large_object_descriptors", capacity.descriptors)?;
        for _ in 0..capacity.descriptors {
            large_object_descriptors
                .push(LargeObjectDescriptor::EMPTY)
                .expect("sized to descriptor capacity");
        }
        Ok(Self {
            plan_invalidations: None,
            staged_commit: None,
            commit_admission: None,
            catalog_history: None,
            mode: TxnMode::Idle,
            failed: false,
            statement_timeout_consumed: false,
            txid: 0,
            isolation: TransactionIsolation::ReadCommitted,
            read_only: false,
            deferrable: false,
            sources: TransactionSources::default(),
            replication_apply: false,
            extension_script: false,
            snapshot_lsn: None,
            snapshot_taken: false,
            transaction_start: None,
            suspended_statement: None,
            write_completion_boundary: None,
            pending_write_completion: None,
            cache_capacity_wait: None,
            checkpoint_request: None,
            command_id: 1,
            command_counter: 1,
            trigger_depth: 0,
            rule_stack: [0; MAX_RULE_NESTING],
            rule_depth: 0,
            concurrent_partition_detach_pending: false,
            concurrent_index: None,
            immediate_index_utility: None,
            touched: FixedVec::new(budget, "txn_touched", capacity.rows)?,
            truncates: FixedVec::new(budget, "txn_truncates", capacity.ddl)?,
            truncate_wal_tables: FixedBuf::new(
                budget,
                "txn_truncate_wal_tables",
                capacity.tables * 128,
            )?,
            ddl: FixedVec::new(budget, "txn_ddl", capacity.ddl)?,
            ddl_origins: FixedVec::new(budget, "txn_ddl_origins", capacity.ddl)?,
            ddl_plan_events: FixedVec::new(budget, "txn_plan_invalidations", capacity.ddl)?,
            ddl_origin: 0,
            next_ddl_origin: 0,
            statistics_undo: FixedVec::new(budget, "txn_statistics_undo", capacity.analyze)?,
            savepoints: FixedVec::new(budget, "txn_savepoints", capacity.savepoints)?,
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
            deferred_constraints: FixedVec::new(
                budget,
                "txn_deferred_constraints",
                capacity.deferred_constraints,
            )?,
            completed_constraints: FixedVec::new(
                budget,
                "txn_completed_constraints",
                capacity.deferred_constraints,
            )?,
            constraint_modes: FixedVec::new(
                budget,
                "txn_constraint_modes",
                capacity.deferred_constraints,
            )?,
            constraint_renames: FixedVec::new(
                budget,
                "txn_constraint_renames",
                capacity.deferred_constraints,
            )?,
            deferred_triggers: FixedVec::new(
                budget,
                "txn_deferred_triggers",
                capacity.deferred_constraints,
            )?,
            completed_deferred_triggers: FixedVec::new(
                budget,
                "txn_completed_deferred_triggers",
                capacity.deferred_constraints,
            )?,
            deferred_trigger_bytes: FixedBuf::new(
                budget,
                "txn_deferred_trigger_bytes",
                capacity.deferred_trigger_bytes,
            )?,
            large_object_descriptors,
        })
    }

    pub(crate) fn statement_large_object_descriptors<'a>(
        &self,
        arena: &'a crate::mem::arena::Arena,
    ) -> Result<&'a [LargeObjectDescriptor], SqlError> {
        let count = self
            .large_object_descriptors
            .iter()
            .rposition(|descriptor| descriptor.active)
            .map_or(0, |index| index + 1);
        arena
            .alloc_slice_copy(&self.large_object_descriptors[..count])
            .map(|descriptors| &*descriptors)
            .map_err(|_| {
                sql_err!(
                    sqlstate::PROGRAM_LIMIT_EXCEEDED,
                    "large-object retry state exceeds statement memory"
                )
            })
    }

    pub(crate) fn restore_statement_large_object_descriptors(
        &mut self,
        descriptors: &[LargeObjectDescriptor],
    ) {
        assert!(descriptors.len() <= self.large_object_descriptors.len());
        self.large_object_descriptors[..descriptors.len()].copy_from_slice(descriptors);
        self.large_object_descriptors[descriptors.len()..].fill(LargeObjectDescriptor::EMPTY);
    }

    pub(crate) fn matches_statement_large_object_descriptors(
        &self,
        saved: &[LargeObjectDescriptor],
    ) -> bool {
        self.large_object_descriptors.get(..saved.len()) == Some(saved)
            && self.large_object_descriptors[saved.len()..]
                .iter()
                .all(|descriptor| !descriptor.active)
    }

    pub(crate) fn open_large_object_descriptor(
        &mut self,
        oid: crate::storage::LargeObjectOid,
        mode: LargeObjectDescriptorMode,
    ) -> Result<i32, SqlError> {
        let slot = self
            .large_object_descriptors
            .iter()
            .position(|descriptor| !descriptor.active)
            .ok_or_else(|| {
                sql_err!(
                    sqlstate::PROGRAM_LIMIT_EXCEEDED,
                    "too many open large objects (limit {})",
                    self.large_object_descriptors.len()
                )
            })?;
        self.large_object_descriptors[slot] = LargeObjectDescriptor {
            oid,
            position: 0,
            mode,
            opened_savepoint_depth: self.savepoints.len(),
            active: true,
        };
        Ok(slot as i32)
    }

    pub(crate) fn large_object_descriptor(
        &self,
        fd: i32,
    ) -> Result<&LargeObjectDescriptor, SqlError> {
        let descriptor = usize::try_from(fd)
            .ok()
            .and_then(|slot| self.large_object_descriptors.get(slot))
            .filter(|descriptor| descriptor.active)
            .ok_or_else(|| {
                sql_err!(
                    sqlstate::UNDEFINED_OBJECT,
                    "invalid large-object descriptor: {}",
                    fd
                )
            })?;
        Ok(descriptor)
    }

    pub(crate) fn large_object_descriptor_mut(
        &mut self,
        fd: i32,
    ) -> Result<&mut LargeObjectDescriptor, SqlError> {
        let slot = usize::try_from(fd).ok().filter(|slot| {
            self.large_object_descriptors
                .get(*slot)
                .is_some_and(|descriptor| descriptor.active)
        });
        slot.map(|slot| &mut self.large_object_descriptors[slot])
            .ok_or_else(|| {
                sql_err!(
                    sqlstate::UNDEFINED_OBJECT,
                    "invalid large-object descriptor: {}",
                    fd
                )
            })
    }

    pub(crate) fn close_large_object_descriptor(&mut self, fd: i32) -> Result<(), SqlError> {
        self.large_object_descriptor_mut(fd)?.active = false;
        Ok(())
    }

    pub(crate) fn rollback_large_object_descriptors_to(&mut self, savepoint_depth: usize) {
        for descriptor in self.large_object_descriptors.iter_mut() {
            if descriptor.active && descriptor.opened_savepoint_depth > savepoint_depth {
                descriptor.active = false;
            }
        }
    }

    pub fn is_active(&self) -> bool {
        self.mode != TxnMode::Idle
    }

    pub(crate) fn restore_prepared_identity(&mut self, transaction_id: u32) {
        debug_assert_eq!(self.txid, 0);
        debug_assert_eq!(self.mode, TxnMode::Idle);
        self.txid = transaction_id;
        self.mode = TxnMode::Explicit;
    }

    pub fn is_explicit(&self) -> bool {
        self.mode == TxnMode::Explicit
    }

    pub(crate) fn is_implicit(&self) -> bool {
        matches!(self.mode, TxnMode::Implicit | TxnMode::ImplicitBlock)
    }

    pub(crate) fn is_transaction_block(&self) -> bool {
        matches!(self.mode, TxnMode::Explicit | TxnMode::ImplicitBlock)
    }

    pub(crate) fn begin_implicit_block(&mut self) {
        if self.mode == TxnMode::Implicit {
            self.mode = TxnMode::ImplicitBlock;
        }
    }

    pub(crate) fn defer_concurrent_index_cancel(&mut self) {
        if let Some(command) = &mut self.concurrent_index {
            command.cancel_after_commit = true;
        }
    }

    pub(crate) fn begin_concurrent_partition_detach(&mut self) {
        self.concurrent_partition_detach_pending = true;
    }

    pub(crate) fn concurrent_partition_detach_pending(&self) -> bool {
        self.concurrent_partition_detach_pending
    }

    pub(crate) fn take_concurrent_partition_detach(&mut self) -> bool {
        core::mem::take(&mut self.concurrent_partition_detach_pending)
    }

    pub(crate) fn has_savepoints(&self) -> bool {
        !self.savepoints.is_empty()
    }

    pub(crate) fn savepoint_names(&self) -> impl Iterator<Item = StackStr<63>> + '_ {
        self.savepoints.iter().map(|savepoint| savepoint.name)
    }

    pub(crate) fn publish_foreign_statement_context(
        &self,
        storage: &Storage,
    ) -> Result<(), SqlError> {
        // A remote connection can first open inside a procedural block, after
        // command admission. Its savepoint stack must include that block.
        storage.set_foreign_statement_context(
            self.txid,
            self.isolation == TransactionIsolation::Serializable,
            self.savepoint_names(),
        )
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
    fn next_command_identity(&self) -> Result<u32, SqlError> {
        self.command_counter
            .checked_add(1)
            .filter(|command| *command < crate::storage::SNAPSHOT_ALL)
            .ok_or_else(|| {
                sql_err!(
                    sqlstate::PROGRAM_LIMIT_EXCEEDED,
                    "transaction command identity space is exhausted"
                )
            })
    }

    pub(crate) fn begin_statement_clock(&self) {
        if let Some(admission) = &self.commit_admission {
            super::datetime::restore_statement_clock(admission.clock);
        } else if let Some(suspended) = &self.suspended_statement {
            super::datetime::restore_statement_clock(suspended.clock);
        } else if let Some(wait) = &self.cache_capacity_wait {
            super::datetime::restore_statement_clock(wait.clock);
        } else {
            super::datetime::begin_statement();
            if let Some(start) = self.transaction_start {
                super::datetime::restore_transaction_clock(start);
            }
        }
    }

    pub(crate) fn activate_transaction_clock(&mut self) {
        let start = *self
            .transaction_start
            .get_or_insert_with(super::datetime::statement_micros);
        super::datetime::restore_transaction_clock(start);
    }

    pub(super) fn commit_control(&self) -> CommitControl {
        CommitControl {
            mode: self.mode,
            txid: self.txid,
            isolation: self.isolation,
            read_only: self.read_only,
            deferrable: self.deferrable,
            replication_apply: self.replication_apply,
            snapshot_lsn: self.snapshot_lsn,
            snapshot_taken: self.snapshot_taken,
            transaction_start: self.transaction_start,
            command_id: self.command_id,
            command_counter: self.command_counter,
        }
    }

    pub(super) fn install_commit_wait(&mut self, control: CommitControl, staged: StagedCommit) {
        assert!(
            !self.is_active() && self.staged_commit.is_none(),
            "empty commit buffer"
        );
        self.mode = control.mode;
        self.txid = control.txid;
        self.isolation = control.isolation;
        self.read_only = control.read_only;
        self.deferrable = control.deferrable;
        self.replication_apply = control.replication_apply;
        self.snapshot_lsn = control.snapshot_lsn;
        self.snapshot_taken = control.snapshot_taken;
        self.transaction_start = control.transaction_start;
        self.command_id = control.command_id;
        self.command_counter = control.command_counter;
        self.staged_commit = Some(staged);
    }

    pub(crate) fn staged_commit(&self) -> Option<StagedCommit> {
        self.staged_commit
    }

    /// SQL effects are complete, but admission may still wait before WAL is
    /// committed. Unlike a staged commit, this phase remains cancellable.
    pub(crate) fn begin_commit_admission(
        &mut self,
        target: CommitTarget,
        wal_prefix: usize,
    ) -> usize {
        assert!(self.is_active() && self.staged_commit.is_none());
        if let Some(admission) = &self.commit_admission {
            assert_eq!(
                admission.target, target,
                "commit admission target is retained"
            );
        } else {
            self.commit_admission = Some(CommitAdmission {
                target,
                clock: super::datetime::statement_clock(),
                deadline: super::query::statement_deadline(),
                prepared_locks_staged: false,
                prepared_resolution_staged: false,
                wal_prefix,
            });
        }
        self.commit_admission
            .expect("admission is initialized")
            .wal_prefix
    }

    pub(crate) fn commit_admission(&self) -> Option<CommitTarget> {
        self.commit_admission.map(|admission| admission.target)
    }

    pub(super) fn prepared_locks_staged(&self) -> bool {
        self.commit_admission
            .as_ref()
            .expect("prepared lock image requires admission")
            .prepared_locks_staged
    }

    pub(super) fn mark_prepared_locks_staged(&mut self, wal_prefix: usize) {
        let admission = self
            .commit_admission
            .as_mut()
            .expect("prepared lock image requires admission");
        assert!(matches!(admission.target, CommitTarget::Prepare { .. }));
        admission.prepared_locks_staged = true;
        admission.wal_prefix = wal_prefix;
    }

    pub(super) fn prepared_resolution_staged(&self) -> bool {
        self.commit_admission
            .as_ref()
            .expect("prepared resolution requires admission")
            .prepared_resolution_staged
    }

    pub(super) fn mark_prepared_resolution_staged(&mut self, wal_prefix: usize) {
        let admission = self
            .commit_admission
            .as_mut()
            .expect("prepared resolution requires admission");
        assert!(matches!(
            admission.target,
            CommitTarget::CommitPrepared { .. } | CommitTarget::RollbackPrepared { .. }
        ));
        admission.prepared_resolution_staged = true;
        admission.wal_prefix = wal_prefix;
    }

    pub(crate) fn stage_commit(&mut self, lsn: u64, target: CommitTarget, barrier: CommitBarrier) {
        assert!(
            self.staged_commit.is_none(),
            "commit staging is performed once"
        );
        if let Some(admission) = self.commit_admission.take() {
            assert_eq!(
                admission.target, target,
                "admitted commit target is retained"
            );
        }
        self.staged_commit = Some(StagedCommit {
            queue: None,
            lsn,
            target,
            barrier,
        });
    }

    pub(crate) fn take_staged_commit(&mut self) -> Option<StagedCommit> {
        self.staged_commit.take()
    }

    pub(crate) fn suspended_statement_deadline(&self) -> Option<i64> {
        let deadline = self
            .commit_admission
            .as_ref()
            .map(|admission| admission.deadline)
            .or_else(|| {
                self.suspended_statement
                    .as_ref()
                    .map(|suspended| suspended.deadline)
            })
            .or_else(|| self.cache_capacity_wait.as_ref().map(|wait| wait.deadline));
        if self.statement_timeout_consumed {
            deadline.map(|_| 0)
        } else {
            deadline
        }
    }

    pub(crate) fn enter_statement_timeout(&mut self, entry: super::StatementTimeoutEntry) {
        if self.staged_commit.is_some() {
            super::query::disarm_timeout();
            return;
        }
        let timeout_ms = match entry {
            super::StatementTimeoutEntry::Command { timeout_ms } => timeout_ms,
            super::StatementTimeoutEntry::Captured { deadline } => {
                self.restore_statement_deadline(deadline);
                return;
            }
            super::StatementTimeoutEntry::Nested => return,
        };
        if let Some(deadline) = self.suspended_statement_deadline() {
            self.restore_statement_deadline(deadline);
        } else if let Some(command) = &self.concurrent_index {
            self.restore_statement_deadline(command.deadline);
        } else {
            self.statement_timeout_consumed = false;
            super::query::arm_timeout(timeout_ms);
        }
    }

    pub(crate) fn restore_statement_deadline(&self, deadline: i64) {
        super::query::restore_statement_deadline(
            if self.statement_timeout_consumed || self.staged_commit.is_some() {
                0
            } else {
                deadline
            },
        );
    }

    pub(super) fn catch_query_cancellation(&mut self, now: i64) {
        self.statement_timeout_consumed |= super::query::consume_expired_timeout(now);
    }

    pub(super) fn suspended_statement_context(
        &self,
    ) -> Option<super::exec::PlpgsqlTransactionContext> {
        self.suspended_statement
            .as_ref()
            .map(|state| state.context)
            .or_else(|| self.cache_capacity_wait.as_ref().map(|wait| wait.context))
    }

    pub(super) fn suspend_statement(
        &mut self,
        lease: Option<crate::storage::StatementSnapshotLease>,
        clock: super::datetime::StatementClock,
        deadline: i64,
        context: super::exec::PlpgsqlTransactionContext,
    ) {
        if let Some(suspended) = &self.suspended_statement {
            assert!(lease.is_none(), "one retained statement snapshot");
            assert!(
                suspended.context == context,
                "one command invocation context"
            );
            return;
        }
        self.suspended_statement = Some(SuspendedStatement {
            lease,
            command: self.command_id,
            clock,
            deadline,
            context,
        });
    }

    pub(super) fn resume_statement(&mut self) -> Option<SuspendedStatement> {
        let suspended = self.suspended_statement.take()?;
        self.command_id = suspended.command;
        Some(suspended)
    }

    pub(crate) fn release_suspended_statement(&mut self) {
        self.suspended_statement = None;
        self.cache_capacity_wait = None;
        self.checkpoint_request = None;
    }

    pub fn begin_command(&mut self) -> Result<(), SqlError> {
        let command = self.next_command_identity()?;
        self.command_counter = command;
        self.command_id = command;
        Ok(())
    }

    #[cfg(test)]
    pub(super) fn test_set_command_id(&mut self, command: u32) {
        self.command_id = command;
        self.command_counter = command;
    }

    pub(crate) fn enter_trigger_sql<'txn>(
        &'txn mut self,
        storage: &Storage,
    ) -> Result<TriggerSqlScope<'txn>, SqlError> {
        if self.trigger_depth == MAX_TRIGGER_NESTING {
            return Err(sql_err!(
                sqlstate::PROGRAM_LIMIT_EXCEEDED,
                "trigger nesting exceeds {} levels",
                MAX_TRIGGER_NESTING
            ));
        }
        let command = self.next_command_identity()?;
        let snapshot = self.statement_snapshot(storage.statement_snapshot_lsn(self.txid)?);
        if matches!(
            self.isolation,
            TransactionIsolation::RepeatableRead | TransactionIsolation::Serializable
        ) {
            storage.register_snapshot(self.txid, snapshot)?;
        }
        let parent_command = self.command_id;
        let parent_depth = self.trigger_depth;
        self.command_counter = command;
        self.command_id = command;
        self.trigger_depth += 1;
        let snapshot = storage.nested_statement_snapshot(snapshot);
        Ok(TriggerSqlScope {
            transaction: self,
            parent_command,
            parent_depth,
            _snapshot: snapshot,
        })
    }

    /// Joins the nested command already running instead of starting one:
    /// the `WITH` queries of a PL/pgSQL statement and its write share one
    /// command and snapshot, so neither sees the other's changes.
    pub(crate) fn share_trigger_sql<'txn>(
        &'txn mut self,
        storage: &Storage,
    ) -> TriggerSqlScope<'txn> {
        let parent_command = self.command_id;
        let parent_depth = self.trigger_depth;
        TriggerSqlScope {
            transaction: self,
            parent_command,
            parent_depth,
            _snapshot: storage.same_statement_snapshot(),
        }
    }

    pub(crate) fn trigger_depth(&self) -> u16 {
        self.trigger_depth
    }

    pub(crate) fn has_pending_write_completion(&self) -> bool {
        self.pending_write_completion.is_some()
    }

    pub(crate) fn enter_extension_script(&mut self) {
        assert!(!self.extension_script, "extension scripts cannot nest");
        self.extension_script = true;
    }

    pub(crate) fn leave_extension_script(&mut self) {
        assert!(self.extension_script, "extension script scope is paired");
        self.extension_script = false;
    }

    pub(crate) fn in_extension_script(&self) -> bool {
        self.extension_script
    }

    pub(crate) fn enter_rule(&mut self, oid: i32, relation: &str) -> Result<(), SqlError> {
        let depth = usize::from(self.rule_depth);
        if self.rule_stack[..depth].contains(&oid) || depth == MAX_RULE_NESTING {
            return Err(sql_err!(
                sqlstate::INVALID_OBJECT_DEFINITION,
                "infinite recursion detected in rules for relation \"{}\"",
                relation
            ));
        }
        self.rule_stack[depth] = oid;
        self.rule_depth += 1;
        Ok(())
    }

    pub(crate) fn leave_rule(&mut self) {
        self.rule_depth = self
            .rule_depth
            .checked_sub(1)
            .expect("rewrite-rule nesting is paired");
    }

    pub fn set_characteristics(
        &mut self,
        isolation: TransactionIsolation,
        read_only: bool,
        deferrable: bool,
    ) {
        self.isolation = isolation;
        self.read_only = read_only;
        self.deferrable = deferrable;
        self.sources = TransactionSources::default();
    }

    pub(crate) fn apply_begin_characteristics(
        &mut self,
        characteristics: super::ast::TransactionCharacteristics,
    ) {
        if let Some(isolation) = characteristics.isolation {
            self.isolation = isolation;
            self.sources.isolation = true;
        }
        if let Some(read_only) = characteristics.read_only {
            self.read_only = read_only;
            self.sources.read_only = true;
        }
        if let Some(deferrable) = characteristics.deferrable {
            self.deferrable = deferrable;
            self.sources.deferrable = true;
        }
    }

    pub(crate) fn capture_configuration(&self) -> TransactionConfiguration {
        TransactionConfiguration {
            isolation: self.isolation,
            read_only: self.read_only,
            deferrable: self.deferrable,
            sources: self.sources,
        }
    }

    pub(crate) fn restore_configuration(&mut self, configuration: TransactionConfiguration) {
        self.isolation = configuration.isolation;
        self.read_only = configuration.read_only;
        self.deferrable = configuration.deferrable;
        self.sources = configuration.sources;
    }

    pub(crate) fn restore_read_only_source(&mut self, explicitly_set: bool) {
        self.sources.read_only = explicitly_set;
    }

    pub(crate) fn setting_source(&self, name: &str) -> Option<&'static str> {
        let explicitly_set = if name.eq_ignore_ascii_case("transaction_isolation") {
            self.sources.isolation
        } else if name.eq_ignore_ascii_case("transaction_read_only") {
            self.sources.read_only
        } else if name.eq_ignore_ascii_case("transaction_deferrable") {
            self.sources.deferrable
        } else {
            return None;
        };
        Some(if explicitly_set {
            "session"
        } else {
            "override"
        })
    }

    pub(crate) fn apply_characteristics(
        &mut self,
        characteristics: super::ast::TransactionCharacteristics,
    ) -> Result<(), SqlError> {
        if characteristics
            .isolation
            .is_some_and(|isolation| isolation != self.isolation)
            && self.has_savepoints()
        {
            return Err(sql_err!(
                sqlstate::ACTIVE_SQL_TRANSACTION,
                "SET TRANSACTION ISOLATION LEVEL must not be called in a subtransaction"
            ));
        }
        if characteristics.deferrable.is_some() && self.has_savepoints() {
            return Err(sql_err!(
                sqlstate::ACTIVE_SQL_TRANSACTION,
                "SET TRANSACTION [NOT] DEFERRABLE cannot be called within a subtransaction"
            ));
        }
        if characteristics
            .isolation
            .is_some_and(|isolation| isolation != self.isolation)
            && self.snapshot_taken
        {
            return Err(sql_err!(
                sqlstate::ACTIVE_SQL_TRANSACTION,
                "SET TRANSACTION ISOLATION LEVEL must be called before any query"
            ));
        }
        if characteristics.read_only == Some(false) && self.read_only && self.snapshot_taken {
            return Err(sql_err!(
                sqlstate::ACTIVE_SQL_TRANSACTION,
                "transaction read-write mode must be set before any query"
            ));
        }
        if characteristics.deferrable.is_some() && self.snapshot_taken {
            return Err(sql_err!(
                sqlstate::ACTIVE_SQL_TRANSACTION,
                "SET TRANSACTION [NOT] DEFERRABLE must be called before any query"
            ));
        }
        self.apply_begin_characteristics(characteristics);
        Ok(())
    }

    /// Selects this statement's durable commit snapshot. A repeatable-read
    /// transaction pins its first data statement; READ COMMITTED follows the
    /// latest committed LSN on every statement.
    pub fn statement_snapshot(&mut self, current_lsn: u64) -> u64 {
        self.snapshot_taken = true;
        match self.isolation {
            TransactionIsolation::ReadUncommitted | TransactionIsolation::ReadCommitted => {
                current_lsn
            }
            TransactionIsolation::RepeatableRead | TransactionIsolation::Serializable => {
                *self.snapshot_lsn.get_or_insert(current_lsn)
            }
        }
    }

    fn check_snapshot_import(&self, assigned: bool) -> Result<(), SqlError> {
        if !self.is_explicit() {
            return Err(sql_err!(
                sqlstate::NO_ACTIVE_SQL_TRANSACTION,
                "SET TRANSACTION SNAPSHOT can only be used in transaction blocks"
            ));
        }
        if self.snapshot_taken || assigned || self.has_savepoints() {
            return Err(sql_err!(
                sqlstate::ACTIVE_SQL_TRANSACTION,
                "SET TRANSACTION SNAPSHOT must be called before any query"
            ));
        }
        if matches!(
            self.isolation,
            TransactionIsolation::ReadUncommitted | TransactionIsolation::ReadCommitted
        ) {
            return Err(sql_err!(
                sqlstate::FEATURE_NOT_SUPPORTED,
                "a snapshot-importing transaction must have isolation level SERIALIZABLE or REPEATABLE READ"
            ));
        }
        Ok(())
    }

    pub(crate) fn prepare_snapshot_import(
        &mut self,
        assigned: bool,
    ) -> Result<SnapshotImport<'_>, SqlError> {
        self.check_snapshot_import(assigned)?;
        Ok(SnapshotImport { transaction: self })
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

    pub(crate) fn record_truncate(
        &mut self,
        storage: &Storage,
        tables: &[usize],
        cascade: bool,
        restart_identity: bool,
        command_id: u32,
    ) -> Result<(), SqlError> {
        if self.truncates.len() == self.truncates.capacity()
            || tables.len() > self.truncate_table_capacity()
        {
            return Err(sql_err!(
                sqlstate::PROGRAM_LIMIT_EXCEEDED,
                "transaction TRUNCATE capacity exceeded"
            ));
        }
        let event = TruncateEvent {
            scope: storage.runtime_input_scope(),
            targets: storage.capture_truncate_targets(tables, self.txid)?,
            command_id,
            cascade,
            restart_identity,
        };
        if let Some(owner) = &self.plan_invalidations {
            for target in event.targets.iter() {
                owner.apply(event.plan_event(target.created_at));
            }
        }
        self.truncates
            .push(event)
            .expect("TRUNCATE event capacity checked before mutation");
        Ok(())
    }

    pub(crate) fn truncates(&self) -> &[TruncateEvent] {
        &self.truncates
    }

    pub(crate) fn truncate_table_capacity(&self) -> usize {
        self.truncate_wal_tables.capacity() / 128
    }

    pub(crate) fn constraint_capacity(&self) -> usize {
        self.deferred_constraints.capacity()
    }

    pub(crate) fn truncate_wal_tables<'a>(
        &'a mut self,
        event: &'a TruncateEvent,
    ) -> (&'a [crate::storage::TruncateTarget], &'a mut FixedBuf) {
        (&event.targets, &mut self.truncate_wal_tables)
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

    pub(crate) fn has_session_notification_actions(&self) -> bool {
        !self.pending_notifies.is_empty() || !self.pending_listen_ops.is_empty()
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

    pub(crate) fn constraint_mode(
        &self,
        constraint: ConstraintIdentity,
        timing: crate::storage::ConstraintTiming,
    ) -> crate::sql::ast::ConstraintMode {
        if !timing.is_deferrable() {
            return crate::sql::ast::ConstraintMode::Immediate;
        }
        let constraint = self.current_constraint_identity(constraint);
        for change in self.constraint_modes.iter().rev() {
            if change.constraint.is_none()
                || change
                    .constraint
                    .map(|identity| self.current_constraint_identity(identity))
                    == Some(constraint)
            {
                return change.mode;
            }
        }
        if timing.initially_deferred() {
            crate::sql::ast::ConstraintMode::Deferred
        } else {
            crate::sql::ast::ConstraintMode::Immediate
        }
    }

    pub(crate) fn record_constraint_mode(
        &mut self,
        constraint: Option<ConstraintIdentity>,
        mode: crate::sql::ast::ConstraintMode,
    ) -> Result<(), SqlError> {
        self.constraint_modes
            .push(ConstraintModeChange { constraint, mode })
            .map_err(|_| {
                sql_err!(
                    sqlstate::PROGRAM_LIMIT_EXCEEDED,
                    "transaction changes constraint modes more than {} times",
                    self.constraint_capacity()
                )
            })
    }

    pub(crate) fn can_record_constraint_modes(&self, additional: usize) -> bool {
        self.constraint_modes
            .len()
            .checked_add(additional)
            .is_some_and(|needed| needed <= self.constraint_modes.capacity())
    }

    pub(crate) fn defer_constraint(
        &mut self,
        constraint: ConstraintIdentity,
        rowid: u64,
    ) -> Result<(), SqlError> {
        let obligation = ConstraintObligation { constraint, rowid };
        if self
            .deferred_constraints
            .iter()
            .enumerate()
            .any(|(index, item)| {
                *item == obligation && !self.completed_constraints.contains(&index)
            })
        {
            return Ok(());
        }
        self.deferred_constraints.push(obligation).map_err(|_| {
            sql_err!(
                sqlstate::PROGRAM_LIMIT_EXCEEDED,
                "transaction defers more than {} constraints",
                self.constraint_capacity()
            )
        })
    }

    pub(crate) fn deferred_constraints(&self) -> &[ConstraintObligation] {
        &self.deferred_constraints
    }

    pub(crate) fn deferred_constraint_is_complete(&self, index: usize) -> bool {
        self.completed_constraints.contains(&index)
    }

    pub(crate) fn deferred_constraint_for(
        &self,
        current: ConstraintIdentity,
    ) -> Option<(usize, ConstraintObligation)> {
        self.deferred_constraints
            .iter()
            .enumerate()
            .filter(|(index, _)| !self.completed_constraints.contains(index))
            .map(|(index, obligation)| (index, *obligation))
            .find(|(_, obligation)| {
                self.current_constraint_identity(obligation.constraint) == current
            })
    }

    pub(crate) fn complete_deferred_constraint(&mut self, index: usize) -> Result<(), SqlError> {
        if self.completed_constraints.contains(&index) {
            return Ok(());
        }
        self.completed_constraints.push(index).map_err(|_| {
            sql_err!(
                sqlstate::PROGRAM_LIMIT_EXCEEDED,
                "transaction completes more than {} deferred constraints",
                self.constraint_capacity()
            )
        })
    }

    pub(crate) fn rename_constraint(
        &mut self,
        table: u32,
        from: crate::storage::SqlName,
        to: crate::storage::SqlName,
    ) -> Result<(), SqlError> {
        let identity = self.constraint_identity(table, from);
        self.constraint_renames
            .push(ConstraintLifecycle::Rename { identity, to })
            .map_err(|_| {
                sql_err!(
                    sqlstate::PROGRAM_LIMIT_EXCEEDED,
                    "transaction renames more than {} constraints",
                    self.constraint_capacity()
                )
            })
    }

    pub(crate) fn drop_constraint(
        &mut self,
        table: u32,
        name: crate::storage::SqlName,
    ) -> Result<(), SqlError> {
        let identity = self.constraint_identity(table, name);
        let ConstraintIdentity::Table { generation, .. } = identity else {
            unreachable!("table constraint lookup returns a table identity")
        };
        let next_generation = generation.checked_add(1).ok_or_else(|| {
            sql_err!(
                sqlstate::PROGRAM_LIMIT_EXCEEDED,
                "constraint generation is exhausted"
            )
        })?;
        self.constraint_renames
            .push(ConstraintLifecycle::Drop {
                identity,
                next_generation,
            })
            .map_err(|_| {
                sql_err!(
                    sqlstate::PROGRAM_LIMIT_EXCEEDED,
                    "transaction changes more than {} constraint identities",
                    self.constraint_capacity()
                )
            })
    }

    pub(crate) fn constraint_identity(
        &self,
        table: u32,
        name: crate::storage::SqlName,
    ) -> ConstraintIdentity {
        for event in self.constraint_renames.iter().rev() {
            match *event {
                ConstraintLifecycle::Drop {
                    identity:
                        ConstraintIdentity::Table {
                            table: identity_table,
                            name: identity_name,
                            ..
                        },
                    next_generation,
                } if identity_table == table && identity_name == name => {
                    return ConstraintIdentity::Table {
                        table,
                        name,
                        generation: next_generation,
                    };
                }
                ConstraintLifecycle::Rename {
                    identity:
                        identity @ ConstraintIdentity::Table {
                            table: identity_table,
                            ..
                        },
                    to,
                } if identity_table == table && to == name => {
                    let ConstraintIdentity::Table { generation, .. } = identity else {
                        unreachable!("constraint lifecycle contains table identities")
                    };
                    return ConstraintIdentity::Table {
                        table,
                        name,
                        generation,
                    };
                }
                _ => {}
            }
        }
        ConstraintIdentity::Table {
            table,
            name,
            generation: 0,
        }
    }

    pub(crate) fn catalog_constraint_identity(
        &self,
        identity: ConstraintIdentity,
    ) -> ConstraintIdentity {
        match identity {
            ConstraintIdentity::Table { table, name, .. } => self.constraint_identity(table, name),
            ConstraintIdentity::Trigger { .. } => identity,
        }
    }

    pub(crate) fn current_constraint_identity(
        &self,
        mut identity: ConstraintIdentity,
    ) -> ConstraintIdentity {
        if !matches!(identity, ConstraintIdentity::Table { .. }) {
            return identity;
        }
        for event in self.constraint_renames.iter() {
            if let ConstraintLifecycle::Rename {
                identity: renamed,
                to,
            } = *event
                && renamed == identity
            {
                let ConstraintIdentity::Table {
                    table, generation, ..
                } = identity
                else {
                    unreachable!("constraint lifecycle contains table identities")
                };
                identity = ConstraintIdentity::Table {
                    table,
                    name: to,
                    generation,
                };
            }
        }
        identity
    }

    pub(crate) fn dropped_constraint_names(
        &self,
        table: u32,
    ) -> impl Iterator<Item = crate::storage::SqlName> + '_ {
        self.constraint_renames
            .iter()
            .filter_map(move |event| match *event {
                ConstraintLifecycle::Drop {
                    identity:
                        ConstraintIdentity::Table {
                            table: dropped_table,
                            name,
                            ..
                        },
                    ..
                } if dropped_table == table => Some(name),
                _ => None,
            })
    }

    pub(crate) fn constraint_identity_is_current(&self, identity: ConstraintIdentity) -> bool {
        let current = self.current_constraint_identity(identity);
        match current {
            ConstraintIdentity::Table { table, name, .. } => {
                self.constraint_identity(table, name) == current
            }
            ConstraintIdentity::Trigger { .. } => true,
        }
    }

    #[expect(
        clippy::too_many_arguments,
        reason = "rewind restores every independently bounded constraint queue"
    )]
    pub(crate) fn rewind_constraints(
        &mut self,
        obligation_mark: usize,
        completion_mark: usize,
        mode_mark: usize,
        rename_mark: usize,
        trigger_mark: usize,
        trigger_completion_mark: usize,
        trigger_bytes_mark: usize,
    ) {
        while self.deferred_constraints.len() > obligation_mark {
            self.deferred_constraints.pop();
        }
        while self.constraint_modes.len() > mode_mark {
            self.constraint_modes.pop();
        }
        while self.completed_constraints.len() > completion_mark {
            self.completed_constraints.pop();
        }
        while self.constraint_renames.len() > rename_mark {
            self.constraint_renames.pop();
        }
        while self.deferred_triggers.len() > trigger_mark {
            self.deferred_triggers.pop();
        }
        while self.completed_deferred_triggers.len() > trigger_completion_mark {
            self.completed_deferred_triggers.pop();
        }
        self.deferred_trigger_bytes.truncate_to(trigger_bytes_mark);
    }

    pub(crate) fn queue_constraint_trigger(
        &mut self,
        identity: ConstraintIdentity,
        effective_table: u16,
        event: u8,
        updated_columns: ColumnSet,
        old: Option<&[u8]>,
        new: Option<&[u8]>,
    ) -> Result<(), SqlError> {
        self.queue_trigger_event(
            DeferredTriggerKind::Constraint {
                identity,
                effective_table,
                trigger_depth: self.trigger_depth,
            },
            event,
            updated_columns,
            old,
            new,
        )
    }

    pub(crate) fn queue_after_row_trigger(
        &mut self,
        trigger_slot: u16,
        effective_table: u16,
        event: u8,
        updated_columns: ColumnSet,
        old: Option<&[u8]>,
        new: Option<&[u8]>,
    ) -> Result<(), SqlError> {
        self.queue_trigger_event(
            DeferredTriggerKind::AfterRow {
                trigger_slot,
                effective_table,
                trigger_depth: self.trigger_depth,
            },
            event,
            updated_columns,
            old,
            new,
        )
    }

    fn queue_trigger_event(
        &mut self,
        kind: DeferredTriggerKind,
        event: u8,
        updated_columns: ColumnSet,
        old: Option<&[u8]>,
        new: Option<&[u8]>,
    ) -> Result<(), SqlError> {
        let bytes_mark = self.deferred_trigger_bytes.mark();
        let mut append = |bytes: Option<&[u8]>| -> Result<Option<DeferredTriggerTuple>, SqlError> {
            let Some(bytes) = bytes else { return Ok(None) };
            let offset = self.deferred_trigger_bytes.mark();
            if !self.deferred_trigger_bytes.append(bytes) {
                return Err(sql_err!(
                    sqlstate::PROGRAM_LIMIT_EXCEEDED,
                    "deferred trigger row images exceed {} bytes",
                    self.deferred_trigger_bytes.capacity()
                ));
            }
            Ok(Some(DeferredTriggerTuple {
                offset: offset as u32,
                length: bytes.len() as u32,
            }))
        };
        let old = append(old)?;
        let new = match append(new) {
            Ok(new) => new,
            Err(error) => {
                self.deferred_trigger_bytes.truncate_to(bytes_mark);
                return Err(error);
            }
        };
        if self
            .deferred_triggers
            .push(DeferredTriggerEvent {
                kind,
                event,
                updated_columns,
                old,
                new,
            })
            .is_err()
        {
            self.deferred_trigger_bytes.truncate_to(bytes_mark);
            return Err(sql_err!(
                sqlstate::PROGRAM_LIMIT_EXCEEDED,
                "transaction queues more than {} constraint trigger firings",
                self.constraint_capacity()
            ));
        }
        Ok(())
    }

    pub(crate) fn deferred_trigger_events(&self) -> &[DeferredTriggerEvent] {
        &self.deferred_triggers
    }

    pub(crate) fn has_deferred_triggers(&self) -> bool {
        self.deferred_triggers
            .iter()
            .enumerate()
            .any(|(index, _)| !self.completed_deferred_triggers.contains(&index))
    }

    pub(crate) fn deferred_trigger_is_complete(&self, index: usize) -> bool {
        self.completed_deferred_triggers.contains(&index)
    }

    pub(crate) fn deferred_trigger_bytes(&self, tuple: DeferredTriggerTuple) -> &[u8] {
        &self.deferred_trigger_bytes.readable()
            [tuple.offset as usize..tuple.offset as usize + tuple.length as usize]
    }

    pub(crate) fn complete_deferred_trigger(&mut self, index: usize) -> Result<(), SqlError> {
        if self.completed_deferred_triggers.contains(&index) {
            return Ok(());
        }
        self.completed_deferred_triggers.push(index).map_err(|_| {
            sql_err!(
                sqlstate::PROGRAM_LIMIT_EXCEEDED,
                "transaction completes more than {} deferred trigger firings",
                self.constraint_capacity()
            )
        })
    }

    /// Reclaims completed queue entries only when no SQL savepoint can make
    /// them live again. Completion markers otherwise form the undo state for
    /// `ROLLBACK TO SAVEPOINT`.
    pub(crate) fn compact_completed_constraints(&mut self) {
        if !self.savepoints.is_empty() {
            return;
        }

        let mut write = 0usize;
        for read in 0..self.deferred_constraints.len() {
            if self.completed_constraints.contains(&read) {
                continue;
            }
            self.deferred_constraints[write] = self.deferred_constraints[read];
            write += 1;
        }
        while self.deferred_constraints.len() > write {
            self.deferred_constraints.pop();
        }
        self.completed_constraints.clear();

        let mut event_write = 0usize;
        let mut byte_write = 0usize;
        for read in 0..self.deferred_triggers.len() {
            if self.completed_deferred_triggers.contains(&read) {
                continue;
            }
            let mut event = self.deferred_triggers[read];
            for tuple in [&mut event.old, &mut event.new].into_iter().flatten() {
                let source = tuple.offset as usize;
                let length = tuple.length as usize;
                self.deferred_trigger_bytes
                    .filled_mut()
                    .copy_within(source..source + length, byte_write);
                tuple.offset = byte_write as u32;
                byte_write += length;
            }
            self.deferred_triggers[event_write] = event;
            event_write += 1;
        }
        while self.deferred_triggers.len() > event_write {
            self.deferred_triggers.pop();
        }
        self.deferred_trigger_bytes.truncate_to(byte_write);
        self.completed_deferred_triggers.clear();
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
    pub(crate) fn savepoint(
        &mut self,
        name: &str,
        wal_mark: usize,
        lock_mark: u64,
        cursor_mark: Option<super::cursor::CursorCreationMark>,
    ) -> Result<(), SqlError> {
        let sp = Savepoint {
            name: {
                let mut s = StackStr::new();
                let _ = core::fmt::Write::write_str(&mut s, name);
                s
            },
            read_only: self.read_only,
            read_only_source: self.sources.read_only,
            touched_mark: self.touched.len(),
            truncate_mark: self.truncates.len(),
            ddl_mark: self.ddl.len(),
            statistics_mark: self.statistics_undo.len(),
            wal_mark,
            lock_mark,
            cursor_mark,
            notify_mark: self.pending_notifies.len(),
            notify_payload_mark: self.notify_payloads.mark(),
            listen_mark: self.pending_listen_ops.len(),
            subscription_advance_mark: self.subscription_advances.len(),
            constraint_obligation_mark: self.deferred_constraints.len(),
            constraint_completion_mark: self.completed_constraints.len(),
            constraint_mode_mark: self.constraint_modes.len(),
            constraint_rename_mark: self.constraint_renames.len(),
            deferred_trigger_mark: self.deferred_triggers.len(),
            deferred_trigger_completion_mark: self.completed_deferred_triggers.len(),
            deferred_trigger_bytes_mark: self.deferred_trigger_bytes.mark(),
            failed: self.failed,
        };
        self.savepoints.push(sp).map_err(|_| {
            sql_err!(
                sqlstate::PROGRAM_LIMIT_EXCEEDED,
                "more than {} active savepoints",
                self.savepoints.capacity()
            )
        })
    }

    pub(crate) fn statement_mark(&self, wal: usize, lock: u64) -> StatementMark {
        StatementMark {
            txid: self.txid,
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
            constraint_obligations: self.deferred_constraints.len(),
            constraint_completions: self.completed_constraints.len(),
            constraint_modes: self.constraint_modes.len(),
            constraint_renames: self.constraint_renames.len(),
            deferred_triggers: self.deferred_triggers.len(),
            deferred_trigger_completions: self.completed_deferred_triggers.len(),
            deferred_trigger_bytes: self.deferred_trigger_bytes.mark(),
        }
    }

    pub(crate) fn transaction_effect_mark(&self) -> TransactionEffectMark {
        TransactionEffectMark([
            self.touched.len(),
            self.truncates.len(),
            self.ddl.len(),
            self.statistics_undo.len(),
            self.pending_notifies.len(),
            self.notify_payloads.mark(),
            self.pending_listen_ops.len(),
            self.subscription_advances.len(),
            self.deferred_constraints.len(),
            self.completed_constraints.len(),
            self.constraint_modes.len(),
            self.constraint_renames.len(),
            self.deferred_triggers.len(),
            self.completed_deferred_triggers.len(),
            self.deferred_trigger_bytes.mark(),
        ])
    }

    pub(crate) fn owns_statement_mark(&self, mark: StatementMark) -> bool {
        self.is_active() && self.txid == mark.txid
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
        for descriptor in self.large_object_descriptors.iter_mut() {
            if descriptor.active && descriptor.opened_savepoint_depth > index {
                descriptor.opened_savepoint_depth = index;
            }
        }
        while self.savepoints.len() > index {
            self.savepoints.pop();
        }
    }

    /// Drops savepoints nested strictly inside `index`, keeping `index` itself (for
    /// `ROLLBACK TO SAVEPOINT`, which leaves the target reusable).
    pub fn rollback_savepoints_after(&mut self, index: usize) {
        self.rollback_large_object_descriptors_to(index + 1);
        while self.savepoints.len() > index + 1 {
            self.savepoints.pop();
        }
    }

    pub(crate) fn rewind_to_savepoint(&mut self, index: usize, boundary: &Savepoint) {
        self.rewind_touched(boundary.touched_mark);
        self.rewind_truncates(boundary.truncate_mark);
        self.rewind_ddl(boundary.ddl_mark);
        self.rewind_statistics(boundary.statistics_mark);
        self.rewind_subscription_advances(boundary.subscription_advance_mark);
        self.rewind_constraints(
            boundary.constraint_obligation_mark,
            boundary.constraint_completion_mark,
            boundary.constraint_mode_mark,
            boundary.constraint_rename_mark,
            boundary.deferred_trigger_mark,
            boundary.deferred_trigger_completion_mark,
            boundary.deferred_trigger_bytes_mark,
        );
        self.rewind_notifications(
            boundary.notify_mark,
            boundary.notify_payload_mark,
            boundary.listen_mark,
        );
        self.rollback_savepoints_after(index);
        self.read_only = boundary.read_only;
        self.restore_read_only_source(boundary.read_only_source);
        self.failed = boundary.failed;
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
            let event = self.truncates.pop().expect("nonempty TRUNCATE event list");
            if let Some(owner) = &self.plan_invalidations {
                for target in event.targets.iter() {
                    owner.apply(event.plan_event(target.created_at));
                }
            }
        }
    }

    pub fn rewind_ddl(&mut self, ddl_mark: usize) {
        while self.ddl.len() > ddl_mark {
            self.ddl.pop().expect("DDL exceeds its rewind mark");
            let event = self
                .ddl_plan_events
                .pop()
                .expect("DDL events parallel their changes");
            if let Some(owner) = &self.plan_invalidations {
                owner.apply(event);
            }
            self.ddl_origins.pop();
        }
    }

    pub(crate) fn record_statistics(&mut self, table: u32) -> Result<(), SqlError> {
        self.statistics_undo
            .push(StatisticsUndo::Table { table, event: None })
            .map_err(|_| {
                sql_err!(
                    crate::sql::eval::sqlstate::PROGRAM_LIMIT_EXCEEDED,
                    "more than {} ANALYZE targets in one transaction",
                    self.statistics_undo.capacity()
                )
            })
    }

    pub(crate) fn record_statistics_plan_change(
        &mut self,
        storage: &crate::storage::Storage,
        table: u32,
    ) {
        let Some(StatisticsUndo::Table {
            table: selected,
            event,
        }) = self.statistics_undo.last_mut()
        else {
            panic!("ANALYZE retained its statistics receipt before publication")
        };
        assert_eq!(*selected, table, "ANALYZE publishes its admitted relation");
        assert!(
            event.is_none(),
            "ANALYZE publishes each relation estimate once"
        );
        let captured = super::plan_invalidations::PlanInvalidationEvent {
            scope: storage.runtime_input_scope(),
            change: super::plan_invalidations::CatalogPlanChange {
                global: false,
                roles: false,
            },
            selected: Some(crate::storage::SelectedPlanTarget::Relation {
                created_at: storage
                    .table_def(table as usize, self.txid)
                    .identity()
                    .created_at,
            }),
        };
        *event = Some(captured);
        // PostgreSQL heap_inplace_update_and_unlock sends an immediate shared
        // invalidation and retains another receipt for commit or local abort.
        storage.publish_plan_invalidations(core::iter::once(captured));
    }

    pub(crate) fn record_extended_statistics(&mut self, slot: u32) -> Result<(), SqlError> {
        self.statistics_undo
            .push(StatisticsUndo::Extended(slot))
            .map_err(|_| {
                sql_err!(
                    crate::sql::eval::sqlstate::PROGRAM_LIMIT_EXCEEDED,
                    "more than {} ANALYZE targets in one transaction",
                    self.statistics_undo.capacity()
                )
            })
    }

    pub(crate) fn statistics_undo(&self) -> &[StatisticsUndo] {
        &self.statistics_undo
    }

    pub(crate) fn rewind_statistics(&mut self, mark: usize) {
        while self.statistics_undo.len() > mark {
            if let Some(StatisticsUndo::Table {
                event: Some(event), ..
            }) = self.statistics_undo.pop()
                && let Some(owner) = &self.plan_invalidations
            {
                owner.apply(event);
            }
        }
    }

    pub(crate) fn rewind_subscription_advances(&mut self, mark: usize) {
        while self.subscription_advances.len() > mark {
            self.subscription_advances.pop();
        }
    }

    pub(crate) fn write_table_definition(
        &mut self,
        storage: &crate::storage::Storage,
        target: crate::storage::TableDefinitionTarget,
        definition: crate::storage::TableDefBuilder,
        column_mapping: &[Option<crate::storage::SqlName>; crate::storage::MAX_COLUMNS],
        rewrites_rows: bool,
        invalidated_statistics: crate::storage::ColumnSet,
    ) -> Result<crate::storage::TableDefinitionTarget, SqlError> {
        if target.transaction() != self.txid {
            return Err(sql_err!(
                crate::sql::eval::sqlstate::SERIALIZATION_FAILURE,
                "table definition selection belongs to another transaction"
            ));
        }
        let change = storage.write_table_def(
            target,
            definition,
            column_mapping,
            rewrites_rows,
            invalidated_statistics,
        )?;
        if let Err(error) = self.record_ddl(storage, DdlUndo::TableAltered(change)) {
            storage.rollback_table_def(change);
            return Err(error);
        }
        Ok(change.row_target())
    }

    pub(crate) fn table_definition_changes(
        &self,
    ) -> impl Iterator<Item = crate::storage::TableDefinitionChange> + Clone {
        self.ddl().iter().filter_map(|undo| match undo {
            DdlUndo::TableAltered(change) => Some(*change),
            _ => None,
        })
    }

    pub(crate) fn type_commit_changes(
        &self,
    ) -> impl Iterator<Item = crate::storage::TypeCommitChange> + Clone {
        self.ddl().iter().filter_map(|undo| {
            use crate::storage::TypeCommitChange;
            Some(match undo {
                DdlUndo::DomainCreated(undo)
                | DdlUndo::DomainDropped(undo)
                | DdlUndo::EnumCreated(undo)
                | DdlUndo::EnumDropped(undo)
                | DdlUndo::CompositeCreated(undo)
                | DdlUndo::CompositeDropped(undo) => TypeCommitChange::Lifecycle(*undo),
                DdlUndo::DomainAltered(undo) => TypeCommitChange::Definition(undo.target()),
                DdlUndo::EnumAltered(undo) => TypeCommitChange::Definition(undo.target()),
                DdlUndo::CompositeAltered(undo) => TypeCommitChange::Definition(undo.target()),
                _ => return None,
            })
        })
    }

    pub(crate) fn index_generation_changes(
        &self,
    ) -> impl Iterator<Item = crate::storage::IndexGenerationChange> + Clone {
        self.ddl().iter().filter_map(|undo| {
            use crate::storage::IndexGenerationChange;
            Some(match undo {
                DdlUndo::IndexCreated(slot) => IndexGenerationChange::Created(*slot as usize),
                DdlUndo::IndexDropped(slot) => IndexGenerationChange::Dropped(*slot as usize),
                DdlUndo::IndexAltered { slot, .. } => IndexGenerationChange::State(*slot as usize),
                DdlUndo::IndexRenamed { slot, .. } => {
                    IndexGenerationChange::Renamed(*slot as usize)
                }
                _ => return None,
            })
        })
    }

    pub(crate) fn write_row(
        &mut self,
        storage: &crate::storage::Storage,
        target: crate::storage::TableDefinitionTarget,
        rowid: u64,
        command: u32,
        image: Option<crate::storage::HeapRowRead>,
        statistics: crate::storage::RowWriteStatistics,
    ) -> Result<crate::storage::PendingWriteUndo, SqlError> {
        if target.transaction() != self.txid {
            return Err(sql_err!(
                crate::sql::eval::sqlstate::SERIALIZATION_FAILURE,
                "row definition selection belongs to another transaction"
            ));
        }
        let undo = storage.write_pending_image(target, rowid, command, image, statistics)?;
        if let Err(error) = self.touch(target.table() as u32, rowid, undo) {
            storage.restore_pending(target.table(), rowid, self.txid, undo);
            return Err(error);
        }
        Ok(undo)
    }

    pub(crate) fn record_table_drop(
        &mut self,
        storage: &crate::storage::Storage,
        undo: crate::storage::CatalogDdlUndo,
    ) -> Result<(), SqlError> {
        storage.validate_table_drop(undo, self.txid)?;
        if let Err(error) = self.record_ddl(storage, DdlUndo::Dropped(undo)) {
            storage.finish_table_drop(undo, crate::storage::CatalogDdlDecision::Rollback);
            return Err(error);
        }
        Ok(())
    }

    pub(crate) fn record_matview_change(
        &mut self,
        storage: &crate::storage::Storage,
        change: crate::storage::MatviewDdlUndo,
    ) -> Result<(), SqlError> {
        storage.validate_matview_change(change, self.txid)?;
        if let Err(error) = self.record_ddl(storage, DdlUndo::MatviewChanged(change)) {
            storage.finish_matview_change(change, crate::storage::CatalogDdlDecision::Rollback);
            return Err(error);
        }
        Ok(())
    }

    pub(crate) fn record_routine_change(
        &mut self,
        storage: &crate::storage::Storage,
        change: crate::storage::RoutineChange,
    ) -> Result<(), SqlError> {
        storage.validate_routine_change(change, self.txid)?;
        if let Err(error) = self.record_ddl(storage, DdlUndo::RoutineChanged(change)) {
            storage.finish_routine_change(change, crate::storage::CatalogDdlDecision::Rollback);
            return Err(error);
        }
        Ok(())
    }

    pub(crate) fn record_sequence_change(
        &mut self,
        storage: &crate::storage::Storage,
        change: crate::storage::SequenceChange,
    ) -> Result<(), SqlError> {
        storage.validate_sequence_change(change, self.txid)?;
        if let Err(error) = self.record_ddl(storage, DdlUndo::SequenceChanged(change)) {
            storage.finish_sequence_change(change, crate::storage::CatalogDdlDecision::Rollback);
            return Err(error);
        }
        Ok(())
    }

    pub(crate) fn record_catalog_change(
        &mut self,
        storage: &mut crate::storage::Storage,
        undo: DdlUndo,
    ) -> Result<(), SqlError> {
        if let Err(error) = self.record_ddl(storage, undo.clone()) {
            super::rollback_catalog_change(storage, undo, self.txid);
            return Err(error);
        }
        Ok(())
    }

    pub(crate) fn record_ddl(&mut self, storage: &Storage, undo: DdlUndo) -> Result<(), SqlError> {
        if self.ddl.len() == self.ddl.capacity()
            || self.ddl_plan_events.len() == self.ddl_plan_events.capacity()
        {
            return Err(sql_err!(
                crate::sql::eval::sqlstate::PROGRAM_LIMIT_EXCEEDED,
                "transaction contains more than {} catalog changes",
                self.ddl.capacity()
            ));
        }
        let event =
            super::plan_invalidations::PlanInvalidationEvent::capture(storage, &undo, self.txid)?;
        self.ddl
            .push(undo)
            .expect("DDL capacity was checked before capturing its event");
        self.ddl_origins
            .push(self.ddl_origin)
            .expect("parallel DDL origin pool has the same capacity");
        self.ddl_plan_events
            .push(event)
            .expect("parallel DDL event pool has the same capacity");
        if let Some(owner) = &self.plan_invalidations {
            owner.apply(event);
        }
        Ok(())
    }

    pub(crate) fn bind_plan_invalidations(
        &mut self,
        owner: &std::sync::Arc<super::plan_invalidations::LocalPlanInvalidations>,
    ) -> Result<(), SqlError> {
        if let Some(current) = &self.plan_invalidations {
            if std::sync::Arc::ptr_eq(current, owner) {
                return Ok(());
            }
            if self.mode != TxnMode::Idle {
                return Err(sql_err!(
                    sqlstate::OBJECT_IN_USE,
                    "active transaction cannot change its SQL plan owner"
                ));
            }
        }
        self.plan_invalidations = Some(std::sync::Arc::clone(owner));
        Ok(())
    }

    #[cfg(test)]
    pub(crate) fn plan_invalidation_owner(
        &self,
    ) -> Option<&std::sync::Arc<super::plan_invalidations::LocalPlanInvalidations>> {
        self.plan_invalidations.as_ref()
    }

    #[cfg(test)]
    pub(crate) fn plan_invalidation_stamp(
        &self,
        depends_on_roles: bool,
    ) -> Result<super::plan_invalidations::LocalPlanStamp, SqlError> {
        self.plan_invalidations
            .as_ref()
            .ok_or_else(|| {
                sql_err!(
                    sqlstate::INTERNAL_ERROR,
                    "transaction has no SQL plan invalidation owner"
                )
            })?
            .stamp(depends_on_roles)
    }

    pub(crate) fn watch_plan_inputs(
        &self,
        storage: &Storage,
        depends_on_roles: bool,
        dependencies: crate::mem::lease_pool::SharedSlice<crate::storage::RuntimeInputDependency>,
        own_routine: Option<i32>,
    ) -> Result<super::plan_invalidations::LocalPlanStamp, SqlError> {
        let owner = self.plan_invalidations.as_ref().ok_or_else(|| {
            sql_err!(
                sqlstate::INTERNAL_ERROR,
                "transaction has no SQL plan invalidation owner"
            )
        })?;
        storage.register_plan_backend(owner)?;
        owner.watch(
            depends_on_roles,
            storage.runtime_input_scope(),
            dependencies,
            own_routine,
        )
    }

    pub(crate) fn invalidate_prepared_plan_inputs(&self) {
        if let Some(owner) = &self.plan_invalidations {
            for event in self.plan_events() {
                owner.apply(event);
            }
        }
    }

    /// Prepared transaction data has no backend. Moving it cannot exchange
    /// the frontend's plan ownership with a reused detached slot.
    pub(crate) fn swap_prepared_state(&mut self, detached: &mut Self) {
        assert!(detached.plan_invalidations.is_none());
        core::mem::swap(self, detached);
        core::mem::swap(
            &mut self.plan_invalidations,
            &mut detached.plan_invalidations,
        );
    }

    /// Prepared changes never belonged to the resolving backend's local plans.
    /// Their commit publishes shared invalidations; rollback publishes none.
    pub(crate) fn with_detached_plan_owner<R>(
        &mut self,
        resolve: impl FnOnce(&mut Self) -> R,
    ) -> R {
        struct RestoreOwner<'a> {
            transaction: &'a mut TxnState,
            owner: Option<std::sync::Arc<super::plan_invalidations::LocalPlanInvalidations>>,
        }
        impl Drop for RestoreOwner<'_> {
            fn drop(&mut self) {
                self.transaction.plan_invalidations = self.owner.take();
            }
        }
        let owner = self.plan_invalidations.take();
        let restore = RestoreOwner {
            transaction: self,
            owner,
        };
        let result = resolve(&mut *restore.transaction);
        assert!(restore.transaction.plan_invalidations.is_none());
        result
    }

    /// A queued commit and its waiting frontend share the same backend until
    /// the frontend collects its durable result.
    pub(crate) fn stage_commit_state(&mut self, worker: &mut Self) {
        let owner = self.plan_invalidations.clone();
        core::mem::swap(self, worker);
        self.plan_invalidations = owner;
    }

    pub(crate) fn finish_commit_state(&mut self, worker: &mut Self) {
        core::mem::swap(self, worker);
        worker.clear();
        worker.plan_invalidations = None;
    }

    pub(crate) fn ddl(&self) -> &[DdlUndo] {
        &self.ddl
    }

    pub(crate) fn plan_events(
        &self,
    ) -> impl Iterator<Item = super::plan_invalidations::PlanInvalidationEvent> + '_ {
        self.ddl_plan_events
            .iter()
            .copied()
            .chain(self.truncates.iter().flat_map(|event| {
                event
                    .targets
                    .iter()
                    .map(move |target| event.plan_event(target.created_at))
            }))
            .chain(self.statistics_undo.iter().filter_map(|undo| match undo {
                StatisticsUndo::Table { event, .. } => *event,
                StatisticsUndo::Extended(_) => None,
            }))
    }

    pub(crate) fn ddl_capacity(&self) -> usize {
        self.ddl.capacity()
    }

    pub(crate) fn ddl_origins(&self) -> &[u32] {
        &self.ddl_origins
    }

    pub(crate) fn enter_ddl_origin(&mut self) -> u32 {
        let prior = self.ddl_origin;
        self.next_ddl_origin = self
            .next_ddl_origin
            .checked_add(1)
            .expect("bounded statement nesting cannot exhaust DDL origins");
        self.ddl_origin = self.next_ddl_origin;
        prior
    }

    pub(crate) fn leave_ddl_origin(&mut self, prior: u32) {
        self.ddl_origin = prior;
    }

    pub(crate) fn ddl_origin(&self) -> u32 {
        self.ddl_origin
    }

    pub fn clear(&mut self) {
        self.pending_write_completion = None;
        self.write_completion_boundary = None;
        self.concurrent_index = None;
        self.immediate_index_utility = None;
        if let Some(owner) = &self.plan_invalidations {
            for event in self.plan_events() {
                owner.apply(event);
            }
        }
        self.staged_commit = None;
        self.commit_admission = None;
        self.catalog_history = None;
        self.mode = TxnMode::Idle;
        self.command_id = 0;
        self.command_counter = 0;
        self.failed = false;
        self.isolation = TransactionIsolation::ReadCommitted;
        self.read_only = false;
        self.deferrable = false;
        self.sources = TransactionSources::default();
        self.replication_apply = false;
        self.extension_script = false;
        self.snapshot_lsn = None;
        self.snapshot_taken = false;
        self.transaction_start = None;
        self.suspended_statement = None;
        self.cache_capacity_wait = None;
        self.checkpoint_request = None;
        self.touched.clear();
        self.truncates.clear();
        self.truncate_wal_tables.clear();
        self.ddl.clear();
        self.ddl_origins.clear();
        self.ddl_plan_events.clear();
        self.ddl_origin = 0;
        self.next_ddl_origin = 0;
        self.concurrent_partition_detach_pending = false;
        self.statistics_undo.clear();
        self.savepoints.clear();
        // Commit flushes these before clearing; rollback drops them here.
        self.pending_notifies.clear();
        self.notify_payloads.clear();
        self.pending_listen_ops.clear();
        self.subscription_advances.clear();
        self.deferred_constraints.clear();
        self.completed_constraints.clear();
        self.constraint_modes.clear();
        self.constraint_renames.clear();
        self.deferred_triggers.clear();
        self.completed_deferred_triggers.clear();
        self.deferred_trigger_bytes.clear();
        for descriptor in self.large_object_descriptors.iter_mut() {
            descriptor.active = false;
        }
    }
}

#[cfg(all(test, not(all(miri, feature = "miri-ownership"))))]
mod tests {
    use super::*;

    #[test]
    fn statement_retry_restores_large_object_positions_and_descriptor_lifetimes() {
        let mut budget = Budget::new(TxnState::budget_bytes(8) + 4096);
        let mut txn = TxnState::new(&mut budget, 8).unwrap();
        let arena = crate::mem::arena::Arena::new(&mut budget, "descriptor retry", 4096).unwrap();
        let mode = LargeObjectDescriptorMode {
            readable: true,
            writable: true,
        };
        let oid = crate::storage::LargeObjectOid::parse(42).unwrap();
        crate::mem::guard::forbid_alloc(|| {
            let empty = txn.statement_large_object_descriptors(&arena).unwrap();
            assert!(empty.is_empty());
            let first = txn.open_large_object_descriptor(oid, mode).unwrap();
            txn.large_object_descriptor_mut(first).unwrap().position = 19;
            let saved = txn.statement_large_object_descriptors(&arena).unwrap();
            for _ in 0..3 {
                txn.large_object_descriptor_mut(first).unwrap().position += 11;
                txn.close_large_object_descriptor(first).unwrap();
                let reused = txn
                    .open_large_object_descriptor(
                        crate::storage::LargeObjectOid::parse(43).unwrap(),
                        mode,
                    )
                    .unwrap();
                assert_eq!(reused, first);
                txn.open_large_object_descriptor(oid, mode).unwrap();
                txn.restore_statement_large_object_descriptors(saved);
                assert_eq!(txn.large_object_descriptor(first).unwrap().position, 19);
                assert_eq!(txn.large_object_descriptor(first).unwrap().oid, oid);
                assert!(txn.large_object_descriptor(first + 1).is_err());
            }
            txn.restore_statement_large_object_descriptors(empty);
            assert!(txn.large_object_descriptor(first).is_err());
        });
    }

    #[test]
    fn configured_transaction_budgets_saturate_and_valid_reservations_are_exact() {
        for field in 0..8 {
            let mut config = crate::config::Config::default_dev();
            match field {
                0 => config.txn_rows = usize::MAX,
                1 => config.max_ddl_per_transaction = usize::MAX,
                2 => config.max_tables = usize::MAX,
                3 => config.max_analyze_per_transaction = usize::MAX,
                4 => config.max_savepoints_per_transaction = usize::MAX,
                5 => config.max_deferred_constraints_per_transaction = usize::MAX,
                6 => config.deferred_trigger_bytes = usize::MAX,
                7 => config.max_large_object_descriptors = usize::MAX,
                _ => unreachable!(),
            }
            assert_eq!(
                TxnState::budget_bytes_with_config(&config),
                usize::MAX,
                "configured field {field}"
            );
        }
        assert_eq!(TxnState::budget_bytes(usize::MAX), usize::MAX);
        let mut config = crate::config::Config::default_dev();
        config.txn_rows = 3;
        config.max_ddl_per_transaction = 2;
        config.max_tables = 2;
        config.max_analyze_per_transaction = 1;
        config.max_savepoints_per_transaction = 3;
        config.max_deferred_constraints_per_transaction = 2;
        config.deferred_trigger_bytes = 73;
        config.max_large_object_descriptors = 2;
        let bytes = TxnState::budget_bytes_with_config(&config);
        let mut budget = Budget::new(bytes);
        let _transaction = TxnState::new_with_config(&mut budget, &config).unwrap();
        assert_eq!(budget.remaining(), 0);
    }

    #[test]
    fn visible_row_heap_ownership_command_exhaustion_preserves_snapshot_boundary() {
        let mut budget = Budget::new(TxnState::budget_bytes(8));
        let mut txn = TxnState::new(&mut budget, 8).unwrap();
        txn.test_set_command_id(crate::storage::SNAPSHOT_ALL - 2);
        crate::mem::guard::forbid_alloc(|| {
            txn.begin_command().unwrap();
            assert_eq!(txn.command_id(), crate::storage::SNAPSHOT_ALL - 1);
            for _ in 0..2 {
                let error = txn.begin_command().unwrap_err();
                assert_eq!(error.sqlstate, sqlstate::PROGRAM_LIMIT_EXCEEDED);
                assert_eq!(txn.command_id(), crate::storage::SNAPSHOT_ALL - 1);
            }
            txn.clear();
            assert_eq!(txn.command_id(), 0);
            txn.begin_command().unwrap();
            assert_eq!(txn.command_id(), 1);
        });
    }
}
