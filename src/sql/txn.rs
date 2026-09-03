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

use super::ast::TransactionIsolation;
use super::eval::SqlError;

/// A row's pending image before a write, as returned by `write_pending`:
/// `None` = no pending existed; `Some(loc)` = a pending change with that loc.
pub type PriorPending = Option<Option<RowLoc>>;

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

pub const MAX_SAVEPOINTS: usize = 16;
pub const DEFAULT_MAX_LARGE_OBJECT_DESCRIPTORS: usize = 64;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct LargeObjectDescriptorMode {
    pub readable: bool,
    pub writable: bool,
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct LargeObjectDescriptor {
    pub oid: crate::storage::LargeObjectOid,
    pub position: i64,
    pub mode: LargeObjectDescriptorMode,
    opened_savepoint_depth: u8,
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

pub struct TxnState {
    pub mode: TxnMode,
    /// An error occurred inside an explicit block: everything until
    /// COMMIT/ROLLBACK fails with 25P02.
    pub failed: bool,
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
    rule_stack: [i32; MAX_RULE_NESTING],
    rule_depth: u8,
    /// A concurrent partition detach published its first durable phase. The
    /// protocol executor commits that phase before it parks for finalization.
    concurrent_partition_detach_pending: bool,
    /// Every row write, in order: (table slot, rowid, pending image before the
    /// write). Recorded per write (not per row) so `ROLLBACK TO SAVEPOINT` can
    /// reverse-replay to any earlier point.
    touched: FixedVec<(u32, u64, PriorPending)>,
    truncates: FixedVec<TruncateEvent>,
    truncate_wal_tables: FixedBuf,
    /// DDL performed in this transaction, for rollback.
    ddl: FixedVec<DdlUndo>,
    ddl_origins: FixedVec<u32>,
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
    ViewSchemaChanged {
        slot: u32,
        prior: Option<crate::storage::PendingObjectSchema>,
    },
    ViewRenamed {
        slot: u32,
        prior: Option<crate::storage::PendingViewName>,
    },
    ViewOptionsChanged {
        slot: u32,
        prior: Option<crate::storage::PendingViewOptions>,
    },
    ViewColumnsChanged {
        slot: u32,
        prior: Option<crate::storage::PendingViewColumns>,
    },
    RuleCreated {
        slot: u32,
        prior_table_rule_txid: Option<u32>,
    },
    RuleAltered {
        slot: u32,
        prior: Option<crate::storage::PendingRuleDefinition>,
    },
    RuleDropped(u32),
    /// CREATE FUNCTION at this slot — undo by dropping its pending definition.
    RoutineCreated(u32),
    RoutineDropped(u32),
    RoutineReplaced {
        slot: u32,
        prior: Option<crate::storage::PendingRoutineDefinition>,
    },
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
    MatviewCreated(u32),
    /// DROP MATERIALIZED VIEW at this slot — undo by reviving it.
    MatviewDropped(u32),
    /// CREATE SEQUENCE at this slot — undo by dropping it. (Its *value* state is
    /// not transactional; only existence rolls back.)
    SequenceCreated(u32),
    /// DROP SEQUENCE at this slot — undo by reviving it.
    SequenceDropped(u32),
    LargeObjectCreated(u32),
    LargeObjectDropped(u32),
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
    DatabaseCreated(u32),
    DatabaseAltered {
        slot: u32,
        prior_definition: Option<crate::storage::PendingDatabaseDefinition>,
        prior_owner: Option<crate::storage::PendingOwnership>,
    },
    DatabaseDropped(u32),
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
    /// ALTER SCHEMA ... RENAME TO changed a schema identity in place.
    SchemaRenamed {
        slot: u32,
        prior: crate::storage::SqlName,
    },
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
        prior: Option<crate::storage::PendingAcl>,
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
        slot: u32,
        prior: Option<crate::storage::PendingParameterAcl>,
    },
    /// A `COMMENT ON` set or removed an object's comment — undo by restoring
    /// the slot's prior uncommitted overlay. On commit, the overlay is
    /// promoted and journaled.
    CommentSet {
        slot: u32,
        prior: Option<crate::storage::PendingComment>,
    },
    /// A table-constraint rename changed a comment's transaction-local name.
    ConstraintCommentRenamed {
        slot: u32,
        prior: Option<crate::storage::PendingCommentIdentity>,
    },
}

/// Sized for a DROP SCHEMA CASCADE closure: every contained table, view and
/// transaction-versioned inbound foreign key takes one undo entry.
pub const MAX_TXN_DDL: usize = 64;
pub const MAX_TXN_ANALYZE: usize = crate::storage::MAX_PENDING_STATISTICS_PER_TXN;
const SUBSCRIPTION_ADVANCES_PER_TXN: usize = 1;
pub const MAX_DEFERRED_CONSTRAINTS: usize = 128;

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
    pub(crate) updated_columns: u64,
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
    Table(u32),
    Extended(u32),
}

impl TxnState {
    pub const fn budget_bytes(capacity: usize) -> usize {
        Self::budget_bytes_with_large_objects(capacity, DEFAULT_MAX_LARGE_OBJECT_DESCRIPTORS)
    }

    pub const fn budget_bytes_with_large_objects(
        capacity: usize,
        descriptor_capacity: usize,
    ) -> usize {
        capacity * core::mem::size_of::<(u32, u64, PriorPending)>()
            + MAX_TXN_DDL * core::mem::size_of::<TruncateEvent>()
            + MAX_TRUNCATE_WAL_TABLE_BYTES
            + MAX_TXN_DDL * core::mem::size_of::<DdlUndo>()
            + MAX_TXN_DDL * core::mem::size_of::<u32>()
            + MAX_TXN_ANALYZE * core::mem::size_of::<StatisticsUndo>()
            + MAX_SAVEPOINTS * core::mem::size_of::<Savepoint>()
            + crate::sql::notify::PER_TXN
                * core::mem::size_of::<crate::sql::notify::BufferedNotify>()
            + crate::sql::notify::PER_TXN_PAYLOAD_BYTES
            + crate::sql::notify::LISTEN_OPS_PER_TXN
                * core::mem::size_of::<crate::sql::notify::ListenOp>()
            + SUBSCRIPTION_ADVANCES_PER_TXN
                * core::mem::size_of::<crate::storage::SubscriptionAdvance>()
            + MAX_DEFERRED_CONSTRAINTS * core::mem::size_of::<ConstraintObligation>()
            + MAX_DEFERRED_CONSTRAINTS * core::mem::size_of::<usize>()
            + MAX_DEFERRED_CONSTRAINTS * core::mem::size_of::<ConstraintModeChange>()
            + MAX_DEFERRED_CONSTRAINTS * core::mem::size_of::<ConstraintLifecycle>()
            + MAX_DEFERRED_CONSTRAINTS * core::mem::size_of::<DeferredTriggerEvent>()
            + MAX_DEFERRED_CONSTRAINTS * core::mem::size_of::<usize>()
            + DEFERRED_TRIGGER_BYTES
            + descriptor_capacity * core::mem::size_of::<LargeObjectDescriptor>()
    }

    pub fn new(budget: &mut Budget, capacity: usize) -> Result<Self, BudgetError> {
        Self::new_with_large_objects(budget, capacity, DEFAULT_MAX_LARGE_OBJECT_DESCRIPTORS)
    }

    pub fn new_with_large_objects(
        budget: &mut Budget,
        capacity: usize,
        descriptor_capacity: usize,
    ) -> Result<Self, BudgetError> {
        let mut large_object_descriptors =
            FixedVec::new(budget, "large_object_descriptors", descriptor_capacity)?;
        for _ in 0..descriptor_capacity {
            large_object_descriptors
                .push(LargeObjectDescriptor::EMPTY)
                .expect("sized to descriptor capacity");
        }
        Ok(Self {
            mode: TxnMode::Idle,
            failed: false,
            txid: 0,
            isolation: TransactionIsolation::ReadCommitted,
            read_only: false,
            deferrable: false,
            sources: TransactionSources::default(),
            replication_apply: false,
            extension_script: false,
            snapshot_lsn: None,
            snapshot_taken: false,
            command_id: 1,
            trigger_depth: 0,
            rule_stack: [0; MAX_RULE_NESTING],
            rule_depth: 0,
            concurrent_partition_detach_pending: false,
            touched: FixedVec::new(budget, "txn_touched", capacity)?,
            truncates: FixedVec::new(budget, "txn_truncates", MAX_TXN_DDL)?,
            truncate_wal_tables: FixedBuf::new(
                budget,
                "txn_truncate_wal_tables",
                MAX_TRUNCATE_WAL_TABLE_BYTES,
            )?,
            ddl: FixedVec::new(budget, "txn_ddl", MAX_TXN_DDL)?,
            ddl_origins: FixedVec::new(budget, "txn_ddl_origins", MAX_TXN_DDL)?,
            ddl_origin: 0,
            next_ddl_origin: 0,
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
            deferred_constraints: FixedVec::new(
                budget,
                "txn_deferred_constraints",
                MAX_DEFERRED_CONSTRAINTS,
            )?,
            completed_constraints: FixedVec::new(
                budget,
                "txn_completed_constraints",
                MAX_DEFERRED_CONSTRAINTS,
            )?,
            constraint_modes: FixedVec::new(
                budget,
                "txn_constraint_modes",
                MAX_DEFERRED_CONSTRAINTS,
            )?,
            constraint_renames: FixedVec::new(
                budget,
                "txn_constraint_renames",
                MAX_DEFERRED_CONSTRAINTS,
            )?,
            deferred_triggers: FixedVec::new(
                budget,
                "txn_deferred_triggers",
                MAX_DEFERRED_CONSTRAINTS,
            )?,
            completed_deferred_triggers: FixedVec::new(
                budget,
                "txn_completed_deferred_triggers",
                MAX_DEFERRED_CONSTRAINTS,
            )?,
            deferred_trigger_bytes: FixedBuf::new(
                budget,
                "txn_deferred_trigger_bytes",
                DEFERRED_TRIGGER_BYTES,
            )?,
            large_object_descriptors,
        })
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
            opened_savepoint_depth: self.savepoints.len() as u8,
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
            if descriptor.active && usize::from(descriptor.opened_savepoint_depth) > savepoint_depth
            {
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

    pub(crate) fn trigger_depth(&self) -> u16 {
        self.trigger_depth
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

    pub fn leave_trigger_sql(&mut self) {
        self.trigger_depth = self
            .trigger_depth
            .checked_sub(1)
            .expect("trigger depth is paired");
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

    pub fn import_snapshot(&mut self, snapshot: u64) -> Result<(), SqlError> {
        if !self.is_explicit() {
            return Err(sql_err!(
                sqlstate::NO_ACTIVE_SQL_TRANSACTION,
                "SET TRANSACTION SNAPSHOT can only be used in transaction blocks"
            ));
        }
        if self.snapshot_taken {
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
                "a snapshot-importing transaction must use REPEATABLE READ or SERIALIZABLE"
            ));
        }
        self.snapshot_lsn = Some(snapshot);
        self.snapshot_taken = true;
        Ok(())
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
                    MAX_DEFERRED_CONSTRAINTS
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
                MAX_DEFERRED_CONSTRAINTS
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
                MAX_DEFERRED_CONSTRAINTS
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
                    MAX_DEFERRED_CONSTRAINTS
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
                    MAX_DEFERRED_CONSTRAINTS
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
        updated_columns: u64,
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
        updated_columns: u64,
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
        updated_columns: u64,
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
                    DEFERRED_TRIGGER_BYTES
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
                MAX_DEFERRED_CONSTRAINTS
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
                MAX_DEFERRED_CONSTRAINTS
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
            read_only: self.read_only,
            read_only_source: self.sources.read_only,
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
                MAX_SAVEPOINTS
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

    pub(crate) fn owns_statement_mark(&self, mark: StatementMark) -> bool {
        self.txid == mark.txid
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
            if descriptor.active && usize::from(descriptor.opened_savepoint_depth) > index {
                descriptor.opened_savepoint_depth = index as u8;
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
            self.ddl_origins.pop();
        }
    }

    pub(crate) fn record_statistics(&mut self, table: u32) -> Result<(), SqlError> {
        self.statistics_undo
            .push(StatisticsUndo::Table(table))
            .map_err(|_| {
                sql_err!(
                    crate::sql::eval::sqlstate::PROGRAM_LIMIT_EXCEEDED,
                    "more than {} ANALYZE targets in one transaction",
                    MAX_TXN_ANALYZE
                )
            })
    }

    pub(crate) fn record_extended_statistics(&mut self, slot: u32) -> Result<(), SqlError> {
        self.statistics_undo
            .push(StatisticsUndo::Extended(slot))
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
        })?;
        self.ddl_origins
            .push(self.ddl_origin)
            .expect("parallel DDL origin pool has the same capacity");
        Ok(())
    }

    pub(crate) fn ddl(&self) -> &[DdlUndo] {
        &self.ddl
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
        self.mode = TxnMode::Idle;
        self.failed = false;
        self.isolation = TransactionIsolation::ReadCommitted;
        self.read_only = false;
        self.deferrable = false;
        self.sources = TransactionSources::default();
        self.replication_apply = false;
        self.extension_script = false;
        self.snapshot_lsn = None;
        self.snapshot_taken = false;
        self.touched.clear();
        self.truncates.clear();
        self.truncate_wal_tables.clear();
        self.ddl.clear();
        self.ddl_origins.clear();
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
