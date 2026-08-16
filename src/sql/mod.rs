//! SQL front end: lexer → parser → execution, and the engine entry point
//! the wire protocol calls.

pub mod array;
pub mod ast;
pub mod catalog;
pub mod copy;
pub mod cursor;
pub mod datetime;
pub mod encoding;
pub mod eval;
pub mod exec;
mod explain;
pub(crate) mod external;
pub mod guc;
pub mod json;
pub mod lexer;
pub(crate) mod lock;
pub mod md5;
pub mod net;
pub mod notify;
pub mod numeric;
pub mod parser;
pub mod prep;
pub mod query;
pub mod range;
pub mod regex;
pub mod ryu;
pub mod sequence;
pub mod sha512;
pub mod timezone;
pub mod to_char;
pub mod txn;
pub mod types;
pub mod tzif;

use crate::checkpoint::{CheckpointSetupError, CheckpointStep, Checkpointer};
use crate::config::Config;
use crate::mem::arena::Arena;
use crate::mem::budget::{Budget, BudgetError};
use crate::mem::buffer::FixedBuf;
use crate::mem::fixed_vec::FixedVec;
use crate::pg::pgoutput;
use crate::pg::respond::Responder;
use crate::pg::wire::WireFull;
use crate::sql_err;
use crate::stack_format;
use crate::storage::{ColumnMeta, RowHome, RowLoc, SqlName, Storage};
use crate::wal::{Wal, WalOp, WalSetupError, encoded_record_len};

use crate::pg::conn::MAX_BIND_PARAMS;
use ast::{Delete, Expr, Insert, Stmt, Update};
use eval::{EvalHooks, NO_HOOKS, NO_PARAMS, NoColumns, SequenceAccess, SqlError, eval, sqlstate};
use exec::MAX_PROJ;
use guc::GucState;
use parser::{ParseError, Parser};
use prep::SqlPreparedPool;
use txn::{DdlUndo, IsolationLevel, TxnMode, TxnState};
use types::{ColDesc, ColType, Datum};

type ReturningCapture<'a> = dyn for<'row> FnMut(&[Datum<'row>]) -> Result<(), SqlError> + 'a;

/// Complete durable input for binding one startup-reserved subscription worker.
/// Values are copied out of the catalog so the reactor never holds a catalog
/// borrow while it drives a network socket or applies a remote transaction.
#[derive(Clone, Copy)]
pub(crate) struct SubscriptionRuntime {
    pub name: SqlName,
    pub endpoint: crate::pg::replication_client::ConnectionInfo,
    pub publications: [SqlName; crate::storage::MAX_SUBSCRIPTION_PUBLICATIONS],
    pub publication_count: usize,
    pub slot: SqlName,
    pub confirmed_lsn: u64,
}

#[derive(Debug)]
pub enum EngineSetupError {
    Budget(BudgetError),
    Wal(WalSetupError),
    Checkpoint(CheckpointSetupError),
    /// A storage operation during recovery failed loudly — e.g. the recovered
    /// data exceeds the configured value-index capacity.
    Storage(SqlError),
}

impl From<SqlError> for EngineSetupError {
    fn from(e: SqlError) -> Self {
        Self::Storage(e)
    }
}

impl std::fmt::Display for EngineSetupError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Budget(e) => write!(f, "{e}"),
            Self::Wal(e) => write!(f, "{e}"),
            Self::Checkpoint(e) => write!(f, "{e}"),
            Self::Storage(e) => write!(f, "{}", e.message.as_str()),
        }
    }
}

impl From<CheckpointSetupError> for EngineSetupError {
    fn from(e: CheckpointSetupError) -> Self {
        Self::Checkpoint(e)
    }
}

impl std::error::Error for EngineSetupError {}

impl From<BudgetError> for EngineSetupError {
    fn from(e: BudgetError) -> Self {
        Self::Budget(e)
    }
}

impl From<WalSetupError> for EngineSetupError {
    fn from(e: WalSetupError) -> Self {
        Self::Wal(e)
    }
}

/// Placeholder for the fixed-size array of data-modifying-CTE materializations.
static EMPTY_DML_CTE: ast::MaterializedCte<'static> = ast::MaterializedCte {
    column_names: &[],
    column_types: &[],
    column_collations: &[],
    rows: &[],
    external_run: None,
};

/// The query engine: catalog, memtable storage, WAL, object-storage
/// checkpointing, and statement execution.
pub struct Engine {
    storage: Storage,
    wal: Wal,
    ckpt: Option<Checkpointer>,
    /// A published manifest whose local bookkeeping still needs to finish.
    /// Keeping this state makes publication and its cleanup one retriable
    /// completion protocol instead of reporting success after the manifest.
    post_publish_cleanup: Option<u64>,
    /// A COPY FROM STDIN the last statement started: the connection takes
    /// it, switches into copy-in mode, and feeds data lines back through
    /// [`Engine::copy_row_line`] until CopyDone.
    pending_copy: Option<exec::CopySetup>,
    wal_upload: bool,
    /// Scratch buffer for reading committed WAL batches before the
    /// provider-neutral object PUT.
    wal_seg_buf: Vec<u8>,
    /// Scratch for materializing scans (ORDER BY, UPDATE, DELETE) and for
    /// sorting SST entries at checkpoint.
    scratch: FixedVec<(u64, RowHome)>,
    /// Scratch for heap compaction: every live row image across tables.
    compact_scratch: FixedVec<(u32, u64, u8, RowLoc)>,
    /// Shared execution arena: one query's materialized rows (ORDER BY /
    /// DISTINCT / GROUP BY buffers) live here, separate from the small
    /// per-connection AST arena. Single-threaded execution means one
    /// instance serves every connection; reset at the start of each
    /// statement. This is the `work_mem` analogue.
    work: Arena,
    next_txid: u32,
    /// LISTEN/NOTIFY registry and delivery outbox, shared across every
    /// connection (see [`notify`]).
    notify: notify::NotifyState,
    /// The connection id whose message is currently being executed, set at each
    /// `execute_simple`/`execute_extended` entry so LISTEN/UNLISTEN/NOTIFY can
    /// stamp their buffered ops without threading the id through every arm.
    current_conn_id: i32,
    /// Stable identity exposed by the replication protocol. It is derived
    /// from the durable namespace rather than process-local state, so a
    /// restarted or cold-recovered server remains the same publisher.
    replication_system_id: u64,
    /// Authenticated sessions per fixed role slot, used to enforce
    /// `CONNECTION LIMIT` without allocating in the server loop.
    role_connections: [u16; crate::storage::MAX_ROLES],
}

#[derive(Clone, Copy)]
pub(crate) struct RoleLogin {
    pub slot: u16,
    pub can_login: bool,
    pub valid: bool,
    pub superuser: bool,
    pub replication: bool,
    pub connection_limit: i32,
    pub password: Option<crate::storage::RolePassword>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExecutionStatus {
    Complete,
    /// The current statement emitted no client-visible output and is parked
    /// on a row lock or a non-blocking block fetch. Statements before it in
    /// the same simple-query message completed exactly once and are skipped
    /// when the message resumes.
    Blocked {
        completed_statements: usize,
        output_mark: usize,
        /// True when the block is an I/O wait (retry on every reactor wake),
        /// false for a lock wait (retry only on lock-generation change).
        io_wait: bool,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExtendedExecutionStatus {
    Complete(bool),
    Blocked { io_wait: bool },
}

#[derive(Clone, Copy)]
struct TransactionCharacteristics {
    isolation: Option<IsolationLevel>,
    read_only: Option<bool>,
    deferrable: Option<bool>,
}

fn transaction_characteristics(text: &str) -> Result<TransactionCharacteristics, &str> {
    let mut parsed = TransactionCharacteristics {
        isolation: None,
        read_only: None,
        deferrable: None,
    };
    let mut words = text
        .split(|character: char| character.is_whitespace() || character == ',')
        .filter(|word| !word.is_empty());
    while let Some(word) = words.next() {
        if word.eq_ignore_ascii_case("isolation") {
            let Some(level) = words.next() else {
                return Err(word);
            };
            let Some(first) = words.next() else {
                return Err(level);
            };
            if !level.eq_ignore_ascii_case("level") {
                return Err(level);
            }
            if first.eq_ignore_ascii_case("serializable") {
                parsed.isolation = Some(IsolationLevel::Serializable);
            } else {
                let Some(second) = words.next() else {
                    return Err(first);
                };
                if first.eq_ignore_ascii_case("read") && second.eq_ignore_ascii_case("committed") {
                    parsed.isolation = Some(IsolationLevel::ReadCommitted);
                } else if first.eq_ignore_ascii_case("repeatable")
                    && second.eq_ignore_ascii_case("read")
                {
                    parsed.isolation = Some(IsolationLevel::RepeatableRead);
                } else {
                    return Err(first);
                }
            }
        } else if word.eq_ignore_ascii_case("read") {
            let Some(mode) = words.next() else {
                return Err(word);
            };
            if mode.eq_ignore_ascii_case("only") {
                parsed.read_only = Some(true);
            } else if mode.eq_ignore_ascii_case("write") {
                parsed.read_only = Some(false);
            } else {
                return Err(mode);
            }
        } else if word.eq_ignore_ascii_case("deferrable") {
            parsed.deferrable = Some(true);
        } else if word.eq_ignore_ascii_case("not") {
            let Some(characteristic) = words.next() else {
                return Err(word);
            };
            if !characteristic.eq_ignore_ascii_case("deferrable") {
                return Err(characteristic);
            }
            parsed.deferrable = Some(false);
        } else {
            return Err(word);
        }
    }
    Ok(parsed)
}

fn statement_writes(statement: &Stmt<'_>) -> bool {
    match statement {
        Stmt::Explain { options, statement } => {
            options.analyze && statement_writes(statement)
        }
        Stmt::Select(_)
        | Stmt::SetQuery(_)
        | Stmt::Begin(_)
        | Stmt::Commit
        | Stmt::Rollback
        | Stmt::Savepoint(_)
        | Stmt::ReleaseSavepoint(_)
        | Stmt::RollbackToSavepoint(_)
        | Stmt::LockTable { .. }
        | Stmt::Set { .. }
        | Stmt::Reset(_)
        | Stmt::SetTransaction(_)
        | Stmt::SetRole { .. }
        | Stmt::SetSessionAuthorization { .. }
        | Stmt::Show(_)
        | Stmt::ShowAll
        | Stmt::Prepare { .. }
        // EXECUTE recursively dispatches the parsed prepared statement, where
        // the actual command is checked before it can mutate anything.
        | Stmt::ExecutePrepared { .. }
        | Stmt::Deallocate(_)
        | Stmt::DeclareCursor { .. }
        | Stmt::FetchCursor { .. }
        | Stmt::CloseCursor(_)
        | Stmt::Analyze(_)
        | Stmt::Listen(_)
        | Stmt::Unlisten(_) => false,
        Stmt::Call { .. } => true,
        Stmt::Copy(copy) => !copy.to,
        // A WITH wrapper exists only for a data-modifying main statement.
        Stmt::With { .. }
        | Stmt::Insert(_)
        | Stmt::Update(_)
        | Stmt::Delete(_)
        | Stmt::Merge(_)
        | Stmt::CreateTable(_)
        | Stmt::DropTable(_)
        | Stmt::Truncate { .. }
        | Stmt::CreateView { .. }
        | Stmt::CreateRoutine(_)
        | Stmt::AlterRoutine { .. }
        | Stmt::DropFunction { .. }
        | Stmt::DropProcedure { .. }
        | Stmt::DropRoutine { .. }
        | Stmt::DropView { .. }
        | Stmt::CreatePublication { .. }
        | Stmt::AlterPublication { .. }
        | Stmt::DropPublication { .. }
        | Stmt::CreateSubscription { .. }
        | Stmt::AlterSubscription { .. }
        | Stmt::DropSubscription { .. }
        | Stmt::CreateTrigger(_)
        | Stmt::AlterTrigger { .. }
        | Stmt::DropTrigger { .. }
        | Stmt::CreateTableAs { .. }
        | Stmt::RefreshMaterializedView { .. }
        | Stmt::DropMaterializedView { .. }
        | Stmt::CreateSequence { .. }
        | Stmt::AlterSequence { .. }
        | Stmt::DropSequence { .. }
        | Stmt::CreateDomain(_)
        | Stmt::AlterDomain { .. }
        | Stmt::DropDomain { .. }
        | Stmt::CreateEnum { .. }
        | Stmt::AlterType { .. }
        | Stmt::DropType { .. }
        | Stmt::CreateIndex { .. }
        | Stmt::AlterIndex { .. }
        | Stmt::DropIndex { .. }
        | Stmt::Reindex { .. }
        | Stmt::Checkpoint
        | Stmt::AlterTable(_)
        | Stmt::CreateSchema { .. }
        | Stmt::DropSchema { .. }
        | Stmt::Vacuum { .. }
        | Stmt::Notify { .. }
        | Stmt::Comment { .. }
        | Stmt::AlterOwner { .. }
        | Stmt::CreateRole { .. }
        | Stmt::AlterRole { .. }
        | Stmt::AlterRoleRename { .. }
        | Stmt::DropRole { .. } => true,
        Stmt::GrantRole { .. }
        | Stmt::RevokeRole { .. }
        | Stmt::GrantPrivileges { .. }
        | Stmt::RevokePrivileges { .. }
        | Stmt::AlterDefaultPrivileges { .. }
        | Stmt::ReassignOwned { .. }
        | Stmt::DropOwned { .. } => true,
    }
}

fn explained_root_rows(statement: &Stmt<'_>, emitted_rows: u64) -> u64 {
    match statement {
        // PostgreSQL's ModifyTable node reports zero output rows even when its
        // RETURNING projection sends rows to the client.
        Stmt::Insert(_) | Stmt::Update(_) | Stmt::Delete(_) | Stmt::Merge(_) => 0,
        Stmt::With { statement, .. } => explained_root_rows(statement, emitted_rows),
        _ => emitted_rows,
    }
}

fn statement_changes_schema(statement: &Stmt<'_>) -> bool {
    matches!(
        statement,
        Stmt::AlterDomain { .. }
            | Stmt::DropDomain { .. }
            | Stmt::AlterType { .. }
            | Stmt::DropType { .. }
            | Stmt::DropSchema { .. }
    )
}

fn statement_tag(statement: &Stmt<'_>) -> &'static str {
    match statement {
        Stmt::Explain { statement, .. } => statement_tag(statement),
        Stmt::With { statement, .. } => statement_tag(statement),
        Stmt::LockTable { .. } => "LOCK TABLE",
        Stmt::Insert(_) => "INSERT",
        Stmt::Update(_) => "UPDATE",
        Stmt::Delete(_) => "DELETE",
        Stmt::Merge(_) => "MERGE",
        Stmt::Copy(_) => "COPY FROM",
        Stmt::Truncate { .. } => "TRUNCATE",
        Stmt::Vacuum { .. } => "VACUUM",
        Stmt::Checkpoint => "CHECKPOINT",
        Stmt::Reindex { .. } => "REINDEX",
        Stmt::Notify { .. } => "NOTIFY",
        _ => "DDL",
    }
}

#[derive(Clone, Copy)]
struct PendingTruncate {
    command_id: u32,
    table_slots: [u16; crate::sql::txn::MAX_TRUNCATE_TABLES],
    table_count: usize,
    cascade: bool,
    restart_identity: bool,
    emitted: bool,
}

#[derive(Clone, Copy)]
struct ReplicationType {
    oid: i32,
    schema: SqlName,
    name: SqlName,
}

fn replication_column_types(
    storage: &Storage,
    columns: &[ColumnMeta],
) -> Result<
    (
        [i32; crate::storage::MAX_COLUMNS],
        [Option<ReplicationType>; crate::storage::MAX_COLUMNS],
    ),
    SqlError,
> {
    let mut type_oids = [0_i32; crate::storage::MAX_COLUMNS];
    let mut types = [None; crate::storage::MAX_COLUMNS];
    for (index, column) in columns.iter().enumerate() {
        let declared_type = storage.declared_column_type(column, 0)?;
        type_oids[index] = declared_type.replication_oid();
        if let Some((schema, name)) = declared_type.replication_user_type() {
            types[index] = Some(ReplicationType {
                oid: declared_type.replication_oid(),
                schema,
                name,
            });
        }
    }
    Ok((type_oids, types))
}

fn emit_replication_relation(
    storage: &Storage,
    definition: &crate::storage::TableDef,
    relation_id: u32,
    responder: &mut Responder,
    end_lsn: u64,
) -> Result<(), SqlError> {
    let columns = definition.columns();
    let (type_oids, types) = replication_column_types(storage, columns)?;
    let overflow = || {
        sql_err!(
            sqlstate::PROGRAM_LIMIT_EXCEEDED,
            "replication transaction exceeds connection send buffer"
        )
    };
    for (index, type_info) in types.iter().enumerate().take(columns.len()) {
        let Some(type_info) = type_info else {
            continue;
        };
        if types[..index]
            .iter()
            .flatten()
            .any(|prior| prior.oid == type_info.oid)
        {
            continue;
        }
        responder
            .copy_data(&|message| {
                pgoutput::xlog_data(message, end_lsn, end_lsn, |plugin| {
                    pgoutput::type_message(
                        plugin,
                        type_info.oid,
                        type_info.schema.as_str(),
                        type_info.name.as_str(),
                    )
                })
            })
            .map_err(|_| overflow())?;
    }
    responder
        .copy_data(&|message| {
            pgoutput::xlog_data(message, end_lsn, end_lsn, |plugin| {
                pgoutput::relation(
                    plugin,
                    relation_id,
                    definition.schema.as_str(),
                    definition.name.as_str(),
                    columns,
                    &type_oids[..columns.len()],
                )
            })
        })
        .map_err(|_| overflow())
}

fn emit_pending_truncates(
    storage: &Storage,
    publication_names: &[SqlName],
    proto_version: crate::pg::pgoutput::ProtocolVersion,
    end_lsn: u64,
    command_id: u32,
    truncates: &mut [PendingTruncate],
    responder: &mut Responder,
) -> Result<(), SqlError> {
    for truncate in truncates {
        if truncate.emitted || truncate.command_id > command_id {
            continue;
        }
        let mut relation_ids = [0_u32; crate::sql::txn::MAX_TRUNCATE_TABLES];
        let mut relation_count = 0usize;
        for &table_slot in &truncate.table_slots[..truncate.table_count] {
            let table_slot = table_slot as usize;
            if !publication_selects(
                storage,
                publication_names,
                table_slot,
                PublicationOperation::Truncate,
            )? {
                continue;
            }
            let definition = storage.table_def(table_slot, 0);
            let relation_id = table_slot as u32 + 1;
            emit_replication_relation(storage, definition, relation_id, responder, end_lsn)?;
            relation_ids[relation_count] = relation_id;
            relation_count += 1;
        }
        if relation_count != 0 {
            if proto_version < crate::pg::pgoutput::ProtocolVersion::V2 {
                return Err(sql_err!(
                    sqlstate::FEATURE_NOT_SUPPORTED,
                    "pgoutput proto_version '2' is required for TRUNCATE"
                ));
            }
            responder
                .copy_data(&|message| {
                    pgoutput::xlog_data(message, end_lsn, end_lsn, |plugin| {
                        pgoutput::truncate(
                            plugin,
                            &relation_ids[..relation_count],
                            truncate.cascade,
                            truncate.restart_identity,
                        )
                    })
                })
                .map_err(|_| {
                    sql_err!(
                        sqlstate::PROGRAM_LIMIT_EXCEEDED,
                        "replication transaction exceeds connection send buffer"
                    )
                })?;
        }
        truncate.emitted = true;
    }
    Ok(())
}

#[derive(Clone, Copy)]
enum PublicationOperation {
    Insert,
    Update,
    Delete,
    Truncate,
}

fn publication_selects(
    storage: &Storage,
    publication_names: &[SqlName],
    table_slot: usize,
    operation: PublicationOperation,
) -> Result<bool, SqlError> {
    for name in publication_names {
        let publication = storage.publication(name.as_str()).ok_or_else(|| {
            sql_err!(
                sqlstate::UNDEFINED_OBJECT,
                "publication \"{}\" does not exist",
                name.as_str()
            )
        })?;
        let table_schema = storage.table_def(table_slot, 0).schema;
        let member = publication.all_tables
            || publication.tables[..publication.table_count].contains(&(table_slot as u16))
            || storage
                .find_schema(table_schema.as_str())
                .is_some_and(|slot| {
                    publication.schemas[..publication.schema_count].contains(&(slot as u8))
                });
        let publishes = match operation {
            PublicationOperation::Insert => publication.publish_insert,
            PublicationOperation::Update => publication.publish_update,
            PublicationOperation::Delete => publication.publish_delete,
            PublicationOperation::Truncate => publication.publish_truncate,
        };
        if member && publishes {
            return Ok(true);
        }
    }
    Ok(false)
}

impl Engine {
    pub(crate) fn subscription_runtime(&self, slot: usize) -> Option<SubscriptionRuntime> {
        self.storage
            .subscriptions_with_slots_visible_to(0)
            .find(|(index, _subscription)| *index == slot)
            .filter(|(_, subscription)| {
                subscription.enabled_to(0) && subscription.slot_name != SqlName::EMPTY
            })
            .and_then(|(_, subscription)| {
                subscription
                    .connection
                    .endpoint()
                    .map(|endpoint| SubscriptionRuntime {
                        name: subscription.name,
                        endpoint,
                        publications: subscription.publications,
                        publication_count: subscription.publication_count,
                        slot: subscription.slot_name,
                        confirmed_lsn: subscription.confirmed_lsn,
                    })
            })
    }
    /// Returns the startup-bounded endpoint retained for a durable
    /// subscription.  The apply worker consumes this typed value directly;
    /// it never reparses catalog text at connection time.
    pub fn subscription_endpoint(
        &self,
        name: &str,
    ) -> Option<crate::pg::replication_client::ConnectionInfo> {
        self.storage
            .subscription(name, 0)
            .and_then(|(_, subscription)| subscription.connection.endpoint())
    }

    pub fn subscription_confirmed_lsn(&self, name: &str) -> Option<u64> {
        self.storage
            .subscription(name, 0)
            .map(|(_, subscription)| subscription.confirmed_lsn)
    }

    /// Opens the local transaction that will receive one publisher commit.
    /// The worker uses the ordinary engine transaction and durability path;
    /// replication cannot create a second, weaker write path.
    pub fn begin_subscription_apply(&mut self, txn: &mut TxnState, guc: &GucState) {
        self.ensure_txn(txn, TxnMode::Implicit, guc);
        txn.begin_command();
        // pgoutput messages form one remote transaction, not independent SQL
        // statements.  Each later row operation must therefore see every
        // earlier local change from that same remote commit.
        self.storage.set_read_snapshot(crate::storage::SNAPSHOT_ALL);
    }

    /// Couples a publisher commit position to the active local transaction.
    /// `false` means the position was already committed locally, so a replayed
    /// remote transaction must be skipped before it can mutate rows.
    pub fn stage_subscription_advance(
        &mut self,
        txn: &mut TxnState,
        name: &str,
        confirmed_lsn: u64,
    ) -> Result<bool, SqlError> {
        if !txn.is_active() {
            return Err(sql_err!(
                sqlstate::NO_ACTIVE_SQL_TRANSACTION,
                "subscription progress requires an active apply transaction"
            ));
        }
        let Some(advance) = self
            .storage
            .subscription_advance(name, confirmed_lsn, txn.txid)?
        else {
            return Ok(false);
        };
        txn.record_subscription_advance(advance)?;
        Ok(true)
    }

    /// Registers a publisher relation descriptor before any of its tuples can
    /// enter the ordinary local write path.
    pub fn register_subscription_relation(
        &self,
        relations: &mut crate::pg::subscription_apply::RelationMap,
        txn: &TxnState,
        relation: crate::pg::pginput::Relation<'_>,
    ) -> Result<(), SqlError> {
        relations.register(&self.storage, txn.txid, relation)
    }

    pub fn apply_subscription_insert(
        &mut self,
        txn: &mut TxnState,
        binding: crate::pg::subscription_apply::RelationBinding,
        tuple: crate::pg::pginput::Tuple<'_>,
        arena: &Arena,
    ) -> Result<(), SqlError> {
        exec::apply_replication_insert(&mut self.storage, txn, binding, tuple, arena)
    }

    pub fn apply_subscription_delete(
        &mut self,
        txn: &mut TxnState,
        binding: crate::pg::subscription_apply::RelationBinding,
        old: crate::pg::pginput::OldTuple<'_>,
        arena: &Arena,
        guc: &GucState,
    ) -> Result<(), SqlError> {
        exec::apply_replication_delete(
            &mut self.storage,
            txn,
            binding,
            old,
            arena,
            guc.seq_session(),
        )
    }

    pub fn apply_subscription_update(
        &mut self,
        txn: &mut TxnState,
        binding: crate::pg::subscription_apply::RelationBinding,
        old: crate::pg::pginput::OldTuple<'_>,
        new: crate::pg::pginput::Tuple<'_>,
        arena: &Arena,
        guc: &GucState,
    ) -> Result<(), SqlError> {
        exec::apply_replication_update(
            &mut self.storage,
            txn,
            binding,
            old,
            new,
            arena,
            guc.seq_session(),
        )
    }

    pub fn apply_subscription_truncate(
        &mut self,
        txn: &mut TxnState,
        tables: &[usize],
        cascade: bool,
        restart_identity: bool,
    ) -> Result<(), SqlError> {
        exec::apply_replication_truncate(&mut self.storage, txn, tables, cascade, restart_identity)
    }

    pub(crate) fn coerce_parameter_null<'a>(
        &self,
        oid: i32,
        arena: &'a Arena,
        txid: u32,
    ) -> Result<Datum<'a>, SqlError> {
        exec::coerce_binary_input_null(&self.storage, oid, arena, txid)
    }

    pub(crate) fn decode_binary_parameter<'a>(
        &self,
        oid: i32,
        bytes: &'a [u8],
        arena: &'a Arena,
        txid: u32,
    ) -> Result<Datum<'a>, SqlError> {
        exec::decode_binary_input(&self.storage, oid, bytes, arena, txid)
    }

    pub(crate) fn decode_text_parameter<'a>(
        &self,
        oid: i32,
        bytes: &'a [u8],
        arena: &'a Arena,
        txid: u32,
    ) -> Result<Datum<'a>, SqlError> {
        exec::decode_text_input(&self.storage, oid, bytes, arena, txid)
    }

    pub(crate) fn role_login(&self, name: &str) -> Option<RoleLogin> {
        let slot = self.storage.find_role(name)?;
        let attributes = self.storage.role(slot).attributes;
        let valid = !attributes.has_valid_until
            || attributes
                .valid_until
                .as_str()
                .eq_ignore_ascii_case("infinity")
            || crate::sql::datetime::parse_timestamp(attributes.valid_until.as_str(), true)
                .is_ok_and(|deadline| deadline >= crate::sql::datetime::now_micros());
        Some(RoleLogin {
            slot: slot as u16,
            can_login: attributes.can_login,
            valid,
            superuser: attributes.superuser,
            replication: attributes.replication,
            connection_limit: attributes.connection_limit,
            password: attributes.has_password.then_some(attributes.password),
        })
    }

    pub(crate) fn reserve_role_connection(&mut self, login: RoleLogin) -> bool {
        let count = &mut self.role_connections[login.slot as usize];
        if !login.superuser
            && login.connection_limit >= 0
            && usize::from(*count) >= login.connection_limit as usize
        {
            return false;
        }
        let Some(next) = count.checked_add(1) else {
            return false;
        };
        *count = next;
        true
    }

    pub(crate) fn release_role_connection(&mut self, slot: u16) {
        let count = &mut self.role_connections[slot as usize];
        *count = count
            .checked_sub(1)
            .expect("an authenticated role connection is released once");
    }

    /// Whether one extended-protocol statement is COPY. Execute's `max_rows`
    /// applies only to row-returning portals; COPY has its own streaming
    /// protocol and must never be staged in the bounded portal buffer.
    pub fn is_copy_statement(&self, text: &str, arena: &Arena) -> bool {
        Parser::new(text, arena)
            .ok()
            .and_then(|mut parser| parser.next_stmt().ok().flatten())
            .is_some_and(|statement| matches!(statement, Stmt::Copy(_)))
    }

    /// Bytes drawn beyond the row heap, for the memory plan.
    pub fn extra_budget_bytes(config: &Config) -> usize {
        Storage::extra_budget_bytes(config)
            + config.table_rows * size_of::<(u64, RowHome)>()
            + (1 + crate::storage::MAX_PENDING_ROW_VERSIONS
                + crate::storage::MAX_COMMITTED_ROW_VERSIONS)
                * config.max_tables
                * config.table_rows
                * size_of::<(u32, u64, u8, RowLoc)>()
            + config.work_arena_bytes
            + config.wal_buffer_bytes
            + config.wal_upload_buffer_bytes.max(config.wal_buffer_bytes)
            + config.max_connections as usize * config.wal_buffer_bytes
            + if config.object_store_on {
                // The checkpointer's fixed parts plus the spilled-row reader's
                // two scratch sets.
                Checkpointer::budget_bytes(config) + crate::storage::SpillReader::budget_bytes()
            } else {
                0
            }
    }

    /// Builds storage, loads the latest checkpoint from object storage
    /// (when enabled), and replays the journal tail on top. Startup only.
    pub fn new(config: &Config, budget: &mut Budget) -> Result<Self, EngineSetupError> {
        crate::sql::tzif::init_catalog();
        let mut storage = Storage::new(config, budget)?;
        storage.configure_collation(config, budget)?;
        let mut ckpt = if config.object_store_on {
            Some(Checkpointer::new(config, budget)?)
        } else {
            None
        };
        // The spilled-row read path shares the checkpointer's block stack;
        // it must exist before the manifest load installs spilled rows.
        if let Some(c) = &ckpt {
            let reader = crate::storage::SpillReader::new(budget, c.block_stack())
                .map_err(EngineSetupError::Budget)?;
            storage.attach_spill(reader);
        }
        let floor = match &mut ckpt {
            Some(c) => c.load_into(&mut storage)?,
            None => 0,
        };
        let mut wal = Wal::open(config, budget)?;
        // Recovery merges two partial sources by LSN and applies the merge in
        // order. Neither source alone spans the committed history past the
        // manifest floor: the journal may restart mid-history (a disk wipe
        // recreates it holding only the newest commits) or end early (a torn
        // write), while the segments lack whatever a failed upload left
        // journaled-only. Applying the journal first and the segments second
        // would let an older segment image clobber a newer journaled one, so
        // both are collected and applied exactly once in LSN order — the two
        // copies of one record are the same bytes, so either wins.
        let mut recovered: std::collections::BTreeMap<u64, Vec<u8>> =
            std::collections::BTreeMap::new();
        let mut journal_tip = floor;
        wal.replay(floor, |lsn, record| {
            journal_tip = journal_tip.max(lsn);
            recovered.insert(lsn, record.to_vec());
            Ok(())
        })?;
        // RPO=0: merge any commit batches in the bucket newer than what the
        // local journal (possibly empty after disk loss) already covered.
        let mut segment_tip = floor;
        if let Some(c) = ckpt.as_mut() {
            c.replay_commit_batches(floor, |lsn, record| {
                segment_tip = segment_tip.max(lsn);
                recovered.entry(lsn).or_insert_with(|| record.to_vec());
                Ok(())
            })
            .map_err(EngineSetupError::Checkpoint)?;
        }
        for (lsn, record) in &recovered {
            let operator =
                crate::wal::decode_record(record).ok_or(EngineSetupError::Storage(SqlError {
                    sqlstate: sqlstate::INTERNAL_ERROR,
                    message: stack_format!(192, "corrupt uploaded WAL record"),
                }))?;
            apply_wal_op(&mut storage, *lsn, operator)?;
        }
        // Startup reconciliation makes every recovered commit durable in the
        // configured object store before the server admits any connection.
        if config.wal_upload
            && config.object_store_on
            && journal_tip > segment_tip
            && let Some(c) = ckpt.as_mut()
        {
            let mut segment: Vec<u8> = Vec::new();
            let mut first = 0u64;
            for (lsn, record) in recovered.range((segment_tip + 1)..=journal_tip) {
                if first == 0 {
                    first = *lsn;
                }
                let payload_len = (record.len() - 8) as u32;
                let mut body = Vec::with_capacity(8 + record.len());
                body.extend_from_slice(&payload_len.to_le_bytes());
                body.extend_from_slice(&lsn.to_le_bytes());
                body.extend_from_slice(record);
                let crc = crate::wal::crc32c::crc32c(&body);
                segment.extend_from_slice(&crc.to_le_bytes());
                segment.extend_from_slice(&body);
            }
            c.publish_commit_batch(first, &segment)?;
        }
        storage.ensure_no_pending_replay_table_rewrite()?;
        storage.reconcile_serials();
        // Replay's row installs bypass the per-row value-index maintenance, so
        // rebuild every table's uniqueness indexes from the recovered committed
        // rows before serving queries.
        storage.rebuild_all_enforcers()?;
        // The upload buffer must hold at least one full WAL batch.
        let upload_buf = config.wal_upload_buffer_bytes.max(config.wal_buffer_bytes);
        Ok(Self {
            storage,
            wal,
            ckpt,
            post_publish_cleanup: None,
            pending_copy: None,
            wal_upload: config.wal_upload && config.object_store_on,
            wal_seg_buf: Vec::with_capacity(upload_buf),
            scratch: FixedVec::new(budget, "scan_scratch", config.table_rows)?,
            compact_scratch: FixedVec::new(
                budget,
                "compact_scratch",
                (1 + crate::storage::MAX_PENDING_ROW_VERSIONS
                    + crate::storage::MAX_COMMITTED_ROW_VERSIONS)
                    * config.max_tables
                    * config.table_rows,
            )?,
            work: Arena::new(budget, "work_arena", config.work_arena_bytes)?,
            next_txid: 0,
            notify: notify::NotifyState::new(
                budget,
                config.max_connections as usize * notify::CHANNELS_PER_CONN,
                notify::OUTBOX,
            )?,
            current_conn_id: 0,
            replication_system_id: crate::object_store::writer_id(config),
            role_connections: [0; crate::storage::MAX_ROLES],
        })
    }

    pub(crate) fn replication_identity(&self) -> (u64, u64) {
        (self.replication_system_id, self.wal.last_lsn())
    }

    /// Creates a durable logical-replication resume point outside SQL
    /// transactions. Replication protocol commands have their own commit
    /// boundary, so this follows the same WAL-before-catalog order as a
    /// committed SQL transaction.
    pub(crate) fn create_replication_slot(
        &mut self,
        name: crate::storage::SqlName,
    ) -> Result<u64, SqlError> {
        if self.storage.replication_slot(name.as_str()).is_some() {
            return Err(sql_err!(
                sqlstate::DUPLICATE_OBJECT,
                "replication slot \"{}\" already exists",
                name.as_str()
            ));
        }
        if self.storage.replication_slots_with_slots().count()
            == self.storage.replication_slot_capacity()
        {
            return Err(sql_err!(
                sqlstate::PROGRAM_LIMIT_EXCEEDED,
                "too many replication slots"
            ));
        }
        self.next_txid = self.next_txid.wrapping_add(1).max(1);
        let transaction_id = self.next_txid;
        let restart_lsn =
            self.storage.lsn().checked_add(1).ok_or_else(|| {
                sql_err!(sqlstate::PROGRAM_LIMIT_EXCEEDED, "WAL LSN space exhausted")
            })?;
        if let Err(error) = self.wal.stage(
            transaction_id,
            restart_lsn,
            &WalOp::CreateReplicationSlot {
                name: name.as_str(),
                restart_lsn,
            },
        ) {
            self.wal.discard_stage(transaction_id);
            return Err(error);
        }
        let commit_lsn = match self.wal.commit_stage(transaction_id, self.storage.lsn()) {
            Ok(lsn) => lsn,
            Err(error) => {
                self.wal.discard_stage(transaction_id);
                return Err(error);
            }
        };
        self.wal.commit();
        self.storage.create_replication_slot(name, restart_lsn)?;
        self.storage.set_lsn(commit_lsn);
        Ok(restart_lsn)
    }

    pub(crate) fn drop_replication_slot(
        &mut self,
        name: crate::storage::SqlName,
    ) -> Result<(), SqlError> {
        let slot = self
            .storage
            .replication_slot(name.as_str())
            .ok_or_else(|| {
                sql_err!(
                    sqlstate::UNDEFINED_OBJECT,
                    "replication slot \"{}\" does not exist",
                    name.as_str()
                )
            })?;
        if slot.active {
            return Err(sql_err!(
                sqlstate::OBJECT_NOT_IN_PREREQUISITE_STATE,
                "replication slot \"{}\" is active",
                name.as_str()
            ));
        }
        self.next_txid = self.next_txid.wrapping_add(1).max(1);
        let transaction_id = self.next_txid;
        let lsn =
            self.storage.lsn().checked_add(1).ok_or_else(|| {
                sql_err!(sqlstate::PROGRAM_LIMIT_EXCEEDED, "WAL LSN space exhausted")
            })?;
        if let Err(error) = self.wal.stage(
            transaction_id,
            lsn,
            &WalOp::DropReplicationSlot {
                name: name.as_str(),
            },
        ) {
            self.wal.discard_stage(transaction_id);
            return Err(error);
        }
        let commit_lsn = match self.wal.commit_stage(transaction_id, self.storage.lsn()) {
            Ok(lsn) => lsn,
            Err(error) => {
                self.wal.discard_stage(transaction_id);
                return Err(error);
            }
        };
        self.wal.commit();
        self.storage.drop_replication_slot(name.as_str())?;
        self.storage.set_lsn(commit_lsn);
        Ok(())
    }

    pub(crate) fn activate_replication_slot(&mut self, name: &str) -> Result<u64, SqlError> {
        self.storage.activate_replication_slot(name)
    }

    pub(crate) fn deactivate_replication_slot(&mut self, name: &str) {
        self.storage.deactivate_replication_slot(name);
    }

    pub(crate) fn advance_replication_slot(
        &mut self,
        name: &str,
        confirmed_flush_lsn: u64,
    ) -> Result<(), SqlError> {
        let advance = self
            .storage
            .prepare_replication_slot_advance(name, confirmed_flush_lsn)?;
        self.next_txid = self.next_txid.wrapping_add(1).max(1);
        let transaction_id = self.next_txid;
        let lsn =
            self.storage.lsn().checked_add(1).ok_or_else(|| {
                sql_err!(sqlstate::PROGRAM_LIMIT_EXCEEDED, "WAL LSN space exhausted")
            })?;
        self.wal.stage(
            transaction_id,
            lsn,
            &WalOp::AdvanceReplicationSlot {
                name: advance.name(),
                confirmed_flush_lsn,
            },
        )?;
        let commit_lsn = match self.wal.commit_stage(transaction_id, self.storage.lsn()) {
            Ok(lsn) => lsn,
            Err(error) => {
                self.wal.discard_stage(transaction_id);
                return Err(error);
            }
        };
        self.wal.commit();
        self.storage.apply_replication_slot_advance(advance);
        self.storage.set_lsn(commit_lsn);
        Ok(())
    }

    /// Emits the next complete committed transaction selected by one logical
    /// publication. The cursor advances only after every CopyData frame for
    /// that transaction fitted in the caller's fixed send buffer.
    pub(crate) fn emit_replication_transaction(
        &mut self,
        floor: u64,
        publication_names: &[SqlName],
        binary: bool,
        proto_version: crate::pg::pgoutput::ProtocolVersion,
        scratch: &mut FixedBuf,
        responder: &mut Responder,
    ) -> Result<Option<(u64, bool)>, SqlError> {
        let storage = &self.storage;
        for name in publication_names {
            if storage.publication(name.as_str()).is_none() {
                return Err(sql_err!(
                    sqlstate::UNDEFINED_OBJECT,
                    "publication \"{}\" does not exist",
                    name.as_str()
                ));
            }
        }
        let mut emitted = false;
        let mut encode = |end_lsn, transaction: &[u8]| {
            let mut at = 0usize;
            let mut transaction_id = 0u32;
            let mut truncates = [PendingTruncate {
                command_id: 0,
                table_slots: [0; crate::sql::txn::MAX_TRUNCATE_TABLES],
                table_count: 0,
                cascade: false,
                restart_identity: false,
                emitted: false,
            }; crate::sql::txn::MAX_TXN_DDL];
            let mut truncate_count = 0usize;
            while at < transaction.len() {
                let length =
                    u32::from_le_bytes(transaction[at + 4..at + 8].try_into().unwrap()) as usize;
                let total = crate::wal::HEADER_LEN + length;
                if let Some(WalOp::Commit { transaction_id: id }) =
                    crate::wal::decode_record(&transaction[at + 16..at + total])
                {
                    transaction_id = id;
                }
                if let Some(WalOp::Truncate {
                    tables,
                    table_count,
                    cascade,
                    restart_identity,
                    command_id,
                }) = crate::wal::decode_record(&transaction[at + 16..at + total])
                {
                    if truncate_count == truncates.len() {
                        return Err(sql_err!(
                            sqlstate::PROGRAM_LIMIT_EXCEEDED,
                            "replication transaction contains too many TRUNCATE commands"
                        ));
                    }
                    let mut table_slots = [0_u16; crate::sql::txn::MAX_TRUNCATE_TABLES];
                    let mut table_at = 0usize;
                    for table_slot in &mut table_slots[..table_count] {
                        let schema_length = *tables.get(table_at).ok_or_else(|| {
                            sql_err!(sqlstate::PROTOCOL_VIOLATION, "corrupt truncate WAL schema")
                        })? as usize;
                        table_at += 1;
                        let schema = core::str::from_utf8(
                            tables
                                .get(table_at..table_at + schema_length)
                                .ok_or_else(|| {
                                    sql_err!(
                                        sqlstate::PROTOCOL_VIOLATION,
                                        "corrupt truncate WAL schema"
                                    )
                                })?,
                        )
                        .map_err(|_| {
                            sql_err!(sqlstate::PROTOCOL_VIOLATION, "corrupt truncate WAL schema")
                        })?;
                        table_at += schema_length;
                        let table_length = *tables.get(table_at).ok_or_else(|| {
                            sql_err!(sqlstate::PROTOCOL_VIOLATION, "corrupt truncate WAL table")
                        })? as usize;
                        table_at += 1;
                        let table = core::str::from_utf8(
                            tables
                                .get(table_at..table_at + table_length)
                                .ok_or_else(|| {
                                    sql_err!(
                                        sqlstate::PROTOCOL_VIOLATION,
                                        "corrupt truncate WAL table"
                                    )
                                })?,
                        )
                        .map_err(|_| {
                            sql_err!(sqlstate::PROTOCOL_VIOLATION, "corrupt truncate WAL table")
                        })?;
                        table_at += table_length;
                        let Some(found_table_slot) = storage.find_table(schema, table) else {
                            return Err(sql_err!(
                                sqlstate::UNDEFINED_TABLE,
                                "replication WAL refers to unknown table \"{}\"",
                                table
                            ));
                        };
                        *table_slot = found_table_slot as u16;
                    }
                    if table_at != tables.len() {
                        return Err(sql_err!(
                            sqlstate::PROTOCOL_VIOLATION,
                            "corrupt truncate WAL table list"
                        ));
                    }
                    truncates[truncate_count] = PendingTruncate {
                        command_id,
                        table_slots,
                        table_count,
                        cascade,
                        restart_identity,
                        emitted: false,
                    };
                    truncate_count += 1;
                }
                at += total;
            }
            // A pgoutput transaction exists only when at least one operation
            // selected by the publication union survives statement-level
            // TRUNCATE suppression. Catalog and slot WAL stay durable but do
            // not manufacture an empty subscriber transaction.
            let mut publication_change = false;
            for truncate in &truncates[..truncate_count] {
                for table_slot in &truncate.table_slots[..truncate.table_count] {
                    if publication_selects(
                        storage,
                        publication_names,
                        *table_slot as usize,
                        PublicationOperation::Truncate,
                    )? {
                        publication_change = true;
                        break;
                    }
                }
                if publication_change {
                    break;
                }
            }
            at = 0;
            while !publication_change && at < transaction.len() {
                let length =
                    u32::from_le_bytes(transaction[at + 4..at + 8].try_into().unwrap()) as usize;
                let total = crate::wal::HEADER_LEN + length;
                let operation = crate::wal::decode_record(&transaction[at + 16..at + total])
                    .ok_or_else(|| {
                        sql_err!(sqlstate::PROTOCOL_VIOLATION, "corrupt committed WAL record")
                    })?;
                publication_change = match operation {
                    WalOp::Upsert {
                        schema,
                        table,
                        is_update,
                        ..
                    } => {
                        let table_slot = storage.find_table(schema, table).ok_or_else(|| {
                            sql_err!(
                                sqlstate::UNDEFINED_TABLE,
                                "replication WAL refers to unknown table \"{}\"",
                                table
                            )
                        })?;
                        publication_selects(
                            storage,
                            publication_names,
                            table_slot,
                            if is_update {
                                PublicationOperation::Update
                            } else {
                                PublicationOperation::Insert
                            },
                        )?
                    }
                    WalOp::Delete {
                        schema,
                        table,
                        command_id,
                        ..
                    } => {
                        let table_slot = storage.find_table(schema, table).ok_or_else(|| {
                            sql_err!(
                                sqlstate::UNDEFINED_TABLE,
                                "replication WAL refers to unknown table \"{}\"",
                                table
                            )
                        })?;
                        let suppressed_by_truncate =
                            truncates[..truncate_count].iter().any(|truncate| {
                                truncate.command_id >= command_id
                                    && truncate.table_slots[..truncate.table_count]
                                        .contains(&(table_slot as u16))
                            });
                        !suppressed_by_truncate
                            && publication_selects(
                                storage,
                                publication_names,
                                table_slot,
                                PublicationOperation::Delete,
                            )?
                    }
                    _ => false,
                };
                at += total;
            }
            if !publication_change {
                return Ok(());
            }
            emitted = true;
            let overflow = || {
                sql_err!(
                    sqlstate::PROGRAM_LIMIT_EXCEEDED,
                    "replication transaction exceeds connection send buffer"
                )
            };
            responder
                .copy_data(&|message| {
                    pgoutput::xlog_data(message, floor, end_lsn, |plugin| {
                        pgoutput::begin(plugin, end_lsn, transaction_id)
                    })
                })
                .map_err(|_| overflow())?;
            at = 0;
            while at < transaction.len() {
                let length =
                    u32::from_le_bytes(transaction[at + 4..at + 8].try_into().unwrap()) as usize;
                let total = crate::wal::HEADER_LEN + length;
                let lsn = u64::from_le_bytes(transaction[at + 8..at + 16].try_into().unwrap());
                let operation = crate::wal::decode_record(&transaction[at + 16..at + total])
                    .ok_or_else(|| {
                        sql_err!(sqlstate::PROTOCOL_VIOLATION, "corrupt committed WAL record")
                    })?;
                match operation {
                    WalOp::Upsert {
                        schema,
                        table,
                        row,
                        is_update,
                        old_row,
                        command_id,
                        ..
                    } => {
                        emit_pending_truncates(
                            storage,
                            publication_names,
                            proto_version,
                            end_lsn,
                            command_id,
                            &mut truncates[..truncate_count],
                            responder,
                        )?;
                        let Some(table_slot) = storage.find_table(schema, table) else {
                            return Err(sql_err!(
                                sqlstate::UNDEFINED_TABLE,
                                "replication WAL refers to unknown table \"{}\"",
                                table
                            ));
                        };
                        if publication_selects(
                            storage,
                            publication_names,
                            table_slot,
                            if is_update {
                                PublicationOperation::Update
                            } else {
                                PublicationOperation::Insert
                            },
                        )? {
                            let definition = storage.table_def(table_slot, 0);
                            let mut schema_types = [ColType::Bool; crate::storage::MAX_COLUMNS];
                            let column_count = definition.schema(&mut schema_types);
                            let mut values = [Datum::Null; crate::storage::MAX_COLUMNS];
                            crate::storage::rowenc::decode(
                                row,
                                &schema_types[..column_count],
                                &mut values,
                            )?;
                            let relation_id = table_slot as u32 + 1;
                            emit_replication_relation(
                                storage,
                                definition,
                                relation_id,
                                responder,
                                end_lsn,
                            )?;
                            if is_update {
                                let old = old_row.ok_or_else(|| {
                                    sql_err!(
                                        sqlstate::PROTOCOL_VIOLATION,
                                        "update WAL record lacks replica identity"
                                    )
                                })?;
                                let mut old_values = [Datum::Null; crate::storage::MAX_COLUMNS];
                                crate::storage::rowenc::decode(
                                    old,
                                    &schema_types[..column_count],
                                    &mut old_values,
                                )?;
                                responder
                                    .copy_data(&|message| {
                                        pgoutput::xlog_data(message, lsn, end_lsn, |plugin| {
                                            pgoutput::update(
                                                plugin,
                                                relation_id,
                                                &old_values[..column_count],
                                                &values[..column_count],
                                                binary,
                                            )
                                        })
                                    })
                                    .map_err(|_| overflow())?;
                            } else {
                                responder
                                    .copy_data(&|message| {
                                        pgoutput::xlog_data(message, lsn, end_lsn, |plugin| {
                                            pgoutput::insert(
                                                plugin,
                                                relation_id,
                                                &values[..column_count],
                                                binary,
                                            )
                                        })
                                    })
                                    .map_err(|_| overflow())?;
                            }
                        }
                    }
                    WalOp::Delete {
                        schema,
                        table,
                        old_row,
                        command_id,
                        ..
                    } => {
                        let Some(table_slot) = storage.find_table(schema, table) else {
                            return Err(sql_err!(
                                sqlstate::UNDEFINED_TABLE,
                                "replication WAL refers to unknown table \"{}\"",
                                table
                            ));
                        };
                        let suppressed_by_truncate =
                            truncates[..truncate_count].iter().any(|truncate| {
                                truncate.command_id >= command_id
                                    && truncate.table_slots[..truncate.table_count]
                                        .contains(&(table_slot as u16))
                            });
                        if publication_selects(
                            storage,
                            publication_names,
                            table_slot,
                            PublicationOperation::Delete,
                        )? && !suppressed_by_truncate
                        {
                            emit_pending_truncates(
                                storage,
                                publication_names,
                                proto_version,
                                end_lsn,
                                command_id,
                                &mut truncates[..truncate_count],
                                responder,
                            )?;
                            let old = old_row.ok_or_else(|| {
                                sql_err!(
                                    sqlstate::PROTOCOL_VIOLATION,
                                    "delete WAL record lacks replica identity"
                                )
                            })?;
                            let definition = storage.table_def(table_slot, 0);
                            let mut schema_types = [ColType::Bool; crate::storage::MAX_COLUMNS];
                            let column_count = definition.schema(&mut schema_types);
                            let mut values = [Datum::Null; crate::storage::MAX_COLUMNS];
                            crate::storage::rowenc::decode(
                                old,
                                &schema_types[..column_count],
                                &mut values,
                            )?;
                            let relation_id = table_slot as u32 + 1;
                            emit_replication_relation(
                                storage,
                                definition,
                                relation_id,
                                responder,
                                end_lsn,
                            )?;
                            responder
                                .copy_data(&|message| {
                                    pgoutput::xlog_data(message, lsn, end_lsn, |plugin| {
                                        pgoutput::delete(
                                            plugin,
                                            relation_id,
                                            &values[..column_count],
                                            binary,
                                        )
                                    })
                                })
                                .map_err(|_| overflow())?;
                        }
                    }
                    WalOp::Truncate { .. } => {}
                    _ => {}
                }
                at += total;
            }
            emit_pending_truncates(
                storage,
                publication_names,
                proto_version,
                end_lsn,
                u32::MAX,
                &mut truncates[..truncate_count],
                responder,
            )?;
            responder
                .copy_data(&|message| {
                    pgoutput::xlog_data(message, end_lsn, end_lsn, |plugin| {
                        pgoutput::commit(plugin, end_lsn)
                    })
                })
                .map_err(|_| overflow())?;
            Ok(())
        };
        if let Some(checkpointer) = self.ckpt.as_mut() {
            // The checkpointer owns the object client; its serving cursor is
            // read-only and uses its startup-reserved segment-list scratch.
            if let Some(lsn) =
                checkpointer.next_committed_wal_transaction(floor, scratch, &mut encode)?
            {
                return Ok(Some((lsn, emitted)));
            }
        }
        Ok(self
            .wal
            .next_committed_after(floor, scratch, encode)?
            .map(|lsn| (lsn, emitted)))
    }

    /// Starts a transaction if none is active.
    fn ensure_txn(&mut self, txn: &mut TxnState, mode: TxnMode, guc: &GucState) {
        if txn.is_active() {
            if mode == TxnMode::Explicit {
                txn.mode = TxnMode::Explicit;
            }
            return;
        }
        self.next_txid = self.next_txid.wrapping_add(1).max(1);
        txn.txid = self.next_txid;
        txn.mode = mode;
        datetime::begin_transaction();
        guc.begin_transaction();
        txn.failed = false;
    }

    /// Commits: journals every touched row, fsyncs once, then promotes the
    /// in-memory images. On failure the transaction rolls back entirely.
    pub fn commit_txn(&mut self, txn: &mut TxnState, guc: &GucState) -> Result<(), SqlError> {
        // The next statement starts a fresh transaction clock.
        datetime::end_transaction();
        if !txn.is_active() {
            return Ok(());
        }
        if txn.isolation == IsolationLevel::Serializable
            && (!txn.touched().is_empty() || !txn.ddl().is_empty())
            && let Err(error) = self.storage.validate_serializable(txn.txid)
        {
            self.rollback_txn(txn, guc);
            return Err(error);
        }
        // This transaction no longer needs its historical view. Release it
        // before promotion so only other live snapshots cause old row images
        // to be retained.
        self.storage.release_snapshot(txn.txid);
        self.storage.release_serializable(txn.txid);
        self.storage.release_table_locks(txn.txid);
        self.storage.release_row_locks(txn.txid);
        for event_index in 0..txn.truncates().len() {
            let event = txn.truncates()[event_index];
            let transaction_id = txn.txid;
            let tables = txn.truncate_wal_tables();
            tables.clear();
            for &table_slot in &event.tables[..event.table_count] {
                let definition = self.storage.table_def(table_slot as usize, transaction_id);
                for name in [definition.schema.as_str(), definition.name.as_str()] {
                    assert!(tables.append(&[name.len() as u8]) && tables.append(name.as_bytes()));
                }
            }
            let lsn = self.storage.lsn() + 1;
            if let Err(error) = self.wal.stage(
                transaction_id,
                lsn,
                &WalOp::Truncate {
                    tables: tables.readable(),
                    table_count: event.table_count,
                    cascade: event.cascade,
                    restart_identity: event.restart_identity,
                    command_id: event.command_id,
                },
            ) {
                self.rollback_txn(txn, guc);
                return Err(error);
            }
            self.storage.set_lsn(lsn);
        }
        for i in 0..txn.touched().len() {
            let (table, rowid, _) = txn.touched()[i];
            // A row may be written several times in one transaction; journal
            // its final committed image once.
            if txn.touched()[..i]
                .iter()
                .any(|&(t, r, _)| t == table && r == rowid)
            {
                continue;
            }
            let Some(state) = self.storage.row_state(table as usize, rowid)? else {
                continue;
            };
            let Some(p) = state.pending.last() else {
                continue;
            };
            let t = self.storage.table(table as usize);
            if p.txid != txn.txid || !t.visible_to(txn.txid) {
                continue;
            }
            let def = self.storage.table_def(table as usize, txn.txid);
            let name = def.name;
            let schema = def.schema;
            let lsn = self.storage.lsn() + 1;
            let appended = match (p.loc, state.committed) {
                (Some(loc), Some(old_home)) => {
                    self.storage
                        .with_row_bytes(table as usize, rowid, old_home, |old_row| {
                            self.wal.stage(
                                txn.txid,
                                lsn,
                                &WalOp::Upsert {
                                    schema: schema.as_str(),
                                    table: name.as_str(),
                                    rowid,
                                    row: self.storage.heap.get(loc),
                                    is_update: true,
                                    old_row: Some(old_row),
                                    command_id: p.cid,
                                },
                            )
                        })
                }
                (Some(loc), None) => self.wal.stage(
                    txn.txid,
                    lsn,
                    &WalOp::Upsert {
                        schema: schema.as_str(),
                        table: name.as_str(),
                        rowid,
                        row: self.storage.heap.get(loc),
                        is_update: false,
                        old_row: None,
                        command_id: p.cid,
                    },
                ),
                (None, Some(old_home)) => {
                    self.storage
                        .with_row_bytes(table as usize, rowid, old_home, |old_row| {
                            self.wal.stage(
                                txn.txid,
                                lsn,
                                &WalOp::Delete {
                                    schema: schema.as_str(),
                                    table: name.as_str(),
                                    rowid,
                                    old_row: Some(old_row),
                                    command_id: p.cid,
                                },
                            )
                        })
                }
                (None, None) => continue,
            };
            if let Err(e) = appended {
                self.rollback_txn(txn, guc);
                return Err(e);
            }
            self.storage.set_lsn(lsn);
        }
        // Journal any sequence advances (this transaction's or ones a
        // rolled-back transaction left dirty): absolute positions, so replay
        // is idempotent.
        for i in 0..self.storage.table_count() {
            if !self.storage.table(i).serial_dirty || !self.storage.table(i).visible_to(txn.txid) {
                continue;
            }
            let def = *self.storage.table_def(i, txn.txid);
            let name = def.name;
            let schema = def.schema;
            for c in 0..def.n_columns {
                if !def.columns()[c].auto_increment {
                    continue;
                }
                let last = self.storage.table(i).serial_last[c];
                let lsn = self.storage.lsn() + 1;
                if let Err(e) = self.wal.stage(
                    txn.txid,
                    lsn,
                    &WalOp::SequenceSet {
                        schema: schema.as_str(),
                        table: name.as_str(),
                        column: c as u16,
                        last,
                    },
                ) {
                    self.rollback_txn(txn, guc);
                    return Err(e);
                }
                self.storage.set_lsn(lsn);
            }
        }
        // Journal sequence advances (this transaction's or ones a rolled-back
        // transaction left dirty). Absolute positions, like serial advances, and
        // deliberately non-transactional: a `nextval` in a rolled-back
        // transaction still consumes its number, matching PostgreSQL's gaps.
        for i in 0..self.storage.sequence_count() {
            let seq = self.storage.sequence_for(i, txn.txid);
            if !seq.visible_to(txn.txid) || !self.storage.sequence_value_dirty_for(i, txn.txid) {
                continue;
            }
            let schema = seq.schema;
            let name = seq.name;
            let (last, is_called) = self.storage.sequence_value_for(i, txn.txid);
            let lsn = self.storage.lsn() + 1;
            if let Err(e) = self.wal.stage(
                txn.txid,
                lsn,
                &WalOp::SequenceAdvance {
                    schema: schema.as_str(),
                    name: name.as_str(),
                    last,
                    is_called,
                },
            ) {
                self.rollback_txn(txn, guc);
                return Err(e);
            }
            self.storage.set_lsn(lsn);
        }
        // pg_statistic column rows are transactional, but PostgreSQL updates
        // pg_class reltuples/relpages in place. A later commit therefore also
        // journals relation statistics left dirty by a rolled-back ANALYZE.
        // In either case the final image crosses the same provider-neutral
        // WAL/object-store durability boundary as every other catalog change.
        for slot in 0..self.storage.table_count() {
            if !self.storage.table(slot).visible_to(txn.txid) {
                continue;
            }
            let pending = self.storage.pending_table_statistics(slot, txn.txid);
            if pending.is_none() && !self.storage.statistics_wal_dirty(slot) {
                continue;
            }
            let statistics =
                pending.unwrap_or_else(|| self.storage.table_statistics(slot, txn.txid));
            let definition = *self.storage.table_def(slot, txn.txid);
            let lsn = self.storage.lsn() + 1;
            if let Err(error) = self.wal.stage(
                txn.txid,
                lsn,
                &WalOp::Analyze {
                    schema: definition.schema.as_str(),
                    table: definition.name.as_str(),
                    statistics: crate::wal::WalTableStatistics::Captured(&statistics),
                },
            ) {
                self.rollback_txn(txn, guc);
                return Err(error);
            }
            self.storage.set_lsn(lsn);
        }
        // Ownership and ACLs are absolute catalog images. They are staged at
        // commit from the transaction-visible overlays, so repeated GRANT,
        // REVOKE, ALTER OWNER, and savepoint rollback publish exactly one
        // final record per object/ACL slot.
        for (position, undo) in txn.ddl().iter().enumerate() {
            let object = match *undo {
                DdlUndo::Created(slot) => Some(crate::storage::AccessObject {
                    class: crate::storage::AccessClass::Table,
                    slot: slot as u16,
                }),
                DdlUndo::ViewCreated(slot) => Some(crate::storage::AccessObject {
                    class: crate::storage::AccessClass::View,
                    slot: slot as u16,
                }),
                DdlUndo::MatviewCreated(slot) => Some(crate::storage::AccessObject {
                    class: crate::storage::AccessClass::MaterializedView,
                    slot: slot as u16,
                }),
                DdlUndo::SequenceCreated(slot) => Some(crate::storage::AccessObject {
                    class: crate::storage::AccessClass::Sequence,
                    slot: slot as u16,
                }),
                DdlUndo::DomainCreated(slot) => Some(crate::storage::AccessObject {
                    class: crate::storage::AccessClass::Domain,
                    slot: slot as u16,
                }),
                DdlUndo::EnumCreated(slot) => Some(crate::storage::AccessObject {
                    class: crate::storage::AccessClass::Enum,
                    slot: slot as u16,
                }),
                DdlUndo::IndexCreated(slot) => Some(crate::storage::AccessObject {
                    class: crate::storage::AccessClass::Index,
                    slot: slot as u16,
                }),
                DdlUndo::SchemaCreated(slot) => Some(crate::storage::AccessObject {
                    class: crate::storage::AccessClass::Schema,
                    slot: slot as u16,
                }),
                DdlUndo::ObjectOwnerChanged { object, .. } => Some(object),
                _ => None,
            };
            let Some(object) = object else {
                continue;
            };
            if !self.storage.access_object_visible_to(object, txn.txid) {
                continue;
            }
            if txn.ddl()[position + 1..].iter().any(|later| {
                matches!(
                    later,
                    DdlUndo::ObjectOwnerChanged {
                        object: later_object,
                        ..
                    } if *later_object == object
                )
            }) {
                continue;
            }
            let (schema, name) = self.storage.access_object_name_to(object, txn.txid);
            let owner = self.storage.object_owner(object, txn.txid);
            let owner_name = self.storage.role_name(owner, txn.txid);
            let lsn = self.storage.lsn() + 1;
            if let Err(error) = self.wal.stage(
                txn.txid,
                lsn,
                &WalOp::SetObjectOwner {
                    class: object.class as u8,
                    object_oid: if object.class == crate::storage::AccessClass::Routine {
                        crate::storage::routine_oid(self.storage.routine(object.slot as usize))
                    } else {
                        0
                    },
                    schema: schema.as_str(),
                    name: name.as_str(),
                    owner: owner_name.as_str(),
                },
            ) {
                self.rollback_txn(txn, guc);
                return Err(error);
            }
            self.storage.set_lsn(lsn);
        }
        for (position, undo) in txn.ddl().iter().enumerate() {
            let DdlUndo::ObjectAclChanged { slot, .. } = *undo else {
                continue;
            };
            if txn.ddl()[position + 1..].iter().any(|later| {
                matches!(later, DdlUndo::ObjectAclChanged { slot: later, .. } if *later == slot)
            }) {
                continue;
            }
            let entry = self.storage.acl_entry(slot as usize);
            let object = entry.object;
            if !self.storage.access_object_visible_to(object, txn.txid) {
                continue;
            }
            let (grantee, grantor) = self.storage.acl_identity(slot as usize, txn.txid);
            if txn.ddl()[..position].iter().any(|earlier| {
                let DdlUndo::ObjectAclChanged {
                    slot: earlier_slot, ..
                } = *earlier
                else {
                    return false;
                };
                if earlier_slot == slot {
                    return false;
                }
                let earlier_entry = self.storage.acl_entry(earlier_slot as usize);
                earlier_entry.object == object
                    && self.storage.acl_identity(earlier_slot as usize, txn.txid)
                        == (grantee, grantor)
            }) {
                continue;
            }
            let (privileges, grant_options) =
                self.storage.acl_from(object, grantee, grantor, txn.txid);
            let (schema, name) = self.storage.access_object_name_to(object, txn.txid);
            let grantee_name = (grantee != crate::storage::PUBLIC_ROLE)
                .then(|| self.storage.role_name(grantee as usize, txn.txid));
            let grantor_name = self.storage.role_name(grantor as usize, txn.txid);
            let lsn = self.storage.lsn() + 1;
            if let Err(error) = self.wal.stage(
                txn.txid,
                lsn,
                &WalOp::SetObjectAcl {
                    class: object.class as u8,
                    object_oid: if object.class == crate::storage::AccessClass::Routine {
                        crate::storage::routine_oid(self.storage.routine(object.slot as usize))
                    } else {
                        0
                    },
                    schema: schema.as_str(),
                    name: name.as_str(),
                    grantee: grantee_name.as_ref().map_or("PUBLIC", |role| role.as_str()),
                    grantor: grantor_name.as_str(),
                    privileges,
                    grant_options,
                },
            ) {
                self.rollback_txn(txn, guc);
                return Err(error);
            }
            self.storage.set_lsn(lsn);
        }
        for (position, undo) in txn.ddl().iter().enumerate() {
            let DdlUndo::DefaultAclChanged { slot, .. } = *undo else {
                continue;
            };
            if txn.ddl()[position + 1..].iter().any(
                |later| matches!(later, DdlUndo::DefaultAclChanged { slot: later, .. } if *later == slot),
            ) {
                continue;
            }
            let entry = *self.storage.default_acl_entry(slot as usize);
            let (defined, privileges, grant_options) = self.storage.default_acl_state(
                entry.owner,
                entry.schema,
                entry.class,
                entry.grantee,
                txn.txid,
            );
            let owner = self.storage.role_name(entry.owner as usize, txn.txid);
            let schema = if entry.schema == crate::storage::DEFAULT_ACL_ALL_SCHEMAS {
                crate::storage::SqlName::EMPTY
            } else {
                self.storage.schema_def(entry.schema as usize).name
            };
            let grantee = (entry.grantee != crate::storage::PUBLIC_ROLE)
                .then(|| self.storage.role_name(entry.grantee as usize, txn.txid));
            let lsn = self.storage.lsn() + 1;
            if let Err(error) = self.wal.stage(
                txn.txid,
                lsn,
                &WalOp::SetDefaultAcl {
                    owner: owner.as_str(),
                    schema: schema.as_str(),
                    class: entry.class as u8,
                    grantee: grantee.as_ref().map_or("PUBLIC", |role| role.as_str()),
                    defined,
                    privileges,
                    grant_options,
                },
            ) {
                self.rollback_txn(txn, guc);
                return Err(error);
            }
            self.storage.set_lsn(lsn);
        }
        for advance in txn.subscription_advances() {
            let lsn = self.storage.lsn() + 1;
            if let Err(error) = self.wal.stage(
                txn.txid,
                lsn,
                &WalOp::AdvanceSubscription {
                    name: advance.name(),
                    confirmed_lsn: advance.confirmed_lsn(),
                },
            ) {
                self.rollback_txn(txn, guc);
                return Err(error);
            }
            self.storage.set_lsn(lsn);
        }
        // Keep the publication object inside its startup-reserved buffer.
        // A full preceding batch is published before adding this transaction;
        // therefore a single transaction is the largest unpublishable unit.
        let (_, staged_bytes) = self.wal.stage_stats(txn.txid);
        let next_batch_bytes = self
            .wal
            .pending_batch_bytes()
            .saturating_add(staged_bytes)
            .saturating_add(crate::wal::HEADER_LEN as u64);
        if next_batch_bytes > self.wal_seg_buf.capacity() as u64
            && let Err(error) = self.commit_wal()
        {
            self.rollback_txn(txn, guc);
            return Err(error);
        }
        let commit_lsn = match self.wal.commit_stage(txn.txid, self.storage.lsn()) {
            Ok(lsn) => lsn,
            Err(error) => {
                self.rollback_txn(txn, guc);
                return Err(error);
            }
        };
        self.storage.set_lsn(commit_lsn);
        // Publication accepted every staged absolute sequence position.
        // Clear retry markers only now: a staging or journal-capacity error
        // rolls the transaction back but PostgreSQL sequence advances remain
        // nontransactional and must be journaled by a later commit.
        for i in 0..self.storage.table_count() {
            if self.storage.table(i).visible_to(txn.txid) {
                self.storage.table_mut(i).serial_dirty = false;
            }
        }
        for i in 0..self.storage.sequence_count() {
            let sequence = self.storage.sequence(i);
            if sequence.visible_to(txn.txid) {
                self.storage.clear_sequence_value_dirty(i, txn.txid);
            }
        }
        for slot in 0..self.storage.table_count() {
            if self.storage.table(slot).visible_to(txn.txid)
                && (self
                    .storage
                    .pending_table_statistics(slot, txn.txid)
                    .is_some()
                    || self.storage.statistics_wal_dirty(slot))
            {
                self.storage.clear_statistics_wal_dirty(slot);
            }
        }
        // The local journal makes the batch crash-recoverable while it waits
        // for the protocol publication barrier.  The connection releases no
        // successful response until that barrier has published the immutable
        // batch to object storage.
        self.wal.commit();
        let mut altered_tables = [(usize::MAX, false); txn::MAX_TXN_DDL];
        let mut altered_count = 0usize;
        let mut index_tables = [usize::MAX; txn::MAX_TXN_DDL];
        let mut index_table_count = 0usize;
        for undo in txn.ddl() {
            let DdlUndo::TableAltered(slot) = *undo else {
                continue;
            };
            let slot = slot as usize;
            if altered_tables[..altered_count]
                .iter()
                .any(|&(existing, _)| existing == slot)
            {
                continue;
            }
            let rewrote_rows = self.storage.commit_table_def(slot, txn.txid);
            altered_tables[altered_count] = (slot, rewrote_rows);
            altered_count += 1;
        }
        for &(table, rowid, _) in txn.touched() {
            let table = table as usize;
            if altered_tables[..altered_count]
                .iter()
                .any(|&(altered, _)| altered == table)
            {
                self.storage
                    .commit_rewritten_row(table, rowid, txn.txid, commit_lsn);
            } else {
                self.storage.commit_row(table, rowid, txn.txid, commit_lsn);
            }
        }
        for undo in txn.ddl() {
            match undo {
                // Promote the transaction's uncommitted DDL into the committed
                // catalog now that the journal is durable.
                DdlUndo::Created(slot) => {
                    self.storage.commit_create(*slot as usize);
                    self.storage.commit_object_owner(
                        crate::storage::AccessObject {
                            class: crate::storage::AccessClass::Table,
                            slot: *slot as u16,
                        },
                        txn.txid,
                    );
                }
                DdlUndo::Dropped(slot) => {
                    let name = self.storage.table(*slot as usize).def.name;
                    let schema = self.storage.table(*slot as usize).def.schema;
                    self.storage.commit_drop(*slot as usize);
                    self.storage.commit_triggers_for_table(*slot as usize);
                    // The table's indexes were pending-dropped with it.
                    self.storage
                        .commit_indexes_for(schema.as_str(), name.as_str(), txn.txid);
                }
                DdlUndo::TableAltered(_) => {}
                DdlUndo::ViewCreated(slot) => {
                    self.storage.commit_view_create(*slot as usize);
                    self.storage.commit_object_owner(
                        crate::storage::AccessObject {
                            class: crate::storage::AccessClass::View,
                            slot: *slot as u16,
                        },
                        txn.txid,
                    );
                }
                DdlUndo::ViewDropped(slot) => self.storage.commit_view_drop(*slot as usize),
                DdlUndo::RoutineCreated(slot) => {
                    self.storage.commit_routine_create(*slot as usize, txn.txid)
                }
                DdlUndo::RoutineDropped(slot) => self.storage.commit_routine_drop(*slot as usize),
                DdlUndo::TriggerCreated(slot) => self.storage.commit_trigger_create(*slot as usize),
                DdlUndo::TriggerDropped(slot) => self.storage.commit_trigger_drop(*slot as usize),
                DdlUndo::TriggerAltered { slot, .. } => {
                    self.storage.commit_trigger_alter(*slot as usize, txn.txid)
                }
                DdlUndo::RoutineIdentityAltered { slot, .. } => self
                    .storage
                    .commit_routine_identity(*slot as usize, txn.txid),
                DdlUndo::PublicationCreated(slot) => {
                    let slot = *slot as usize;
                    self.storage.commit_publication_create(slot);
                    self.storage.commit_publication_owner(slot, txn.txid);
                }
                DdlUndo::PublicationDropped(slot) => {
                    self.storage.commit_publication_drop(*slot as usize)
                }
                DdlUndo::PublicationAltered { slot, .. } => self
                    .storage
                    .commit_publication_alter(*slot as usize, txn.txid),
                DdlUndo::PublicationOwnerChanged { slot, .. } => self
                    .storage
                    .commit_publication_owner(*slot as usize, txn.txid),
                DdlUndo::PublicationRenamed { slot, .. } => self
                    .storage
                    .commit_publication_rename(*slot as usize, txn.txid),
                DdlUndo::SubscriptionCreated(slot) => {
                    self.storage.commit_subscription_create(*slot as usize)
                }
                DdlUndo::SubscriptionDropped(slot) => {
                    self.storage.commit_subscription_drop(*slot as usize)
                }
                DdlUndo::SubscriptionEnabled { slot, .. } => self
                    .storage
                    .commit_subscription_enabled(*slot as usize, txn.txid),
                DdlUndo::SubscriptionDefinitionChanged { slot, .. } => self
                    .storage
                    .commit_subscription_definition(*slot as usize, txn.txid),
                DdlUndo::MatviewCreated(slot) => {
                    self.storage.commit_matview_create(*slot as usize);
                    self.storage.commit_object_owner(
                        crate::storage::AccessObject {
                            class: crate::storage::AccessClass::MaterializedView,
                            slot: *slot as u16,
                        },
                        txn.txid,
                    );
                }
                DdlUndo::MatviewDropped(slot) => self.storage.commit_matview_drop(*slot as usize),
                DdlUndo::SequenceCreated(slot) => {
                    self.storage.commit_sequence_create(*slot as usize);
                    self.storage.commit_object_owner(
                        crate::storage::AccessObject {
                            class: crate::storage::AccessClass::Sequence,
                            slot: *slot as u16,
                        },
                        txn.txid,
                    );
                }
                DdlUndo::SequenceDropped(slot) => self.storage.commit_sequence_drop(*slot as usize),
                DdlUndo::SequenceAltered { slot, .. } => {
                    self.storage.commit_sequence_alter(*slot as usize, txn.txid)
                }
                DdlUndo::DomainCreated(slot) => {
                    self.storage.commit_domain_create(*slot as usize);
                    self.storage.commit_object_owner(
                        crate::storage::AccessObject {
                            class: crate::storage::AccessClass::Domain,
                            slot: *slot as u16,
                        },
                        txn.txid,
                    );
                }
                DdlUndo::DomainDropped(slot) => self.storage.commit_domain_drop(*slot as usize),
                DdlUndo::DomainAltered { slot, .. } => {
                    self.storage.commit_domain_alter(*slot as usize, txn.txid)
                }
                DdlUndo::EnumCreated(slot) => {
                    self.storage.commit_enum_create(*slot as usize);
                    self.storage.commit_object_owner(
                        crate::storage::AccessObject {
                            class: crate::storage::AccessClass::Enum,
                            slot: *slot as u16,
                        },
                        txn.txid,
                    );
                }
                DdlUndo::EnumDropped(slot) => self.storage.commit_enum_drop(*slot as usize),
                DdlUndo::EnumAltered { slot, .. } => {
                    self.storage.commit_enum_alter(*slot as usize, txn.txid)
                }
                DdlUndo::IndexCreated(slot) => {
                    let slot = *slot as usize;
                    self.storage.commit_index_create(slot);
                    self.storage.commit_object_owner(
                        crate::storage::AccessObject {
                            class: crate::storage::AccessClass::Index,
                            slot: slot as u16,
                        },
                        txn.txid,
                    );
                    if let Some(table) = self.storage.index_table_slot(slot)
                        && !index_tables[..index_table_count].contains(&table)
                    {
                        index_tables[index_table_count] = table;
                        index_table_count += 1;
                    }
                }
                DdlUndo::IndexDropped(slot) => {
                    let slot = *slot as usize;
                    self.storage.commit_index_drop(slot);
                    if let Some(table) = self.storage.index_table_slot(slot)
                        && !index_tables[..index_table_count].contains(&table)
                    {
                        index_tables[index_table_count] = table;
                        index_table_count += 1;
                    }
                }
                DdlUndo::IndexRenamed { slot, .. } => {
                    self.storage.commit_index_rename(*slot as usize, txn.txid)
                }
                // The reset already happened in place; committing keeps it.
                DdlUndo::SequenceReset { .. } | DdlUndo::OwnedSequenceReset { .. } => {}
                DdlUndo::SchemaCreated(slot) => {
                    self.storage.commit_schema_create(*slot as usize);
                    self.storage.commit_object_owner(
                        crate::storage::AccessObject {
                            class: crate::storage::AccessClass::Schema,
                            slot: *slot as u16,
                        },
                        txn.txid,
                    );
                }
                DdlUndo::SchemaDropped(slot) => self.storage.commit_schema_drop(*slot as usize),
                DdlUndo::RoleChanged { slot, .. } => {
                    self.storage.commit_role_change(*slot as usize);
                }
                DdlUndo::RoleMembershipChanged { slot, .. } => {
                    self.storage.commit_role_membership_change(*slot as usize);
                }
                DdlUndo::ObjectOwnerChanged { object, .. } => {
                    self.storage.commit_object_owner(*object, txn.txid);
                }
                DdlUndo::ObjectAclChanged { slot, .. } => {
                    self.storage.commit_acl(*slot as usize, txn.txid);
                }
                DdlUndo::DefaultAclChanged { slot, .. } => {
                    self.storage.commit_default_acl(*slot as usize, txn.txid);
                }
                // Promote the uncommitted comment overlay to committed; its WAL
                // record was journaled at exec time (like other DDL).
                DdlUndo::CommentSet { slot, .. } => {
                    self.storage.commit_comment(*slot as usize, txn.txid);
                }
            }
        }
        for &advance in txn.subscription_advances() {
            self.storage.apply_subscription_advance(advance);
        }
        let mut index_result = Ok(());
        for &(table, rewrote_rows) in &altered_tables[..altered_count] {
            self.storage.finish_table_def_commit(table, rewrote_rows);
            if self.storage.table(table).live
                && let Err(error) = self.storage.refresh_enforcers(table)
            {
                index_result = Err(error);
                break;
            }
        }
        if index_result.is_ok() {
            for &table in &index_tables[..index_table_count] {
                if altered_tables[..altered_count]
                    .iter()
                    .any(|&(altered, _)| altered == table)
                {
                    continue;
                }
                if let Err(error) = self.storage.refresh_enforcers(table) {
                    index_result = Err(error);
                    break;
                }
            }
        }
        for slot in 0..self.storage.table_count() {
            self.storage.commit_table_statistics(slot, txn.txid);
        }
        // Past the durability point, so these fire iff the transaction really
        // committed: apply its LISTEN/UNLISTEN to the shared registry and move
        // its notifications into the delivery outbox. A pool-exhaustion here is
        // a loud error reported to the client — like a post-commit upload
        // failure, the data is committed regardless — never a silent drop.
        let notify_result = self.flush_committed_notifications(txn);
        guc.commit_transaction();
        txn.clear();
        notify_result.and(index_result)
    }

    /// Applies a committing transaction's buffered LISTEN/UNLISTEN to the shared
    /// registry and moves its NOTIFYs into the delivery outbox. Called only past
    /// the commit's durability point.
    fn flush_committed_notifications(&mut self, txn: &TxnState) -> Result<(), SqlError> {
        for &op in txn.pending_listen_ops() {
            self.notify.apply(op)?;
        }
        for i in 0..txn.pending_notify_count() {
            self.notify.enqueue(txn.pending_notification(i))?;
        }
        Ok(())
    }

    /// Applies one transaction-local catalog undo entry. Full rollback and
    /// savepoint rollback share this choke point so new DDL cannot accidentally
    /// acquire different rollback semantics on the two paths.
    fn rollback_ddl(&mut self, undo: DdlUndo, txid: u32) {
        match undo {
            DdlUndo::Created(slot) => self.storage.rollback_create(slot as usize),
            DdlUndo::Dropped(slot) => {
                self.storage.rollback_drop(slot as usize);
                let name = self.storage.table(slot as usize).def.name;
                let schema = self.storage.table(slot as usize).def.schema;
                self.storage
                    .rollback_indexes_for(schema.as_str(), name.as_str(), txid);
            }
            DdlUndo::TableAltered(slot) => {
                self.storage.rollback_table_def(slot as usize, txid);
            }
            DdlUndo::ViewCreated(slot) => self.storage.rollback_view_create(slot as usize),
            DdlUndo::RoutineCreated(slot) => self.storage.rollback_routine_create(slot as usize),
            DdlUndo::RoutineDropped(slot) => {
                self.storage.rollback_routine_drop(slot as usize, txid)
            }
            DdlUndo::TriggerCreated(slot) => self.storage.rollback_trigger_create(slot as usize),
            DdlUndo::TriggerDropped(slot) => {
                self.storage.rollback_trigger_drop(slot as usize, txid)
            }
            DdlUndo::TriggerAltered { slot, prior } => {
                self.storage.rollback_trigger_alter(slot as usize, prior)
            }
            DdlUndo::RoutineIdentityAltered { slot, prior } => {
                self.storage.restore_routine_identity(slot as usize, prior)
            }
            DdlUndo::ViewDropped(slot) => {
                self.storage.rollback_view_drop(slot as usize, txid);
            }
            DdlUndo::PublicationCreated(slot) => {
                self.storage.rollback_publication_create(slot as usize)
            }
            DdlUndo::PublicationDropped(slot) => {
                self.storage.rollback_publication_drop(slot as usize, txid)
            }
            DdlUndo::PublicationAltered { slot, prior } => self
                .storage
                .rollback_publication_alter(slot as usize, prior),
            DdlUndo::PublicationOwnerChanged { slot, prior } => self
                .storage
                .restore_publication_owner_pending(slot as usize, prior),
            DdlUndo::PublicationRenamed { slot, prior } => self
                .storage
                .rollback_publication_rename(slot as usize, prior),
            DdlUndo::SubscriptionCreated(slot) => {
                self.storage.rollback_subscription_create(slot as usize)
            }
            DdlUndo::SubscriptionDropped(slot) => {
                self.storage.rollback_subscription_drop(slot as usize, txid)
            }
            DdlUndo::SubscriptionEnabled { slot, prior } => self
                .storage
                .restore_subscription_enabled(slot as usize, prior),
            DdlUndo::SubscriptionDefinitionChanged { slot, prior } => self
                .storage
                .restore_subscription_definition(slot as usize, prior),
            DdlUndo::MatviewCreated(slot) => self.storage.rollback_matview_create(slot as usize),
            DdlUndo::MatviewDropped(slot) => {
                self.storage.rollback_matview_drop(slot as usize, txid);
            }
            DdlUndo::SequenceCreated(slot) => {
                self.storage.rollback_sequence_create(slot as usize);
            }
            DdlUndo::SequenceDropped(slot) => {
                self.storage.rollback_sequence_drop(slot as usize, txid);
            }
            DdlUndo::SequenceAltered { slot, prior } => {
                self.storage.rollback_sequence_alter(slot as usize, prior);
            }
            DdlUndo::DomainCreated(slot) => self.storage.rollback_domain_create(slot as usize),
            DdlUndo::DomainDropped(slot) => {
                self.storage.rollback_domain_drop(slot as usize, txid);
            }
            DdlUndo::DomainAltered { slot, prior } => {
                self.storage.rollback_domain_alter(slot as usize, prior)
            }
            DdlUndo::EnumCreated(slot) => self.storage.rollback_enum_create(slot as usize),
            DdlUndo::EnumDropped(slot) => {
                self.storage.rollback_enum_drop(slot as usize, txid);
            }
            DdlUndo::EnumAltered { slot, prior } => {
                self.storage.rollback_enum_alter(slot as usize, prior);
            }
            DdlUndo::IndexCreated(slot) => {
                let slot = slot as usize;
                let table = self.storage.index_table_slot(slot);
                self.storage.rollback_index_create(slot);
                if let Some(table) = table {
                    self.storage
                        .refresh_enforcers(table)
                        .expect("rolling back CREATE INDEX restores its cache binding");
                }
            }
            DdlUndo::IndexDropped(slot) => {
                self.storage.rollback_index_drop(slot as usize, txid);
            }
            DdlUndo::IndexRenamed { slot, prior } => {
                self.storage.rollback_index_rename(slot as usize, prior)
            }
            DdlUndo::SequenceReset {
                table,
                column,
                prior,
            } => {
                let table = self.storage.table_mut(table as usize);
                table.serial_last[column as usize] = prior;
                table.serial_dirty = true;
            }
            DdlUndo::OwnedSequenceReset { sequence, prior } => {
                self.storage
                    .restore_sequence_value(sequence as usize, prior);
            }
            DdlUndo::SchemaCreated(slot) => self.storage.rollback_schema_create(slot as usize),
            DdlUndo::SchemaDropped(slot) => self.storage.rollback_schema_drop(slot as usize, txid),
            DdlUndo::RoleChanged { slot, prior } => {
                self.storage.rollback_role_change(slot as usize, prior);
            }
            DdlUndo::RoleMembershipChanged { slot, prior } => {
                self.storage
                    .rollback_role_membership_change(slot as usize, prior);
            }
            DdlUndo::ObjectOwnerChanged { object, prior } => {
                self.storage.restore_object_owner(object, prior);
            }
            DdlUndo::ObjectAclChanged { slot, prior } => {
                self.storage.restore_acl_pending(slot as usize, prior);
            }
            DdlUndo::DefaultAclChanged { slot, prior } => {
                self.storage
                    .restore_default_acl_pending(slot as usize, prior);
            }
            DdlUndo::CommentSet { slot, prior } => {
                self.storage.restore_comment_pending(slot as usize, prior);
            }
        }
    }

    /// Discards every uncommitted change and journal byte of the
    /// transaction.
    pub fn rollback_txn(&mut self, txn: &mut TxnState, guc: &GucState) {
        // The next statement starts a fresh transaction clock.
        datetime::end_transaction();
        if txn.txid == 0 {
            guc.rollback_transaction();
            txn.clear();
            return;
        }
        self.storage.release_snapshot(txn.txid);
        self.storage.release_serializable(txn.txid);
        self.storage.release_table_locks(txn.txid);
        self.storage.release_row_locks(txn.txid);
        // Reverse-replay every write to its prior image (newest first), so a
        // row written multiple times unwinds to its pre-transaction state.
        for &(table, rowid, prior) in txn.touched().iter().rev() {
            self.storage
                .restore_pending(table as usize, rowid, txn.txid, prior);
        }
        for &undo in txn.ddl().iter().rev() {
            self.rollback_ddl(undo, txn.txid);
        }
        for undo in txn.statistics_undo().iter().rev() {
            self.storage
                .rollback_table_statistics(undo.table as usize, txn.txid);
        }
        self.wal.discard_stage(txn.txid);
        guc.rollback_transaction();
        txn.clear();
    }

    /// A deadlock victim releases pending versions and locks immediately,
    /// while ReadyForQuery continues to report an aborted explicit block until
    /// the client issues COMMIT or ROLLBACK.
    fn abort_explicit_txn(&mut self, txn: &mut TxnState, guc: &GucState) {
        let txid = txn.txid;
        self.rollback_txn(txn, guc);
        txn.txid = txid;
        txn.mode = TxnMode::Explicit;
        txn.failed = true;
    }

    /// Restores transaction-owned state a partially executed statement built
    /// before discovering a lock wait. Transaction-level locks remain held
    /// while the protocol message is parked.
    fn rollback_waiting_statement(&mut self, txn: &mut TxnState, mark: txn::StatementMark) {
        for index in (mark.touched..txn.touched().len()).rev() {
            let (table, rowid, prior) = txn.touched()[index];
            self.storage
                .restore_pending(table as usize, rowid, txn.txid, prior);
        }
        for index in (mark.ddl..txn.ddl().len()).rev() {
            self.rollback_ddl(txn.ddl()[index], txn.txid);
        }
        for index in (mark.statistics..txn.statistics_undo().len()).rev() {
            self.storage
                .rollback_table_statistics(txn.statistics_undo()[index].table as usize, txn.txid);
        }
        txn.rewind_touched(mark.touched);
        txn.rewind_truncates(mark.truncates);
        txn.rewind_ddl(mark.ddl);
        txn.rewind_statistics(mark.statistics);
        txn.rewind_subscription_advances(mark.subscription_advances);
        txn.rewind_notifications(
            mark.notifications,
            mark.notification_payload,
            mark.listen_ops,
        );
        self.wal.truncate_stage(txn.txid, mark.wal);
    }

    /// Rolls back to the savepoint at `index`: undoes every row write and DDL
    /// performed after it (reverse-replayed), discards the journal tail, and
    /// restores the pre-savepoint failed state — leaving the transaction (and
    /// the savepoint) open for reuse.
    fn rollback_to_savepoint(&mut self, txn: &mut TxnState, index: usize, guc: &GucState) {
        let sp = txn.savepoint_at(index);
        for i in (sp.touched_mark..txn.touched().len()).rev() {
            let (table, rowid, prior) = txn.touched()[i];
            self.storage
                .restore_pending(table as usize, rowid, txn.txid, prior);
        }
        for i in (sp.ddl_mark..txn.ddl().len()).rev() {
            self.rollback_ddl(txn.ddl()[i], txn.txid);
        }
        for i in (sp.statistics_mark..txn.statistics_undo().len()).rev() {
            let undo = txn.statistics_undo()[i];
            self.storage
                .rollback_table_statistics(undo.table as usize, txn.txid);
        }
        txn.rewind_touched(sp.touched_mark);
        txn.rewind_truncates(sp.truncate_mark);
        txn.rewind_ddl(sp.ddl_mark);
        txn.rewind_statistics(sp.statistics_mark);
        txn.rewind_subscription_advances(sp.subscription_advance_mark);
        txn.rewind_notifications(sp.notify_mark, sp.notify_payload_mark, sp.listen_mark);
        self.storage.rollback_locks_to(txn.txid, sp.lock_mark);
        txn.rollback_savepoints_after(index);
        self.wal.truncate_stage(txn.txid, sp.wal_mark);
        guc.rollback_to_savepoint(index);
        txn.failed = sp.failed;
    }

    /// Publishes every committed journal record accumulated since the prior
    /// protocol flush.  This is the object-store acknowledgement barrier.
    pub fn commit_wal(&mut self) -> Result<(), SqlError> {
        self.wal.commit();
        self.upload_wal_batch()
    }

    /// Whether success responses must remain buffered until commit-batch
    /// publication completes.
    pub(crate) const fn publication_required(&self) -> bool {
        self.wal_upload
    }

    /// Publishes the accumulated immutable commit batch to the bucket.
    fn upload_wal_batch(&mut self) -> Result<(), SqlError> {
        if !self.wal_upload {
            return Ok(());
        }
        let Some(batch) = self.wal.last_committed_batch() else {
            return Ok(());
        };
        if batch.byte_len() == 0 {
            self.wal.clear_batch_marker();
            return Ok(());
        }
        self.wal_seg_buf.resize(batch.byte_len(), 0);
        if self
            .wal
            .read_range(batch.start(), &mut self.wal_seg_buf)
            .is_err()
        {
            return Err(SqlError {
                sqlstate: sqlstate::IO_ERROR,
                message: stack_format!(192, "cannot read WAL batch for upload"),
            });
        }
        if let Some(c) = self.ckpt.as_mut() {
            c.publish_commit_batch(batch.first_lsn(), &self.wal_seg_buf)?;
        }
        self.wal.clear_batch_marker();
        Ok(())
    }

    /// Makes a previously failed committed batch object-store durable before a
    /// later statement can observe it.
    fn retry_pending_wal_upload(&mut self) -> Result<(), SqlError> {
        if !self.wal_upload || self.wal.pending_batch_bytes() == 0 {
            return Ok(());
        }
        self.upload_wal_batch()
    }

    /// Snapshots to object storage, then truncates the journal and compacts
    /// the heap. The atomic form — drives the sliced checkpoint's beats to
    /// completion in one call, for the explicit `CHECKPOINT` statement and
    /// shutdown. `Ok(false)` = nothing to do.
    pub fn checkpoint_enabled(&self) -> bool {
        self.ckpt.is_some()
    }

    /// Enables reactor-driven reads only for the durable block stack. Manifest
    /// and WAL clients retain their synchronous, statement-atomic contracts.
    pub(crate) fn enable_async_block_reads(&mut self) {
        if let Some(checkpointer) = self.ckpt.as_mut() {
            checkpointer.enable_async_block_reads();
        }
    }

    pub(crate) fn disable_async_block_reads(&mut self) {
        if let Some(checkpointer) = self.ckpt.as_mut() {
            checkpointer.disable_async_block_reads();
        }
    }

    pub(crate) fn block_read_slots(&self) -> usize {
        self.ckpt
            .as_ref()
            .map_or(0, crate::checkpoint::Checkpointer::block_read_slots)
    }

    pub(crate) fn pending_block_read_fd(&self, slot: usize) -> Option<std::os::fd::RawFd> {
        self.ckpt
            .as_ref()
            .and_then(|checkpointer| checkpointer.pending_block_read_fd(slot))
    }

    fn block_reads_pending(&self) -> bool {
        self.ckpt
            .as_ref()
            .is_some_and(crate::checkpoint::Checkpointer::block_reads_busy)
    }

    /// Advances a pending block read. A completed read or a terminal failure
    /// both wake parked statements; the retry then consumes the cached block
    /// or returns the real storage error.
    pub(crate) fn advance_pending_block_read(
        &mut self,
        slot: usize,
    ) -> Result<bool, crate::store::StoreError> {
        let Some(checkpointer) = self.ckpt.as_mut() else {
            return Ok(false);
        };
        checkpointer.advance_pending_block_read(slot)
    }

    pub(crate) fn next_block_read_hedge_deadline(&self) -> Option<std::time::Instant> {
        self.ckpt
            .as_ref()
            .and_then(crate::checkpoint::Checkpointer::next_block_read_hedge_deadline)
    }

    pub(crate) fn issue_due_block_read_hedges(&mut self, now: std::time::Instant) {
        if let Some(checkpointer) = self.ckpt.as_mut() {
            checkpointer.issue_due_block_read_hedges(now);
        }
    }

    fn validate_maintenance_targets(
        &self,
        targets: &[ast::MaintenanceTarget<'_>],
        txid: u32,
    ) -> Result<(), SqlError> {
        for target in targets {
            let slot = exec::resolve_dml_table(&self.storage, &target.table, txid)?;
            let definition = self.storage.table_def(slot, txid);
            for column in target.columns {
                if !definition
                    .columns()
                    .iter()
                    .any(|metadata| metadata.name.as_str() == *column)
                {
                    return Err(sql_err!(
                        sqlstate::UNDEFINED_COLUMN,
                        "column \"{}\" of relation \"{}\" does not exist",
                        column,
                        target.table.name
                    ));
                }
            }
        }
        Ok(())
    }

    fn lock_maintenance_targets(
        &self,
        targets: &[ast::MaintenanceTarget<'_>],
        txid: u32,
    ) -> Result<(), SqlError> {
        if targets.is_empty() {
            for slot in 0..self.storage.table_count() {
                if self.storage.table(slot).visible_to(txid) {
                    self.storage.lock_table(
                        txid,
                        slot,
                        ast::TableLockMode::ShareUpdateExclusive,
                        false,
                    )?;
                }
            }
            return Ok(());
        }
        for target in targets {
            let slot = exec::resolve_dml_table(&self.storage, &target.table, txid)?;
            self.storage
                .lock_table(txid, slot, ast::TableLockMode::ShareUpdateExclusive, false)?;
        }
        Ok(())
    }

    fn analyze_targets(
        &mut self,
        targets: &[ast::MaintenanceTarget<'_>],
        txn: &mut TxnState,
    ) -> Result<u64, SqlError> {
        self.validate_maintenance_targets(targets, txn.txid)?;
        let mut total_rows = 0u64;
        if targets.is_empty() {
            for slot in 0..self.storage.table_count() {
                if self.storage.table(slot).visible_to(txn.txid) {
                    txn.record_statistics(slot as u32)?;
                    total_rows = total_rows
                        .saturating_add(self.storage.analyze_table(slot, txn.txid, &[])?.rows);
                }
            }
            return Ok(total_rows);
        }
        for target in targets {
            let slot = exec::resolve_dml_table(&self.storage, &target.table, txn.txid)?;
            let definition = self.storage.table_def(slot, txn.txid);
            let mut selected = [0usize; crate::storage::MAX_COLUMNS];
            let mut selected_count = 0usize;
            for column in target.columns {
                selected[selected_count] = definition
                    .columns()
                    .iter()
                    .position(|metadata| metadata.name.as_str() == *column)
                    .expect("maintenance targets were validated");
                selected_count += 1;
            }
            txn.record_statistics(slot as u32)?;
            total_rows = total_rows.saturating_add(
                self.storage
                    .analyze_table(slot, txn.txid, &selected[..selected_count])?
                    .rows,
            );
        }
        Ok(total_rows)
    }

    pub fn checkpoint(&mut self) -> Result<bool, SqlError> {
        self.retry_pending_wal_upload()?;
        if self.post_publish_cleanup.is_some() {
            self.finish_post_publish_cleanup()?;
        }
        let Some(ckpt) = self.ckpt.as_mut() else {
            return Err(SqlError {
                sqlstate: sqlstate::FEATURE_NOT_SUPPORTED,
                message: stack_format!(192, "no object storage configured (object_store = off)"),
            });
        };
        // Everything the snapshot will contain must be journal-durable
        // first, so an interrupted checkpoint never strands acked writes.
        self.wal.commit();
        if ckpt.block_reads_busy() {
            return Err(sql_err!(
                sqlstate::INTERNAL_IO_WAIT,
                "durable block reads in progress"
            ));
        }
        // A checkpoint owns the block stack synchronously: its publication
        // state machine cannot be rewound as a client statement can.
        ckpt.disable_async_block_reads();
        let checkpoint = ckpt.checkpoint(&mut self.storage, &mut self.scratch);
        ckpt.enable_async_block_reads();
        match checkpoint? {
            Some(lsn) => {
                self.begin_post_publish_cleanup(lsn);
                self.finish_post_publish_cleanup()?;
                Ok(true)
            }
            None => Ok(false),
        }
    }

    /// The journal and heap bookkeeping owed once a manifest has published:
    /// everything at or below `lsn` is bucket-durable, so the local journal
    /// restarts and the heap compacts (spilling under memory pressure).
    fn after_publish(&mut self, lsn: u64) -> Result<(), SqlError> {
        if !self.storage.has_active_snapshots() {
            if self.wal_upload
                && let Some(ckpt) = self.ckpt.as_mut()
            {
                // A checkpoint covers its own state, but not a logical
                // consumer's history. Retain the segment straddling the
                // oldest slot restart point until the slot advances or drops.
                let retain_through = self
                    .storage
                    .oldest_replication_restart_lsn()
                    .unwrap_or(lsn)
                    .min(lsn);
                ckpt.prune_commit_batches(retain_through)?;
            }
            // A sliced checkpoint can publish a snapshot while later
            // statements have already appended WAL. Retaining the journal in
            // that case lets recovery replay the suffix above this manifest;
            // only a checkpoint at the current tail may restart it.
            if self.wal.last_lsn() <= lsn {
                self.wal.reset_after_checkpoint();
            }
        }
        // The checkpoint installed each table's spill-SST list as it
        // wrote (full rewrites collapse a list, deltas append).
        self.storage.release_durable_histories();
        self.storage.compact_heap(&mut self.compact_scratch)?;
        // Under memory pressure, committed bytes leave the heap: the map
        // entries flip to spilled and a second compaction drops the
        // bytes. Reads fetch them back through the cache tiers. Below the
        // threshold nothing spills and reads stay heap-fast.
        if self.storage.spill_attached()
            && (self.storage.heap.used() * 100 >= self.storage.heap.capacity() * 50
                || self.storage.map_pressure())
        {
            self.storage.evict_committed();
            self.storage.compact_heap(&mut self.compact_scratch)?;
        }
        // Map-occupancy pressure sheds redundant entries the same way heap
        // pressure sheds bytes: the overlay keeps the working set, the
        // bucket keeps the rows.
        self.storage.evict_entries();
        Ok(())
    }

    fn begin_post_publish_cleanup(&mut self, lsn: u64) {
        // Cleanup may retry, but publication already marked only the table
        // generations the manifest captured as clean.
        self.post_publish_cleanup = Some(lsn);
    }

    fn finish_post_publish_cleanup(&mut self) -> Result<(), SqlError> {
        let lsn = self
            .post_publish_cleanup
            .expect("post-publication cleanup has its published LSN");
        self.after_publish(lsn)?;
        self.post_publish_cleanup = None;
        Ok(())
    }

    fn retry_post_publish_cleanup(&mut self) -> Result<(), SqlError> {
        if self.post_publish_cleanup.is_some() {
            self.finish_post_publish_cleanup()?;
        }
        Ok(())
    }

    /// Whether checkpoint or compaction work is pending — an active sweep,
    /// a paced merge (mid-flight, finished-awaiting-publish, or a list at
    /// the trigger). The event loop keeps beating pending work between
    /// events, so an idle server still finishes what a trigger started and
    /// compacts what its lists owe.
    /// The COPY FROM the last statement started, if any; the connection
    /// takes it and enters copy-in mode.
    pub fn take_pending_copy(&mut self) -> Option<exec::CopySetup> {
        self.pending_copy.take()
    }

    /// True if committed notifications await delivery. The server drains them
    /// after each connection's message (see [`Engine::notifications`]).
    pub fn has_notifications(&self) -> bool {
        self.notify.has_pending()
    }

    /// The committed notifications awaiting delivery.
    pub fn notifications(&self) -> &[notify::Notification] {
        self.notify.outbox()
    }

    /// True if the connection is registered for the channel.
    pub fn is_listening(&self, conn_id: i32, channel: &str) -> bool {
        self.notify.is_listening(conn_id, channel)
    }

    /// Discards the delivered notifications (the server calls this after fanning
    /// the outbox out to every listener).
    pub fn clear_notifications(&mut self) {
        self.notify.clear_outbox();
    }

    /// Drops a closing connection's LISTEN registrations.
    pub fn drop_connection(&mut self, conn_id: i32) {
        self.notify.drop_conn(conn_id);
    }

    /// One complete COPY data line (no trailing newline).
    pub fn copy_row_line(
        &mut self,
        setup: &exec::CopySetup,
        txn: &mut TxnState,
        seq_session: &guc::SeqSession,
        arena: &Arena,
        line: &[u8],
    ) -> Result<(), SqlError> {
        exec::copy_row(&mut self.storage, txn, seq_session, setup, line, arena)
    }

    /// One complete COPY FROM binary row (int16 field count + fields).
    pub fn copy_row_binary(
        &mut self,
        setup: &exec::CopySetup,
        txn: &mut TxnState,
        seq_session: &guc::SeqSession,
        arena: &Arena,
        row: &[u8],
    ) -> Result<(), SqlError> {
        exec::copy_row_binary(&mut self.storage, txn, seq_session, setup, row, arena)
    }

    /// Ends a successful COPY FROM: an implicit transaction commits here
    /// (this was the statement's end); an explicit one stays open, exactly
    /// as INSERT inside BEGIN would.
    pub fn copy_finish(&mut self, txn: &mut TxnState, guc: &GucState) -> Result<(), SqlError> {
        if txn.mode == TxnMode::Implicit {
            return self.commit_txn(txn, guc);
        }
        Ok(())
    }

    /// Abandons a failed COPY FROM: an implicit transaction rolls back
    /// outright; an explicit one is marked failed, as any errored statement
    /// leaves it.
    pub fn copy_abort(&mut self, txn: &mut TxnState, guc: &GucState) {
        if txn.mode == TxnMode::Implicit {
            self.rollback_txn(txn, guc);
        } else {
            txn.failed = true;
        }
    }

    pub fn checkpoint_work_pending(&self) -> bool {
        self.post_publish_cleanup.is_some()
            || self.ckpt.as_ref().is_some_and(|c| {
                c.sweep_active() || c.maintenance_pending() || c.merge_work_pending(&self.storage)
            })
    }

    /// One checkpoint beat: a trigger (heap or journal filling) starts a
    /// sweep, and an active sweep advances one slice per call until its
    /// manifest publishes — so a checkpoint never stalls the connections for
    /// its whole duration, only for one table's write. Called after each
    /// query message and by the idle event loop. Failures are reported on
    /// stderr and the beat retried rather than failing unrelated statements;
    /// the return is false on a failed beat so the idle driver can back off
    /// a persistently-down bucket.
    pub fn maybe_checkpoint(&mut self) -> bool {
        // A suspended read owns the sole block client until the reactor
        // completes it. Checkpoint publication uses that client synchronously.
        if self.block_reads_pending() {
            return true;
        }
        if let Err(error) = self.retry_pending_wal_upload() {
            eprintln!(
                "pos3ql: auto-checkpoint failed ({}): {}",
                error.sqlstate,
                error.message.as_str()
            );
            return false;
        }
        if self.post_publish_cleanup.is_some() {
            return match self.finish_post_publish_cleanup() {
                Ok(()) => true,
                Err(e) => {
                    eprintln!(
                        "pos3ql: post-checkpoint bookkeeping failed ({}): {}",
                        e.sqlstate,
                        e.message.as_str()
                    );
                    false
                }
            };
        }
        let Some(ckpt) = self.ckpt.as_mut() else {
            return true;
        };
        let heap_full = self.storage.heap.used() * 100 >= self.storage.heap.capacity() * 65;
        let wal_full = self.wal.used_bytes() * 100 >= self.wal.capacity_bytes() * 50;
        let history_full = self.storage.history_pressure();
        if !(ckpt.sweep_active()
            || ckpt.maintenance_pending()
            || ckpt.merge_work_pending(&self.storage)
            || heap_full
            || wal_full
            || history_full)
        {
            return true;
        }
        self.wal.commit();
        if ckpt.block_reads_busy() {
            return true;
        }
        // A checkpoint beat advances publication state that cannot be replayed
        // after yielding. Its block reads therefore own the store
        // synchronously, just as an explicit CHECKPOINT does.
        ckpt.disable_async_block_reads();
        let checkpoint = ckpt.checkpoint_step(&mut self.storage, &mut self.scratch);
        ckpt.enable_async_block_reads();
        match checkpoint {
            Ok(CheckpointStep::Published { lsn }) => {
                self.begin_post_publish_cleanup(lsn);
                if let Err(e) = self.finish_post_publish_cleanup() {
                    eprintln!(
                        "pos3ql: post-checkpoint bookkeeping failed ({}): {}",
                        e.sqlstate,
                        e.message.as_str()
                    );
                    return false;
                }
                true
            }
            Ok(_) => true,
            Err(e) => {
                eprintln!(
                    "pos3ql: auto-checkpoint failed ({}): {}",
                    e.sqlstate,
                    e.message.as_str()
                );
                false
            }
        }
    }

    /// Executes a simple-query string (possibly several statements).
    /// SQL errors become ErrorResponses and stop the remainder, as in
    /// PostgreSQL. `Err(WireFull)` means the send buffer overflowed and the
    /// connection must handle it.
    #[allow(clippy::too_many_arguments)]
    pub fn execute_simple(
        &mut self,
        text: &str,
        arena: &Arena,
        txn: &mut TxnState,
        sqlprep: &mut SqlPreparedPool,
        cursors: &mut cursor::CursorPool,
        guc: &mut GucState,
        responder: &mut Responder,
        conn_id: i32,
    ) -> Result<ExecutionStatus, WireFull> {
        // Embedders that call the engine directly have no protocol reactor to
        // provide the response barrier. Preserve the same acknowledgement
        // contract here; network connections use `execute_simple_from` and
        // publish once per readable batch instead.
        if let Err(error) = self.retry_pending_wal_upload() {
            responder.error(error.sqlstate, error.message.as_str())?;
            return Ok(ExecutionStatus::Complete);
        }
        let output_mark = responder.buffer.mark();
        let result = self.execute_simple_from(
            text, 0, arena, txn, sqlprep, cursors, guc, responder, conn_id, false,
        )?;
        if let Err(error) = self.commit_wal() {
            responder.buffer.truncate_to(output_mark);
            responder.error(error.sqlstate, error.message.as_str())?;
        }
        Ok(result)
    }

    #[allow(clippy::too_many_arguments)]
    pub fn execute_simple_from(
        &mut self,
        text: &str,
        resume_statement: usize,
        arena: &Arena,
        txn: &mut TxnState,
        sqlprep: &mut SqlPreparedPool,
        cursors: &mut cursor::CursorPool,
        guc: &mut GucState,
        responder: &mut Responder,
        conn_id: i32,
        lock_timeout_expired: bool,
    ) -> Result<ExecutionStatus, WireFull> {
        self.current_conn_id = conn_id;
        let mut parser = match Parser::new(text, arena) {
            Ok(p) => p,
            Err(e) => {
                report_parse_error(responder, &e)?;
                return Ok(ExecutionStatus::Complete);
            }
        };
        // The whole message runs in one implicit transaction unless an
        // explicit block is open — an error undoes the entire message,
        // matching PostgreSQL's implicit-transaction rule.
        // Freeze this statement's clock before anything anchors a transaction
        // to it, so `now()` and `statement_timestamp()` agree on a lone
        // statement as they do in PostgreSQL.
        datetime::begin_statement();
        self.ensure_txn(txn, TxnMode::Implicit, guc);
        let mut executed_any = resume_statement > 0;
        let mut statement_index = 0usize;
        loop {
            match parser.next_stmt() {
                Ok(Some(statement)) => {
                    if statement_index < resume_statement {
                        statement_index += 1;
                        continue;
                    }
                    if self.post_publish_cleanup.is_some()
                        && !matches!(statement, Stmt::Rollback | Stmt::RollbackToSavepoint(_))
                        && let Err(error) = self.retry_post_publish_cleanup()
                    {
                        responder.error(error.sqlstate, error.message.as_str())?;
                        return Ok(ExecutionStatus::Complete);
                    }
                    if self.pending_copy.take().is_some() {
                        // COPY FROM STDIN takes over the connection; a
                        // statement after it in the same string has nowhere
                        // to run.
                        self.copy_abort(txn, guc);
                        let e = sql_err!(
                            sqlstate::FEATURE_NOT_SUPPORTED,
                            "COPY FROM STDIN must be the last statement in a query string"
                        );
                        responder.error(e.sqlstate, e.message.as_str())?;
                        return Ok(ExecutionStatus::Complete);
                    }
                    executed_any = true;
                    let output_mark = responder.buffer.mark();
                    let statement_mark =
                        txn.statement_mark(self.wal.stage_mark(txn.txid), self.storage.lock_mark());
                    emit_parse_warnings(&mut parser, responder)?;
                    let outcome = self.execute_stmt(
                        &statement, arena, NO_PARAMS, txn, sqlprep, cursors, guc, responder,
                    )?;
                    let outcome = outcome.and_then(|()| query::check_timeout());
                    if let Err(mut e) = outcome {
                        if e.sqlstate == sqlstate::INTERNAL_LOCK_WAIT
                            || e.sqlstate == sqlstate::INTERNAL_IO_WAIT
                        {
                            self.rollback_waiting_statement(txn, statement_mark);
                            if !lock_timeout_expired {
                                return Ok(ExecutionStatus::Blocked {
                                    completed_statements: statement_index,
                                    output_mark,
                                    io_wait: e.sqlstate == sqlstate::INTERNAL_IO_WAIT,
                                });
                            }
                            self.storage
                                .rollback_locks_to(txn.txid, statement_mark.lock);
                            e = sql_err!(
                                sqlstate::LOCK_NOT_AVAILABLE,
                                "canceling statement due to lock timeout"
                            );
                        }
                        if txn.is_explicit() && e.sqlstate == sqlstate::DEADLOCK_DETECTED {
                            self.abort_explicit_txn(txn, guc);
                        } else if txn.is_explicit() {
                            txn.failed = true;
                        } else {
                            self.rollback_txn(txn, guc);
                        }
                        responder.error(e.sqlstate, e.message.as_str())?;
                        return Ok(ExecutionStatus::Complete);
                    }
                    statement_index += 1;
                }
                Ok(None) => break,
                Err(e) => {
                    if txn.is_explicit() {
                        txn.failed = true;
                    } else {
                        self.rollback_txn(txn, guc);
                    }
                    report_parse_error(responder, &e)?;
                    return Ok(ExecutionStatus::Complete);
                }
            }
        }
        if !executed_any {
            responder.empty_query_response()?;
        }
        // Implicit transactions commit at end of message — except a COPY
        // FROM in flight, whose statement does not end until CopyDone.
        if txn.mode == TxnMode::Implicit
            && self.pending_copy.is_none()
            && let Err(e) = self.commit_txn(txn, guc)
        {
            responder.error(e.sqlstate, e.message.as_str())?;
        }
        Ok(ExecutionStatus::Complete)
    }

    pub fn lock_generation(&self) -> u64 {
        self.storage.lock_generation()
    }

    /// Extended-protocol Execute: exactly one statement, already-validated
    /// text, bound parameters. Returns whether it succeeded (a false sends
    /// the connection into skip-to-Sync).
    #[allow(clippy::too_many_arguments)]
    pub fn execute_extended(
        &mut self,
        text: &str,
        arena: &Arena,
        params: &[Datum],
        txn: &mut TxnState,
        sqlprep: &mut SqlPreparedPool,
        cursors: &mut cursor::CursorPool,
        guc: &mut GucState,
        responder: &mut Responder,
        conn_id: i32,
        lock_timeout_expired: bool,
    ) -> Result<ExtendedExecutionStatus, WireFull> {
        self.current_conn_id = conn_id;
        let mut parser = match Parser::new(text, arena) {
            Ok(p) => p,
            Err(e) => {
                report_parse_error(responder, &e)?;
                return Ok(ExtendedExecutionStatus::Complete(false));
            }
        };
        let statement = match parser.next_stmt() {
            Ok(Some(statement)) => statement,
            Ok(None) => {
                responder.empty_query_response()?;
                return Ok(ExtendedExecutionStatus::Complete(true));
            }
            Err(e) => {
                if txn.is_explicit() && e.sqlstate == sqlstate::DEADLOCK_DETECTED {
                    self.abort_explicit_txn(txn, guc);
                } else if txn.is_explicit() {
                    txn.failed = true;
                }
                report_parse_error(responder, &e)?;
                return Ok(ExtendedExecutionStatus::Complete(false));
            }
        };
        if self.post_publish_cleanup.is_some()
            && !matches!(statement, Stmt::Rollback | Stmt::RollbackToSavepoint(_))
            && let Err(error) = self.retry_post_publish_cleanup()
        {
            responder.error(error.sqlstate, error.message.as_str())?;
            return Ok(ExtendedExecutionStatus::Complete(false));
        }
        // Freeze this statement's clock before anything anchors a transaction
        // to it, so `now()` and `statement_timestamp()` agree on a lone
        // statement as they do in PostgreSQL.
        datetime::begin_statement();
        self.ensure_txn(txn, TxnMode::Implicit, guc);
        let statement_mark =
            txn.statement_mark(self.wal.stage_mark(txn.txid), self.storage.lock_mark());
        emit_parse_warnings(&mut parser, responder)?;
        let outcome = self
            .execute_stmt(
                &statement, arena, params, txn, sqlprep, cursors, guc, responder,
            )?
            .and_then(|()| query::check_timeout());
        match outcome {
            Ok(()) => {
                if txn.mode == TxnMode::Implicit
                    && self.pending_copy.is_none()
                    && let Err(e) = self.commit_txn(txn, guc)
                {
                    responder.error(e.sqlstate, e.message.as_str())?;
                    return Ok(ExtendedExecutionStatus::Complete(false));
                }
                Ok(ExtendedExecutionStatus::Complete(true))
            }
            Err(mut e) => {
                if e.sqlstate == sqlstate::INTERNAL_LOCK_WAIT
                    || e.sqlstate == sqlstate::INTERNAL_IO_WAIT
                {
                    self.rollback_waiting_statement(txn, statement_mark);
                    if !lock_timeout_expired {
                        return Ok(ExtendedExecutionStatus::Blocked {
                            io_wait: e.sqlstate == sqlstate::INTERNAL_IO_WAIT,
                        });
                    }
                    self.storage
                        .rollback_locks_to(txn.txid, statement_mark.lock);
                    e = sql_err!(
                        sqlstate::LOCK_NOT_AVAILABLE,
                        "canceling statement due to lock timeout"
                    );
                }
                if txn.is_explicit() && e.sqlstate == sqlstate::DEADLOCK_DETECTED {
                    self.abort_explicit_txn(txn, guc);
                } else if txn.is_explicit() {
                    txn.failed = true;
                } else {
                    self.rollback_txn(txn, guc);
                }
                responder.error(e.sqlstate, e.message.as_str())?;
                Ok(ExtendedExecutionStatus::Complete(false))
            }
        }
    }

    /// Infers each `$n` parameter's type OID from how it is used, as
    /// PostgreSQL's parse analysis does — so a client that Describes a prepared
    /// statement (e.g. pgx) encodes its arguments in the right binary form.
    /// A parameter whose type cannot be determined defaults to `text`, and a
    /// client-supplied non-zero OID (from Parse) always wins. Returns the OIDs
    /// for `$1..$n_params`.
    pub fn infer_param_types(
        &self,
        text: &str,
        arena: &Arena,
        txn: &TxnState,
        client_oids: &[i32],
    ) -> [i32; MAX_BIND_PARAMS] {
        let mut oids = [types::oid::TEXT; MAX_BIND_PARAMS];
        let mut parser = match Parser::new(text, arena) {
            Ok(p) => p,
            Err(_) => return oids,
        };
        if let Ok(Some(statement)) = parser.next_stmt() {
            self.infer_stmt_params(&statement, txn.txid, &mut oids);
        }
        // A client's explicit (non-zero) parameter type overrides inference.
        for (i, &c) in client_oids.iter().enumerate().take(MAX_BIND_PARAMS) {
            if c != 0 {
                oids[i] = c;
            }
        }
        oids
    }

    /// The OID of a named column of a visible table, if resolvable.
    fn parameter_type_oid(&self, table: &ast::QualName, col: &str, txid: u32) -> Option<i32> {
        let slot = match self
            .storage
            .resolve_relation(table.schema, table.name, txid)
        {
            Some(crate::storage::ResolvedRelation::Table(slot)) => slot,
            _ => return None,
        };
        let def = self.storage.table_def(slot, txid);
        let index = def.column_index(col)?;
        Some(
            self.storage
                .declared_column_type(&def.columns()[index], txid)
                .expect("table column declared type is catalog-validated")
                .parameter_oid()
                .raw(),
        )
    }

    fn infer_stmt_params(&self, statement: &Stmt, txid: u32, oids: &mut [i32; MAX_BIND_PARAMS]) {
        let set = |oids: &mut [i32; MAX_BIND_PARAMS], e: &Expr, ty: i32| {
            if let Expr::Param(n) = e
                && *n >= 1
                && (*n as usize) <= MAX_BIND_PARAMS
            {
                oids[*n as usize - 1] = ty;
            }
        };
        match statement {
            Stmt::Explain { statement, .. } => {
                self.infer_stmt_params(statement, txid, oids);
            }
            Stmt::With { ctes, statement } => {
                for cte in *ctes {
                    match cte.dml {
                        Some(dml) => self.infer_stmt_params(dml, txid, oids),
                        None => self.infer_stmt_params(&Stmt::Select(*cte.query), txid, oids),
                    }
                }
                self.infer_stmt_params(statement, txid, oids);
            }
            Stmt::Insert(ins) => {
                let slot =
                    match self
                        .storage
                        .resolve_relation(ins.table.schema, ins.table.name, txid)
                    {
                        Some(crate::storage::ResolvedRelation::Table(slot)) => Some(slot),
                        _ => None,
                    };
                let def = slot.map(|s| self.storage.table_def(s, txid));
                for row in ins.rows {
                    for (i, value) in row.iter().enumerate() {
                        let ty = def.and_then(|d| {
                            let ci = if ins.columns.is_empty() {
                                (i < d.n_columns).then_some(i)
                            } else {
                                ins.columns.get(i).and_then(|c| d.column_index(c))
                            };
                            ci.map(|ci| {
                                self.storage
                                    .declared_column_type(&d.columns()[ci], txid)
                                    .expect("table column declared type is catalog-validated")
                                    .parameter_oid()
                                    .raw()
                            })
                        });
                        if let Some(ty) = ty {
                            set(oids, value, ty);
                        }
                    }
                }
            }
            Stmt::Update(u) => {
                for (col, value) in u.assignments {
                    if let Some(ty) = self.parameter_type_oid(&u.table, col, txid) {
                        set(oids, value, ty);
                    }
                }
                if let Some(w) = u.where_clause {
                    self.infer_where_params(&u.table, w, txid, oids);
                }
            }
            Stmt::Delete(d) => {
                if let Some(w) = d.where_clause {
                    self.infer_where_params(&d.table, w, txid, oids);
                }
            }
            Stmt::Select(s) => {
                // Single-table WHERE comparisons only (joins would need scope
                // resolution; those params stay text).
                if let (Some(from), Some(w)) = (&s.from, s.where_clause)
                    && from.joins.is_empty()
                    && from.base.subquery.is_none()
                {
                    let table = ast::QualName {
                        schema: from.base.schema,
                        name: from.base.table,
                    };
                    self.infer_where_params(&table, w, txid, oids);
                }
                // A parameter explicitly cast in the select list — `$n::type`
                // — takes that type, as PostgreSQL resolves an otherwise-unknown
                // parameter from the cast wrapping it.
                for item in s.items {
                    if let ast::SelectItem::Expr { expression, .. } = item {
                        Self::infer_cast_param(expression, oids);
                    }
                }
            }
            _ => {}
        }
    }

    /// Types a parameter written as `$n::type` (possibly through further casts)
    /// by the innermost cast wrapping it, as PostgreSQL resolves an otherwise-
    /// unknown parameter from the cast.
    fn infer_cast_param(expr: &Expr, oids: &mut [i32; MAX_BIND_PARAMS]) {
        if let Expr::Cast {
            operand, type_name, ..
        } = expr
        {
            if let Expr::Param(n) = operand {
                if *n >= 1
                    && (*n as usize) <= MAX_BIND_PARAMS
                    && let Some(ct) = types::ColType::from_sql_name(type_name)
                {
                    oids[*n as usize - 1] = ct.oid();
                }
            } else {
                Self::infer_cast_param(operand, oids);
            }
        }
    }

    /// Walks a single-table predicate, typing a `Column OP $n` (or the mirror)
    /// parameter from the column's type.
    fn infer_where_params(
        &self,
        table: &ast::QualName,
        expression: &Expr,
        txid: u32,
        oids: &mut [i32; MAX_BIND_PARAMS],
    ) {
        use ast::BinaryOp::*;
        if let Expr::Binary {
            operator,
            left,
            right,
        } = expression
        {
            match operator {
                And | Or => {
                    self.infer_where_params(table, left, txid, oids);
                    self.infer_where_params(table, right, txid, oids);
                }
                Eq | NotEq | Lt | LtEq | Gt | GtEq => {
                    let mut pair = |c: &Expr, p: &Expr| {
                        if let (Expr::Column { name, .. }, Expr::Param(n)) = (c, p)
                            && *n >= 1
                            && (*n as usize) <= MAX_BIND_PARAMS
                            && let Some(ty) = self.parameter_type_oid(table, name, txid)
                        {
                            oids[*n as usize - 1] = ty;
                        }
                    };
                    pair(left, right);
                    pair(right, left);
                }
                _ => {}
            }
        }
    }

    fn describe_data_modification(
        &self,
        statement: &Stmt,
        arena: &Arena,
        txn: &TxnState,
        responder: &mut Responder,
    ) -> Result<bool, WireFull> {
        let (target, returning) = match statement {
            Stmt::Insert(insert) => (insert.table, insert.returning),
            Stmt::Update(update) => (update.table, update.returning),
            Stmt::Delete(delete) => (delete.table, delete.returning),
            _ => {
                responder.no_data()?;
                return Ok(true);
            }
        };
        if returning.is_empty() {
            responder.no_data()?;
            return Ok(true);
        }
        let (target, returning) =
            match query::resolve_view_for_dml(&self.storage, target, txn.txid, arena) {
                Ok(Some(view)) => {
                    let rewritten = match query::rewrite_view_dml(
                        statement,
                        target.name,
                        view.base.name,
                        view.base.schema.expect("view base is qualified"),
                        view.columns,
                        &self.storage,
                        txn.txid,
                        arena,
                    ) {
                        Ok(rewritten) => rewritten,
                        Err(error) => {
                            responder.error(error.sqlstate, error.message.as_str())?;
                            return Ok(false);
                        }
                    };
                    let returning = match rewritten {
                        Stmt::Insert(insert) => insert.returning,
                        Stmt::Update(update) => update.returning,
                        Stmt::Delete(delete) => delete.returning,
                        _ => unreachable!("view rewrite keeps its statement kind"),
                    };
                    (view.base, returning)
                }
                Ok(None) => (target, returning),
                Err(error) => {
                    responder.error(error.sqlstate, error.message.as_str())?;
                    return Ok(false);
                }
            };
        let table_index = match exec::resolve_dml_table(&self.storage, &target, txn.txid) {
            Ok(table_index) => table_index,
            Err(error) => {
                responder.error(error.sqlstate, error.message.as_str())?;
                return Ok(false);
            }
        };
        let definition = *self.storage.table_def(table_index, txn.txid);
        let mut columns = [ColDesc::new("", 0, 0); MAX_PROJ];
        match query::describe_catalog_items(
            returning,
            Some(&definition),
            &self.storage,
            txn.txid,
            &mut columns,
        ) {
            Ok(count) => {
                responder.row_description(&columns[..count])?;
                Ok(true)
            }
            Err(error) => {
                responder.error(error.sqlstate, error.message.as_str())?;
                Ok(false)
            }
        }
    }

    /// Describe (statement or portal): RowDescription for SELECT/SHOW,
    /// NoData otherwise. Returns whether it succeeded.
    pub fn describe(
        &mut self,
        text: &str,
        arena: &Arena,
        txn: &TxnState,
        responder: &mut Responder,
    ) -> Result<bool, WireFull> {
        // responder already carries the portal's result-format flag when this is
        // a portal Describe (set by the caller).
        let mut parser = match Parser::new(text, arena) {
            Ok(p) => p,
            Err(e) => {
                report_parse_error(responder, &e)?;
                return Ok(false);
            }
        };
        let statement = match parser.next_stmt() {
            Ok(Some(statement)) => statement,
            Ok(None) => {
                responder.no_data()?;
                return Ok(true);
            }
            Err(e) => {
                report_parse_error(responder, &e)?;
                return Ok(false);
            }
        };
        match &statement {
            Stmt::Explain { .. } => {
                responder.row_description(&[ColDesc::new("QUERY PLAN", types::oid::TEXT, -1)])?;
                Ok(true)
            }
            Stmt::With { statement, .. } => {
                self.describe_data_modification(statement, arena, txn, responder)
            }
            Stmt::Insert(_) | Stmt::Update(_) | Stmt::Delete(_) => {
                self.describe_data_modification(&statement, arena, txn, responder)
            }
            Stmt::Select(s) => {
                // Describe the CTE-expanded query so derived columns resolve.
                let s = match query::expand_ctes(s, &self.storage, txn.txid, arena) {
                    Ok(x) => x,
                    Err(e) => {
                        responder.error(e.sqlstate, e.message.as_str())?;
                        return Ok(false);
                    }
                };
                let mut columns = [ColDesc::new("", 0, 0); MAX_PROJ];
                let described = match &s.from {
                    Some(from) => {
                        match query::QueryScope::resolve_schema(
                            &self.storage,
                            from,
                            txn.txid,
                            arena,
                        ) {
                            Ok(scope) => query::describe_select_items(
                                s.items,
                                Some(&scope),
                                &self.storage,
                                txn.txid,
                                arena,
                                &mut columns,
                            ),
                            Err(e) => {
                                responder.error(e.sqlstate, e.message.as_str())?;
                                return Ok(false);
                            }
                        }
                    }
                    None => query::describe_select_items(
                        s.items,
                        None,
                        &self.storage,
                        txn.txid,
                        arena,
                        &mut columns,
                    ),
                };
                match described {
                    Ok(n) => {
                        responder.row_description(&columns[..n])?;
                        Ok(true)
                    }
                    Err(e) => {
                        responder.error(e.sqlstate, e.message.as_str())?;
                        Ok(false)
                    }
                }
            }
            Stmt::SetQuery(q) => {
                let mut columns = [ColDesc::new("", 0, 0); MAX_PROJ];
                match query::describe_set_query(&self.storage, txn.txid, q, &mut columns, arena) {
                    Ok(n) => {
                        responder.row_description(&columns[..n])?;
                        Ok(true)
                    }
                    Err(e) => {
                        responder.error(e.sqlstate, e.message.as_str())?;
                        Ok(false)
                    }
                }
            }
            Stmt::Show(name) => {
                responder.row_description(&[ColDesc::new(name, types::oid::TEXT, -1)])?;
                Ok(true)
            }
            _ => {
                responder.no_data()?;
                Ok(true)
            }
        }
    }

    /// Runs a statement's data-modifying CTEs (`WITH x AS (INSERT/UPDATE/DELETE
    /// ... RETURNING ...)`) once each, capturing each RETURNING output as a
    /// materialized relation the main query binds by name. Runs under this
    /// statement's command snapshot, so the CTEs' base-table changes are not
    /// visible to sibling CTEs or the main query except through these relations
    /// (matching PostgreSQL's single-snapshot rule). Returns `None` when the
    /// statement has no data-modifying CTE, so the ordinary path is unchanged.
    #[allow(clippy::too_many_arguments)]
    fn run_dml_ctes<'a>(
        &mut self,
        with: &'a [ast::Cte<'a>],
        txn: &mut TxnState,
        arena: &'a Arena,
        params: &[Datum<'a>],
        guc: &mut GucState,
        responder: &mut Responder,
    ) -> Result<Option<&'a [(&'a str, &'a ast::MaterializedCte<'a>)]>, SqlError> {
        use crate::sql::exec::{MAX_PROJ, encode_projected_pub};
        use crate::sql::types::ColDesc;
        if !with.iter().any(|c| c.dml.is_some()) {
            return Ok(None);
        }
        // Analysis precedes every side effect: a duplicate name or an
        // over-wide output rename list must not let an earlier DML CTE run
        // before the statement fails.
        for (index, cte) in with.iter().enumerate() {
            if with[..index].iter().any(|prior| prior.name == cte.name) {
                return Err(sql_err!(
                    sqlstate::DUPLICATE_ALIAS,
                    "WITH query name \"{}\" specified more than once",
                    cte.name
                ));
            }
        }
        // All of this statement's sub-parts share one command snapshot.
        self.storage.set_read_snapshot(txn.command_id());
        let mut mats: [(&'a str, &'a ast::MaterializedCte<'a>); parser::MAX_CTES] =
            [("", &EMPTY_DML_CTE); parser::MAX_CTES];
        let mut n = 0;
        for (cte_index, cte) in with.iter().enumerate() {
            let Some(dml) = cte.dml else { continue };
            // Earlier ordinary, recursive, and data-modifying CTEs are in
            // scope inside this CTE body. Expansion finishes its immutable
            // catalog work before the statement takes a mutable storage borrow.
            let dml = query::expand_dml_ctes(
                dml,
                &with[..cte_index],
                &self.storage,
                txn.txid,
                arena,
                params,
                &mats[..n],
            )?;
            let (target, returning) = match dml {
                Stmt::Insert(i) => (&i.table, i.returning),
                Stmt::Update(u) => (&u.table, u.returning),
                Stmt::Delete(d) => (&d.table, d.returning),
                _ => {
                    return Err(sql_err!(
                        sqlstate::FEATURE_NOT_SUPPORTED,
                        "a data-modifying WITH sub-statement must be INSERT, UPDATE or DELETE"
                    ));
                }
            };
            // Describe the RETURNING columns against the target table, applying
            // the CTE's optional rename list.
            let described_target =
                match query::resolve_view_for_dml(&self.storage, *target, txn.txid, arena)? {
                    Some(view) => view.base,
                    None => *target,
                };
            let idx =
                crate::sql::exec::resolve_dml_table(&self.storage, &described_target, txn.txid)?;
            let def = *self.storage.table_def(idx, txn.txid);
            let mut descs = [ColDesc::new("", 0, 0); MAX_PROJ];
            let ncols = query::describe_catalog_items(
                returning,
                Some(&def),
                &self.storage,
                txn.txid,
                &mut descs,
            )?;
            if cte.columns.len() > ncols {
                return Err(sql_err!(
                    sqlstate::INVALID_COLUMN_REFERENCE,
                    "WITH query \"{}\" has {} columns available but {} columns specified",
                    cte.name,
                    ncols,
                    cte.columns.len()
                ));
            }
            let mut names: [&str; MAX_PROJ] = [""; MAX_PROJ];
            let mut types = [(0i32, 0i16, -1i32); MAX_PROJ];
            for i in 0..ncols {
                // Copy the name into the statement arena: a described column
                // name borrows the (local, owned) table def, which drops here.
                let nm = cte.columns.get(i).copied().unwrap_or(descs[i].name);
                names[i] = arena.alloc_str(nm).map_err(|_| query::arena_full_pub())?;
                types[i] = (descs[i].type_oid, descs[i].typlen, descs[i].type_mod);
            }
            let column_names = arena
                .alloc_slice_copy(&names[..ncols])
                .map_err(|_| query::arena_full_pub())?;
            let column_types = arena
                .alloc_slice_copy(&types[..ncols])
                .map_err(|_| query::arena_full_pub())?;
            let column_collations = arena
                .alloc_slice_with(ncols, |index| descs[index].collation)
                .map_err(|_| query::arena_full_pub())?;
            // Run the DML once, capturing RETURNING rows (projected-encoded).
            const EMPTY: &[u8] = &[];
            let mut store: *mut &[u8] = core::ptr::null_mut();
            let mut len = 0usize;
            let mut cap = 0usize;
            let mut sink = |vals: &[Datum]| -> Result<(), SqlError> {
                let enc = encode_projected_pub(vals, arena)?;
                if len == cap {
                    let new_cap = if cap == 0 { 8 } else { cap * 2 };
                    let fresh: &mut [&[u8]] = arena
                        .alloc_slice_with(new_cap, |_| EMPTY)
                        .map_err(|_| query::arena_full_pub())?;
                    if len > 0 {
                        let old = unsafe { core::slice::from_raw_parts(store, len) };
                        fresh[..len].copy_from_slice(old);
                    }
                    store = fresh.as_mut_ptr();
                    cap = new_cap;
                }
                unsafe { store.add(len).write(enc) };
                len += 1;
                Ok(())
            };
            let outcome = Self::execute_data_modification(
                &mut self.storage,
                &mut self.scratch,
                &self.work,
                dml,
                txn,
                params,
                guc,
                responder,
                Some(&mut sink),
            );
            match outcome {
                Ok(Ok(())) => {}
                Ok(Err(e)) => return Err(e),
                Err(_) => return Err(query::arena_full_pub()),
            }
            let rows: &'a [&'a [u8]] = if len == 0 {
                &[]
            } else {
                unsafe { core::slice::from_raw_parts(store, len) }
            };
            let mcte = arena
                .alloc(ast::MaterializedCte {
                    column_names,
                    column_types,
                    column_collations,
                    rows,
                    external_run: None,
                })
                .map_err(|_| query::arena_full_pub())?;
            if n == parser::MAX_CTES {
                return Err(sql_err!(
                    sqlstate::TOO_MANY_ARGUMENTS,
                    "too many WITH entries"
                ));
            }
            mats[n] = (cte.name, &*mcte);
            n += 1;
        }
        Ok(Some(
            arena
                .alloc_slice_copy(&mats[..n])
                .map_err(|_| query::arena_full_pub())?,
        ))
    }

    /// Executes one INSERT/UPDATE/DELETE after any enclosing WITH clause has
    /// been expanded. View rewriting lives here as well, so a data-modifying
    /// CTE and a main DML statement have exactly the same target semantics.
    #[allow(clippy::too_many_arguments, clippy::type_complexity)]
    fn execute_data_modification<'a, 'capture>(
        storage: &mut Storage,
        scratch: &mut FixedVec<(u64, RowHome)>,
        arena: &Arena,
        statement: &'a Stmt<'a>,
        txn: &mut TxnState,
        params: &[Datum<'a>],
        guc: &mut GucState,
        responder: &mut Responder,
        capture: Option<&'capture mut ReturningCapture<'capture>>,
    ) -> Result<Result<(), SqlError>, WireFull> {
        match statement {
            Stmt::Insert(insert) => {
                let insert =
                    match query::resolve_view_for_dml(storage, insert.table, txn.txid, arena) {
                        Ok(Some(view)) => {
                            let rewritten = match query::rewrite_view_dml(
                                statement,
                                insert.table.name,
                                view.base.name,
                                view.base.schema.expect("view base is qualified"),
                                view.columns,
                                storage,
                                txn.txid,
                                arena,
                            ) {
                                Ok(Stmt::Insert(rewritten)) => rewritten,
                                Ok(_) => unreachable!("insert rewrite keeps its statement kind"),
                                Err(error) => return Ok(Err(error)),
                            };
                            let columns = if rewritten.columns.is_empty() {
                                view.columns
                            } else {
                                rewritten.columns
                            };
                            match arena.alloc(Insert {
                                table: view.base,
                                columns,
                                rows: rewritten.rows,
                                select: rewritten.select,
                                on_conflict: rewritten.on_conflict,
                                returning: rewritten.returning,
                                overriding: rewritten.overriding,
                            }) {
                                Ok(rewritten) => &*rewritten,
                                Err(_) => return Ok(Err(query::arena_full_pub())),
                            }
                        }
                        Ok(None) => insert,
                        Err(error) => return Ok(Err(error)),
                    };
                exec::insert(
                    storage,
                    txn,
                    scratch,
                    insert,
                    arena,
                    params,
                    guc.seq_session(),
                    responder,
                    capture,
                    None,
                    None,
                    None,
                    exec::InsertSource::Statement,
                )
            }
            Stmt::Update(update) => {
                let update =
                    match query::resolve_view_for_dml(storage, update.table, txn.txid, arena) {
                        Ok(Some(view)) => {
                            let rewritten = match query::rewrite_view_dml(
                                statement,
                                update.table.name,
                                view.base.name,
                                view.base.schema.expect("view base is qualified"),
                                view.columns,
                                storage,
                                txn.txid,
                                arena,
                            ) {
                                Ok(Stmt::Update(rewritten)) => rewritten,
                                Ok(_) => unreachable!("update rewrite keeps its statement kind"),
                                Err(error) => return Ok(Err(error)),
                            };
                            let where_clause = match query::and_where(
                                view.where_clause,
                                rewritten.where_clause,
                                arena,
                            ) {
                                Ok(where_clause) => where_clause,
                                Err(error) => return Ok(Err(error)),
                            };
                            match arena.alloc(Update {
                                table: view.base,
                                alias: update.alias,
                                assignments: rewritten.assignments,
                                from: rewritten.from,
                                where_clause,
                                returning: rewritten.returning,
                            }) {
                                Ok(rewritten) => &*rewritten,
                                Err(_) => return Ok(Err(query::arena_full_pub())),
                            }
                        }
                        Ok(None) => update,
                        Err(error) => return Ok(Err(error)),
                    };
                exec::update(
                    storage,
                    txn,
                    scratch,
                    update,
                    arena,
                    params,
                    guc.seq_session(),
                    responder,
                    capture,
                    None,
                )
            }
            Stmt::Delete(delete) => {
                let delete =
                    match query::resolve_view_for_dml(storage, delete.table, txn.txid, arena) {
                        Ok(Some(view)) => {
                            let rewritten = match query::rewrite_view_dml(
                                statement,
                                delete.table.name,
                                view.base.name,
                                view.base.schema.expect("view base is qualified"),
                                view.columns,
                                storage,
                                txn.txid,
                                arena,
                            ) {
                                Ok(Stmt::Delete(rewritten)) => rewritten,
                                Ok(_) => unreachable!("delete rewrite keeps its statement kind"),
                                Err(error) => return Ok(Err(error)),
                            };
                            let where_clause = match query::and_where(
                                view.where_clause,
                                rewritten.where_clause,
                                arena,
                            ) {
                                Ok(where_clause) => where_clause,
                                Err(error) => return Ok(Err(error)),
                            };
                            match arena.alloc(Delete {
                                table: view.base,
                                alias: delete.alias,
                                using: rewritten.using,
                                where_clause,
                                returning: rewritten.returning,
                            }) {
                                Ok(rewritten) => &*rewritten,
                                Err(_) => return Ok(Err(query::arena_full_pub())),
                            }
                        }
                        Ok(None) => delete,
                        Err(error) => return Ok(Err(error)),
                    };
                exec::delete(
                    storage,
                    txn,
                    scratch,
                    delete,
                    arena,
                    params,
                    guc.seq_session(),
                    responder,
                    capture,
                    None,
                )
            }
            _ => Ok(Err(sql_err!(
                sqlstate::INTERNAL_ERROR,
                "expected a data-modifying statement"
            ))),
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn execute_select(
        &mut self,
        statement: &ast::Select<'_>,
        arena: &Arena,
        params: &[Datum],
        txn: &mut TxnState,
        sqlprep: &mut SqlPreparedPool,
        cursors: &mut cursor::CursorPool,
        guc: &mut GucState,
        responder: &mut Responder,
    ) -> Result<Result<(), SqlError>, WireFull> {
        let invocations = query::RoutineInvocationState::new();
        let sequence_state = sequence::SequenceReplayState::new();
        let source_snapshot = txn.command_id().saturating_add(1);
        loop {
            self.work.reset();
            self.storage.set_read_snapshot(source_snapshot);
            invocations.begin_attempt();
            sequence_state.begin_attempt();
            let output_mark = responder.buffer.mark();
            let outcome = self.execute_select_once(
                statement,
                arena,
                params,
                txn,
                guc,
                Some(&invocations),
                Some(&sequence_state),
                responder,
            )?;
            let Err(error) = outcome else {
                return Ok(outcome);
            };
            if error.sqlstate != sqlstate::INTERNAL_ROUTINE_INVOCATION {
                return Ok(Err(error));
            }
            responder.buffer.truncate_to(output_mark);
            let Some(pending) = invocations.take_pending() else {
                return Ok(Err(sql_err!(
                    sqlstate::INTERNAL_ERROR,
                    "routine invocation yielded without a pending call"
                )));
            };
            if let Err(error) = self.complete_pending_routine(
                pending,
                &invocations,
                arena,
                txn,
                sqlprep,
                cursors,
                guc,
                responder,
            )? {
                return Ok(Err(error));
            }
        }
    }

    /// Runs a suspended mutable function and records its typed result in the
    /// statement-owned invocation log before the enclosing expression restarts.
    #[allow(clippy::too_many_arguments)]
    fn complete_pending_routine<'a>(
        &mut self,
        pending: query::PendingRoutineInvocation<'a>,
        invocations: &query::RoutineInvocationState<'a>,
        arena: &'a Arena,
        txn: &mut TxnState,
        sqlprep: &mut SqlPreparedPool,
        cursors: &mut cursor::CursorPool,
        guc: &mut GucState,
        responder: &mut Responder,
    ) -> Result<Result<(), SqlError>, WireFull> {
        if self.storage.routine(pending.slot).kind.is_set_returning() {
            let rows = match self.execute_pending_table_routine(
                pending, arena, txn, sqlprep, cursors, guc, responder,
            )? {
                Ok(rows) => rows,
                Err(error) => return Ok(Err(error)),
            };
            return Ok(invocations.complete_rows(rows));
        }
        let value = match self
            .execute_pending_scalar_routine(pending, arena, txn, sqlprep, cursors, guc, responder)?
        {
            Ok(value) => value,
            Err(error) => return Ok(Err(error)),
        };
        Ok(invocations.complete(value))
    }

    #[allow(clippy::too_many_arguments)]
    fn execute_select_once<'statement>(
        &mut self,
        statement: &ast::Select<'_>,
        arena: &'statement Arena,
        params: &[Datum],
        txn: &mut TxnState,
        guc: &mut GucState,
        invocations: Option<&'statement query::RoutineInvocationState<'statement>>,
        sequence_state: Option<&sequence::SequenceReplayState>,
        responder: &mut Responder,
    ) -> Result<Result<(), SqlError>, WireFull> {
        let _routine_invocation_scope = query::enter_routine_invocation_scope(
            invocations.map(|invocations| query::RoutineInvocationContext::new(invocations, arena)),
        );
        let dml_mats = match self.run_dml_ctes(statement.with, txn, arena, params, guc, responder) {
            Ok(materialized) => materialized.unwrap_or(&[]),
            Err(error) => return Ok(Err(error)),
        };
        let statement = match query::expand_ctes_exec(
            statement,
            &self.storage,
            txn.txid,
            &self.work,
            params,
            dml_mats,
        ) {
            Ok(expanded) => expanded,
            Err(error) => return Ok(Err(error)),
        };
        if let Err(error) = query::validate_locking(statement) {
            return Ok(Err(error));
        }
        let base_sequence = sequence::SeqEval::new(&self.storage, guc.seq_session(), txn.txid);
        let replay_sequence =
            sequence_state.map(|state| sequence::ReplaySeqEval::new(base_sequence, state));
        let sequence: &dyn SequenceAccess = replay_sequence
            .as_ref()
            .map(|sequence| sequence as &dyn SequenceAccess)
            .unwrap_or(&base_sequence);
        if statement.from.is_none() {
            query::constant_select_resumable(
                &self.storage,
                txn.txid,
                statement,
                &self.work,
                params,
                Some(sequence),
                invocations,
                invocations.map(|_| arena),
                responder,
            )
        } else {
            query::select_query_resumable(
                &self.storage,
                txn.txid,
                statement,
                &self.work,
                params,
                Some(sequence),
                invocations,
                invocations.map(|_| arena),
                responder,
            )
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn execute_scalar_routine_program<'a>(
        &mut self,
        slot: usize,
        routine: crate::storage::RoutineDef,
        result_type: ColType,
        program: query::RoutineFunctionProgram<'a>,
        arguments: &[Datum<'a>],
        arena: &'a Arena,
        txn: &mut TxnState,
        sqlprep: &mut SqlPreparedPool,
        cursors: &mut cursor::CursorPool,
        guc: &mut GucState,
        responder: &mut Responder,
    ) -> Result<Result<Datum<'a>, SqlError>, WireFull> {
        if let Err(error) = self.storage.require_routine_execute(slot, txn.txid) {
            return Ok(Err(error));
        }
        let _formal_scope = exec::enter_routine_parameter_types(routine.arguments());
        let nested_invocations = query::RoutineInvocationState::new();
        nested_invocations.begin_attempt();
        let _routine_invocation_scope = query::enter_routine_invocation_scope(Some(
            query::RoutineInvocationContext::new(&nested_invocations, arena),
        ));
        let output_mark = responder.buffer.mark();
        for step in program.preceding {
            let statement = match step {
                query::RoutinePrelude::Statement(statement) => statement,
                query::RoutinePrelude::Forbidden(forbidden) => {
                    responder.buffer.truncate_to(output_mark);
                    return Ok(Err(query::routine_forbidden_statement_error(forbidden)));
                }
            };
            self.work.reset();
            match self.execute_routine_stmt(
                statement, arena, arguments, txn, sqlprep, cursors, guc, responder, None,
            ) {
                Ok(Ok(())) => responder.buffer.truncate_to(output_mark),
                Ok(Err(error)) => {
                    responder.buffer.truncate_to(output_mark);
                    return Ok(Err(error));
                }
                Err(error) => {
                    responder.buffer.truncate_to(output_mark);
                    return Err(error);
                }
            }
        }
        let result = core::cell::Cell::new(Datum::Null);
        let has_result = core::cell::Cell::new(false);
        let mut capture_result = |row: &[Datum]| {
            if row.len() != 1 {
                return Err(sql_err!(
                    sqlstate::SYNTAX_ERROR,
                    "SQL function query must return one column"
                ));
            }
            if !has_result.get() {
                let encoded = exec::encode_projected_pub(row, arena)?;
                result.set(exec::decode_projected_pub(encoded, 0));
                has_result.set(true);
            }
            Ok(())
        };
        let outcome = match program.result {
            query::RoutineFunctionResult::Query(result_query) => loop {
                self.work.reset();
                nested_invocations.begin_attempt();
                let outcome = query::execute_routine_query(
                    result_query,
                    &self.storage,
                    txn.txid,
                    &self.work,
                    arguments,
                    true,
                    &mut capture_result,
                );
                let Err(error) = outcome else { break outcome };
                if error.sqlstate != sqlstate::INTERNAL_ROUTINE_INVOCATION {
                    break Err(error);
                }
                let Some(pending) = nested_invocations.take_pending() else {
                    break Err(sql_err!(
                        sqlstate::INTERNAL_ERROR,
                        "nested routine invocation yielded without a pending call"
                    ));
                };
                if let Err(error) = self.complete_pending_routine(
                    pending,
                    &nested_invocations,
                    arena,
                    txn,
                    sqlprep,
                    cursors,
                    guc,
                    responder,
                )? {
                    break Err(error);
                }
            },
            query::RoutineFunctionResult::DataModification(statement) => loop {
                self.work.reset();
                nested_invocations.begin_attempt();
                has_result.set(false);
                let mark =
                    txn.statement_mark(self.wal.stage_mark(txn.txid), self.storage.lock_mark());
                let outcome = self.execute_routine_stmt(
                    statement,
                    arena,
                    arguments,
                    txn,
                    sqlprep,
                    cursors,
                    guc,
                    responder,
                    Some(&mut capture_result),
                )?;
                let Err(error) = outcome else {
                    responder.buffer.truncate_to(output_mark);
                    break Ok(());
                };
                if error.sqlstate != sqlstate::INTERNAL_ROUTINE_INVOCATION {
                    responder.buffer.truncate_to(output_mark);
                    break Err(error);
                }
                self.rollback_waiting_statement(txn, mark);
                responder.buffer.truncate_to(output_mark);
                let Some(pending) = nested_invocations.take_pending() else {
                    break Err(sql_err!(
                        sqlstate::INTERNAL_ERROR,
                        "nested routine invocation yielded without a pending call"
                    ));
                };
                if let Err(error) = self.complete_pending_routine(
                    pending,
                    &nested_invocations,
                    arena,
                    txn,
                    sqlprep,
                    cursors,
                    guc,
                    responder,
                )? {
                    break Err(error);
                }
            },
            query::RoutineFunctionResult::Void(statement) => loop {
                self.work.reset();
                nested_invocations.begin_attempt();
                let mark =
                    txn.statement_mark(self.wal.stage_mark(txn.txid), self.storage.lock_mark());
                let outcome = self.execute_routine_stmt(
                    statement, arena, arguments, txn, sqlprep, cursors, guc, responder, None,
                )?;
                let Err(error) = outcome else {
                    responder.buffer.truncate_to(output_mark);
                    break Ok(());
                };
                if error.sqlstate != sqlstate::INTERNAL_ROUTINE_INVOCATION {
                    responder.buffer.truncate_to(output_mark);
                    break Err(error);
                }
                self.rollback_waiting_statement(txn, mark);
                responder.buffer.truncate_to(output_mark);
                let Some(pending) = nested_invocations.take_pending() else {
                    break Err(sql_err!(
                        sqlstate::INTERNAL_ERROR,
                        "nested routine invocation yielded without a pending call"
                    ));
                };
                if let Err(error) = self.complete_pending_routine(
                    pending,
                    &nested_invocations,
                    arena,
                    txn,
                    sqlprep,
                    cursors,
                    guc,
                    responder,
                )? {
                    break Err(error);
                }
            },
            query::RoutineFunctionResult::Forbidden(statement) => {
                Err(query::routine_forbidden_statement_error(statement))
            }
        };
        if let Err(error) = outcome {
            responder.buffer.truncate_to(output_mark);
            return Ok(Err(error));
        }
        let value = match eval::cast_to(result.get(), result_type, arena) {
            Ok(value) => value,
            Err(error) => {
                responder.buffer.truncate_to(output_mark);
                return Ok(Err(error));
            }
        };
        responder.buffer.truncate_to(output_mark);
        Ok(Ok(value))
    }

    #[allow(clippy::too_many_arguments)]
    fn execute_pending_scalar_routine<'a>(
        &mut self,
        pending: query::PendingRoutineInvocation<'a>,
        arena: &'a Arena,
        txn: &mut TxnState,
        sqlprep: &mut SqlPreparedPool,
        cursors: &mut cursor::CursorPool,
        guc: &mut GucState,
        responder: &mut Responder,
    ) -> Result<Result<Datum<'a>, SqlError>, WireFull> {
        let routine = *self.storage.routine(pending.slot);
        let Some(result_type) = routine.kind.function_result() else {
            return Ok(Err(sql_err!(
                sqlstate::INTERNAL_ERROR,
                "scalar routine resolved to a procedure"
            )));
        };
        if routine.kind.is_set_returning() {
            return Ok(Err(sql_err!(
                sqlstate::INTERNAL_ERROR,
                "scalar routine resolved to a set-returning function"
            )));
        }
        let body = match arena.alloc_str(routine.body.as_str()) {
            Ok(body) => body,
            Err(_) => {
                return Ok(Err(sql_err!(
                    sqlstate::PROGRAM_LIMIT_EXCEEDED,
                    "statement arena exhausted while invoking SQL function"
                )));
            }
        };
        let program = match query::parse_routine_function_program(
            body,
            arena,
            result_type == ColType::Void,
        ) {
            Ok(program) => program,
            Err(error) => return Ok(Err(error)),
        };
        let mut arguments = [Datum::Null; crate::storage::MAX_ROUTINE_ARGUMENTS];
        if pending.argument_count > arguments.len() {
            return Ok(Err(sql_err!(
                sqlstate::PROGRAM_LIMIT_EXCEEDED,
                "too many function arguments"
            )));
        }
        for (index, argument) in arguments
            .iter_mut()
            .enumerate()
            .take(pending.argument_count)
        {
            *argument = exec::decode_projected_pub(pending.arguments, index);
        }
        self.execute_scalar_routine_program(
            pending.slot,
            routine,
            result_type,
            program,
            &arguments[..pending.argument_count],
            arena,
            txn,
            sqlprep,
            cursors,
            guc,
            responder,
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn execute_pending_table_routine<'a>(
        &mut self,
        pending: query::PendingRoutineInvocation<'a>,
        arena: &'a Arena,
        txn: &mut TxnState,
        sqlprep: &mut SqlPreparedPool,
        cursors: &mut cursor::CursorPool,
        guc: &mut GucState,
        responder: &mut Responder,
    ) -> Result<Result<&'a [&'a [u8]], SqlError>, WireFull> {
        let routine = *self.storage.routine(pending.slot);
        if !routine.kind.is_set_returning() {
            return Ok(Err(sql_err!(
                sqlstate::INTERNAL_ERROR,
                "table routine invocation resolved to a scalar function"
            )));
        }
        if let Err(error) = self.storage.require_routine_execute(pending.slot, txn.txid) {
            return Ok(Err(error));
        }
        let body = match arena.alloc_str(routine.body.as_str()) {
            Ok(body) => body,
            Err(_) => {
                return Ok(Err(sql_err!(
                    sqlstate::PROGRAM_LIMIT_EXCEEDED,
                    "statement arena exhausted while invoking SQL table function"
                )));
            }
        };
        let result_type = routine.kind.function_result().expect("set routine result");
        let program = match query::parse_routine_function_program(
            body,
            arena,
            result_type == ColType::Void,
        ) {
            Ok(program) => program,
            Err(error) => return Ok(Err(error)),
        };
        let mut arguments = [Datum::Null; crate::storage::MAX_ROUTINE_ARGUMENTS];
        if pending.argument_count > arguments.len() {
            return Ok(Err(sql_err!(
                sqlstate::PROGRAM_LIMIT_EXCEEDED,
                "too many function arguments"
            )));
        }
        for (index, argument) in arguments
            .iter_mut()
            .enumerate()
            .take(pending.argument_count)
        {
            *argument = exec::decode_projected_pub(pending.arguments, index);
        }
        let _formal_scope = exec::enter_routine_parameter_types(routine.arguments());
        let nested_invocations = query::RoutineInvocationState::new();
        nested_invocations.begin_attempt();
        let _routine_invocation_scope = query::enter_routine_invocation_scope(Some(
            query::RoutineInvocationContext::new(&nested_invocations, arena),
        ));
        let output_mark = responder.buffer.mark();
        for step in program.preceding {
            let query::RoutinePrelude::Statement(statement) = step else {
                let query::RoutinePrelude::Forbidden(statement) = step else {
                    unreachable!("routine prelude has two variants");
                };
                responder.buffer.truncate_to(output_mark);
                return Ok(Err(query::routine_forbidden_statement_error(statement)));
            };
            self.work.reset();
            match self.execute_routine_stmt(
                statement,
                arena,
                &arguments[..pending.argument_count],
                txn,
                sqlprep,
                cursors,
                guc,
                responder,
                None,
            ) {
                Ok(Ok(())) => responder.buffer.truncate_to(output_mark),
                Ok(Err(error)) => {
                    responder.buffer.truncate_to(output_mark);
                    return Ok(Err(error));
                }
                Err(error) => {
                    responder.buffer.truncate_to(output_mark);
                    return Err(error);
                }
            }
        }
        const EMPTY: &[u8] = &[];
        let table_columns = routine.table_columns();
        let rows = core::cell::Cell::new(core::ptr::null_mut::<&[u8]>());
        let len = core::cell::Cell::new(0usize);
        let cap = core::cell::Cell::new(0usize);
        let mut capture_rows = |values: &[Datum]| {
            let expected_columns = table_columns.map_or(1, <[_]>::len);
            if values.len() != expected_columns {
                return Err(sql_err!(
                    sqlstate::SYNTAX_ERROR,
                    "SQL function query must return {} column{}",
                    expected_columns,
                    if expected_columns == 1 { "" } else { "s" }
                ));
            }
            let encoded = if let Some(output) = table_columns {
                let mut cast = [Datum::Null; crate::storage::MAX_COLUMNS];
                for (slot, column) in output.iter().enumerate() {
                    let projected = exec::encode_projected_pub(&[values[slot]], arena)?;
                    cast[slot] = eval::cast_to(
                        exec::decode_projected_pub(projected, 0),
                        column.ctype,
                        arena,
                    )?;
                }
                exec::encode_projected_pub(&cast[..output.len()], arena)?
            } else {
                let projected = exec::encode_projected_pub(values, arena)?;
                let value =
                    eval::cast_to(exec::decode_projected_pub(projected, 0), result_type, arena)?;
                exec::encode_projected_pub(&[value], arena)?
            };
            if len.get() == cap.get() {
                let new_cap = if cap.get() == 0 { 8 } else { cap.get() * 2 };
                let fresh = arena.alloc_slice_with(new_cap, |_| EMPTY).map_err(|_| {
                    sql_err!(
                        sqlstate::PROGRAM_LIMIT_EXCEEDED,
                        "statement arena exhausted while materializing SQL table function"
                    )
                })?;
                if len.get() > 0 {
                    let prior = unsafe { core::slice::from_raw_parts(rows.get(), len.get()) };
                    fresh[..len.get()].copy_from_slice(prior);
                }
                rows.set(fresh.as_mut_ptr());
                cap.set(new_cap);
            }
            unsafe { rows.get().add(len.get()).write(encoded) };
            len.set(len.get() + 1);
            Ok(())
        };
        let outcome = match program.result {
            query::RoutineFunctionResult::Query(result) => loop {
                self.work.reset();
                nested_invocations.begin_attempt();
                len.set(0);
                let outcome = query::execute_routine_query(
                    result,
                    &self.storage,
                    txn.txid,
                    &self.work,
                    &arguments[..pending.argument_count],
                    true,
                    &mut capture_rows,
                );
                let Err(error) = outcome else { break outcome };
                if error.sqlstate != sqlstate::INTERNAL_ROUTINE_INVOCATION {
                    break Err(error);
                }
                let Some(pending) = nested_invocations.take_pending() else {
                    break Err(sql_err!(
                        sqlstate::INTERNAL_ERROR,
                        "nested routine invocation yielded without a pending call"
                    ));
                };
                if let Err(error) = self.complete_pending_routine(
                    pending,
                    &nested_invocations,
                    arena,
                    txn,
                    sqlprep,
                    cursors,
                    guc,
                    responder,
                )? {
                    break Err(error);
                }
            },
            query::RoutineFunctionResult::DataModification(statement) => loop {
                self.work.reset();
                nested_invocations.begin_attempt();
                len.set(0);
                let mark =
                    txn.statement_mark(self.wal.stage_mark(txn.txid), self.storage.lock_mark());
                let outcome = self.execute_routine_stmt(
                    statement,
                    arena,
                    &arguments[..pending.argument_count],
                    txn,
                    sqlprep,
                    cursors,
                    guc,
                    responder,
                    Some(&mut capture_rows),
                )?;
                let Err(error) = outcome else { break Ok(()) };
                if error.sqlstate != sqlstate::INTERNAL_ROUTINE_INVOCATION {
                    break Err(error);
                }
                self.rollback_waiting_statement(txn, mark);
                let Some(pending) = nested_invocations.take_pending() else {
                    break Err(sql_err!(
                        sqlstate::INTERNAL_ERROR,
                        "nested routine invocation yielded without a pending call"
                    ));
                };
                if let Err(error) = self.complete_pending_routine(
                    pending,
                    &nested_invocations,
                    arena,
                    txn,
                    sqlprep,
                    cursors,
                    guc,
                    responder,
                )? {
                    break Err(error);
                }
            },
            query::RoutineFunctionResult::Void(statement) => loop {
                self.work.reset();
                nested_invocations.begin_attempt();
                let mark =
                    txn.statement_mark(self.wal.stage_mark(txn.txid), self.storage.lock_mark());
                let outcome = self.execute_routine_stmt(
                    statement,
                    arena,
                    &arguments[..pending.argument_count],
                    txn,
                    sqlprep,
                    cursors,
                    guc,
                    responder,
                    None,
                )?;
                let Err(error) = outcome else { break Ok(()) };
                if error.sqlstate != sqlstate::INTERNAL_ROUTINE_INVOCATION {
                    break Err(error);
                }
                self.rollback_waiting_statement(txn, mark);
                let Some(pending) = nested_invocations.take_pending() else {
                    break Err(sql_err!(
                        sqlstate::INTERNAL_ERROR,
                        "nested routine invocation yielded without a pending call"
                    ));
                };
                if let Err(error) = self.complete_pending_routine(
                    pending,
                    &nested_invocations,
                    arena,
                    txn,
                    sqlprep,
                    cursors,
                    guc,
                    responder,
                )? {
                    break Err(error);
                }
            },
            query::RoutineFunctionResult::Forbidden(statement) => {
                Err(query::routine_forbidden_statement_error(statement))
            }
        };
        responder.buffer.truncate_to(output_mark);
        if let Err(error) = outcome {
            return Ok(Err(error));
        }
        Ok(Ok(if len.get() == 0 {
            &[]
        } else {
            unsafe { core::slice::from_raw_parts(rows.get(), len.get()) }
        }))
    }

    fn execute_explained_statement(
        &mut self,
        statement: &Stmt<'_>,
        arena: &Arena,
        params: &[Datum],
        txn: &mut TxnState,
        guc: &mut GucState,
        responder: &mut Responder,
    ) -> Result<Result<(), SqlError>, WireFull> {
        match statement {
            Stmt::Select(select) => {
                self.execute_select_once(select, arena, params, txn, guc, None, None, responder)
            }
            Stmt::SetQuery(query) => {
                let sequence = sequence::SeqEval::new(&self.storage, guc.seq_session(), txn.txid);
                query::set_query(
                    &self.storage,
                    txn.txid,
                    query,
                    &self.work,
                    params,
                    Some(&sequence),
                    responder,
                )
            }
            Stmt::Insert(_) | Stmt::Update(_) | Stmt::Delete(_) => Self::execute_data_modification(
                &mut self.storage,
                &mut self.scratch,
                &self.work,
                statement,
                txn,
                params,
                guc,
                responder,
                None,
            ),
            Stmt::Merge(merge) => exec::merge(
                &mut self.storage,
                txn,
                &mut self.scratch,
                merge,
                &self.work,
                params,
                guc.seq_session(),
                responder,
            ),
            Stmt::With { ctes, statement } => self.execute_with_data_modification(
                ctes, statement, arena, params, txn, guc, responder, None,
            ),
            _ => Ok(Err(sql_err!(
                sqlstate::FEATURE_NOT_SUPPORTED,
                "EXPLAIN does not support this statement type"
            ))),
        }
    }

    /// Expands and executes the data-modifying main statement of a WITH.
    /// EXPLAIN ANALYZE and ordinary execution share this choke point so CTE
    /// materialization, snapshot visibility, and DML dispatch cannot drift.
    #[allow(clippy::too_many_arguments)]
    fn execute_with_data_modification<'a, 'capture>(
        &mut self,
        ctes: &'a [ast::Cte<'a>],
        statement: &'a Stmt<'a>,
        arena: &'a Arena,
        params: &[Datum<'a>],
        txn: &mut TxnState,
        guc: &mut GucState,
        responder: &mut Responder,
        capture: Option<&'capture mut ReturningCapture<'capture>>,
    ) -> Result<Result<(), SqlError>, WireFull> {
        let dml_mats = match self.run_dml_ctes(ctes, txn, arena, params, guc, responder) {
            Ok(materialized) => materialized.unwrap_or(&[]),
            Err(error) => return Ok(Err(error)),
        };
        let statement = match query::expand_dml_ctes(
            statement,
            ctes,
            &self.storage,
            txn.txid,
            &self.work,
            params,
            dml_mats,
        ) {
            Ok(expanded) => expanded,
            Err(error) => return Ok(Err(error)),
        };
        match statement {
            Stmt::Insert(_) | Stmt::Update(_) | Stmt::Delete(_) => Self::execute_data_modification(
                &mut self.storage,
                &mut self.scratch,
                &self.work,
                statement,
                txn,
                params,
                guc,
                responder,
                capture,
            ),
            Stmt::Merge(merge) => exec::merge(
                &mut self.storage,
                txn,
                &mut self.scratch,
                merge,
                &self.work,
                params,
                guc.seq_session(),
                responder,
            ),
            _ => Ok(Err(sql_err!(
                sqlstate::INTERNAL_ERROR,
                "WITH expanded to a non-data-modifying statement"
            ))),
        }
    }

    /// Counts the row-journal records produced by writes since `touched_mark`.
    ///
    /// Row DML is retained as transaction-private heap state and encoded into
    /// WAL at commit, unlike DDL records that enter the private WAL stage
    /// immediately. Measuring the final row images through the production WAL
    /// codec reports those real pending bytes without changing publication or
    /// object-store durability semantics.
    fn explained_row_wal_stats(
        &self,
        txn: &TxnState,
        touched_mark: usize,
    ) -> Result<(u64, u64), SqlError> {
        let touched = txn.touched();
        let mut records = 0u64;
        let mut bytes = 0u64;
        for index in touched_mark..touched.len() {
            let (table, rowid, _) = touched[index];
            if touched[touched_mark..index]
                .iter()
                .any(|&(prior_table, prior_rowid, _)| prior_table == table && prior_rowid == rowid)
            {
                continue;
            }
            let Some(state) = self.storage.row_state(table as usize, rowid)? else {
                continue;
            };
            let Some(pending) = state.pending.last() else {
                continue;
            };
            let table_definition = self.storage.table_def(table as usize, txn.txid);
            let operation = match pending.loc {
                Some(location) => WalOp::Upsert {
                    schema: table_definition.schema.as_str(),
                    table: table_definition.name.as_str(),
                    rowid,
                    row: self.storage.heap.get(location),
                    is_update: false,
                    old_row: None,
                    command_id: pending.cid,
                },
                None => WalOp::Delete {
                    schema: table_definition.schema.as_str(),
                    table: table_definition.name.as_str(),
                    rowid,
                    old_row: None,
                    command_id: pending.cid,
                },
            };
            records = records.saturating_add(1);
            bytes = bytes.saturating_add(encoded_record_len(&operation) as u64);
        }
        Ok((records, bytes))
    }

    #[allow(clippy::too_many_arguments)]
    fn execute_call(
        &mut self,
        name: ast::QualName<'_>,
        arguments: &[&Expr<'_>],
        arena: &Arena,
        params: &[Datum],
        txn: &mut TxnState,
        sqlprep: &mut SqlPreparedPool,
        cursors: &mut cursor::CursorPool,
        guc: &mut GucState,
        responder: &mut Responder,
    ) -> Result<Result<(), SqlError>, WireFull> {
        let mut values = [Datum::Null; crate::storage::MAX_ROUTINE_ARGUMENTS];
        if arguments.len() > values.len() {
            return Ok(Err(sql_err!(
                sqlstate::PROGRAM_LIMIT_EXCEEDED,
                "too many procedure arguments"
            )));
        }
        let catalog = query::storage_catalog(&self.storage, &self.work, txn.txid);
        let hooks = EvalHooks {
            catalog: Some(&catalog),
            ..NO_HOOKS
        };
        for (slot, argument) in arguments.iter().enumerate() {
            values[slot] = match eval::eval_full(argument, arena, params, &NoColumns, &hooks) {
                Ok(value) => value,
                Err(error) => return Ok(Err(error)),
            };
        }
        let mut types = [ColType::Text; crate::storage::MAX_ROUTINE_ARGUMENTS];
        for (slot, value) in values[..arguments.len()].iter().enumerate() {
            let Some(ctype) = exec::coltype_of_oid_pub(value.type_oid()) else {
                return Ok(Err(sql_err!(
                    sqlstate::UNDEFINED_FUNCTION,
                    "procedure \"{}\" does not exist",
                    name.name
                )));
            };
            types[slot] = ctype;
        }
        let qualified = match name.schema {
            Some(schema) => stack_format!(260, "{}.{}", schema, name.name),
            None => stack_format!(260, "{}", name.name),
        };
        let Some(slot) = self.storage.procedure_slot_for_call_types(
            qualified.as_str(),
            &types[..arguments.len()],
            txn.txid,
        ) else {
            return Ok(Err(sql_err!(
                sqlstate::UNDEFINED_FUNCTION,
                "procedure \"{}\" does not exist",
                qualified.as_str()
            )));
        };
        if let Err(error) = self.storage.require_routine_execute(slot, txn.txid) {
            return Ok(Err(error));
        }
        let routine = self.storage.routine(slot);
        let body = routine.body;
        let _formal_scope = exec::enter_routine_parameter_types(routine.arguments());
        let mut parser = match Parser::new(body.as_str(), arena) {
            Ok(parser) => parser,
            Err(error) => return Ok(Err(parse_error_to_sql(&error))),
        };
        let output_mark = responder.buffer.mark();
        let mut statements = 0usize;
        loop {
            let statement = match parser.next_stmt() {
                Ok(Some(statement)) => statement,
                Ok(None) => break,
                Err(error) => {
                    responder.buffer.truncate_to(output_mark);
                    return Ok(Err(parse_error_to_sql(&error)));
                }
            };
            statements += 1;
            // A top-level CALL has no enclosing query workspace; reclaim each
            // suppressed internal result exactly as the ordinary dispatcher
            // did before routine dispatch gained a non-resetting mode.
            self.work.reset();
            match self.execute_routine_stmt(
                &statement,
                arena,
                &values[..arguments.len()],
                txn,
                sqlprep,
                cursors,
                guc,
                responder,
                None,
            ) {
                Ok(Ok(())) => {}
                Ok(Err(error)) => {
                    responder.buffer.truncate_to(output_mark);
                    return Ok(Err(error));
                }
                Err(error) => {
                    responder.buffer.truncate_to(output_mark);
                    return Err(error);
                }
            }
        }
        responder.buffer.truncate_to(output_mark);
        if statements == 0 {
            return Ok(Err(sql_err!(
                sqlstate::SYNTAX_ERROR,
                "procedure body is empty"
            )));
        }
        responder.command_complete("CALL").map(|_| Ok(()))
    }

    /// Outer Result: wire-level trouble. Inner Result: SQL-level error.
    #[allow(clippy::too_many_arguments)]
    fn execute_stmt(
        &mut self,
        statement: &Stmt,
        arena: &Arena,
        params: &[Datum],
        txn: &mut TxnState,
        sqlprep: &mut SqlPreparedPool,
        cursors: &mut cursor::CursorPool,
        guc: &mut GucState,
        responder: &mut Responder,
    ) -> Result<Result<(), SqlError>, WireFull> {
        self.execute_stmt_with_workspace(
            statement, arena, params, txn, sqlprep, cursors, guc, responder, true, None,
        )
    }

    /// Dispatches a statement entered from an SQL routine.  The enclosing
    /// query owns `work`, so a nested statement must not reclaim it beneath
    /// the evaluator that invoked the routine.
    #[allow(clippy::too_many_arguments)]
    fn execute_routine_stmt<'capture>(
        &mut self,
        statement: &Stmt,
        arena: &Arena,
        params: &[Datum],
        txn: &mut TxnState,
        sqlprep: &mut SqlPreparedPool,
        cursors: &mut cursor::CursorPool,
        guc: &mut GucState,
        responder: &mut Responder,
        capture: Option<&'capture mut ReturningCapture<'capture>>,
    ) -> Result<Result<(), SqlError>, WireFull> {
        self.execute_stmt_with_workspace(
            statement, arena, params, txn, sqlprep, cursors, guc, responder, false, capture,
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn execute_stmt_with_workspace<'capture>(
        &mut self,
        statement: &Stmt,
        arena: &Arena,
        params: &[Datum],
        txn: &mut TxnState,
        sqlprep: &mut SqlPreparedPool,
        cursors: &mut cursor::CursorPool,
        guc: &mut GucState,
        responder: &mut Responder,
        reset_workspace: bool,
        capture: Option<&'capture mut ReturningCapture<'capture>>,
    ) -> Result<Result<(), SqlError>, WireFull> {
        if statement_writes(statement) && self.block_reads_pending() {
            return Ok(Err(sql_err!(
                sqlstate::INTERNAL_IO_WAIT,
                "durable block reads in progress"
            )));
        }
        if statement_writes(statement) {
            self.disable_async_block_reads();
        }
        let _guc_eval_scope = guc::enter_eval_scope(guc);
        // Reclaim the shared execution arena from the previous top-level
        // statement. A routine entered from an active query keeps that query's
        // materialized state alive until evaluation returns.
        if reset_workspace {
            self.work.reset();
        }
        // Drop any diagnostic detail a swallowed error left behind, and
        // install this session's effective search path for the statement:
        // every name resolution below reads it from storage.
        let _ = eval::take_diagnostic();
        exec::reset_record_shapes();
        let session_user = guc.session_user();
        eval::funcs::system::set_session_user(session_user.as_str());
        let current_role = guc.current_role();
        eval::funcs::system::set_current_user(current_role.as_str());
        let raw_path = guc.search_path();
        let path = self
            .storage
            .compute_path(raw_path.as_str(), current_role.as_str(), txn.txid);
        self.storage.swap_path(path);
        // Publish the path's schema names for current_schema/current_schemas.
        {
            use core::fmt::Write as _;
            let mut published = eval::funcs::system::SessionSchemas {
                names: [crate::util::StackStr::new(); 17],
                n: 0,
                catalog_pos: usize::MAX,
            };
            for entry in path.entries() {
                match entry {
                    crate::storage::PathEntry::Catalog => {
                        // An *explicit* pg_catalog is a real path element
                        // (current_schema can be pg_catalog); the implicit
                        // one only surfaces in current_schemas(true).
                        if path.explicit_catalog() {
                            let _ = write!(published.names[published.n], "pg_catalog");
                            published.n += 1;
                        } else if published.catalog_pos == usize::MAX {
                            published.catalog_pos = published.n;
                        }
                    }
                    crate::storage::PathEntry::Schema(slot) => {
                        let _ = write!(
                            published.names[published.n],
                            "{}",
                            self.storage.schema_def(*slot as usize).name.as_str()
                        );
                        published.n += 1;
                    }
                }
            }
            eval::funcs::system::set_session_schemas(published);
        }
        // Publish this statement's readable settings for `current_setting()`,
        // the exact values `SHOW` reports (fixed server params + session GUCs).
        {
            let mut names = [""; SETTING_NAMES.len()];
            let mut values = [crate::util::StackStr::<256>::new(); SETTING_NAMES.len()];
            let mut setting_count = 0;
            for &name in SETTING_NAMES {
                if let Some(value) = fixed_setting(name)
                    .map(crate::util::StackStr::from_str)
                    .or_else(|| guc.get_owned(name))
                {
                    names[setting_count] = name;
                    values[setting_count] = value;
                    setting_count += 1;
                }
            }
            if let Err(e) = eval::funcs::system::set_session_settings(
                &names[..setting_count],
                &values[..setting_count],
            ) {
                return Ok(Err(e));
            }
        }
        // Arm this statement's `statement_timeout` deadline (0 clears it); each
        // statement re-arms, so no explicit disarm is needed.
        query::arm_timeout(guc.statement_timeout_ms());
        // Publish the session zone for the same span, so a cast that has to
        // supply one (`'12:00'::timetz`) sees what the client set.
        timezone::set_session(guc.timezone());
        // Render output with the current session settings (a SET earlier in the
        // same batch takes effect here).
        responder.set_render(guc.render());
        // Inside a failed explicit block only COMMIT/ROLLBACK (and ROLLBACK TO
        // SAVEPOINT, which recovers the block) act.
        if txn.failed
            && !matches!(
                statement,
                Stmt::Commit | Stmt::Rollback | Stmt::RollbackToSavepoint(_)
            )
        {
            return Ok(Err(SqlError {
                sqlstate: sqlstate::IN_FAILED_SQL_TRANSACTION,
                message: stack_format!(
                    192,
                    "current transaction is aborted, commands ignored until end of transaction block"
                ),
            }));
        }
        // CHECKPOINT cannot run inside a transaction block (as in
        // PostgreSQL, where it is a utility command). DDL is transactional:
        // CREATE/DROP TABLE roll back with their transaction — with the
        // divergence that uncommitted DDL is visible to other sessions
        // (PostgreSQL would block them on a lock instead).
        if txn.is_explicit() && matches!(statement, Stmt::Checkpoint) {
            return Ok(Err(SqlError {
                sqlstate: sqlstate::FEATURE_NOT_SUPPORTED,
                message: stack_format!(192, "CHECKPOINT cannot run inside a transaction block"),
            }));
        }
        // VACUUM is non-transactional (25001); ANALYZE, by contrast, is allowed
        // inside a transaction block.
        if txn.is_explicit() && matches!(statement, Stmt::Vacuum { .. }) {
            return Ok(Err(SqlError {
                sqlstate: sqlstate::ACTIVE_SQL_TRANSACTION,
                message: stack_format!(192, "VACUUM cannot run inside a transaction block"),
            }));
        }
        if txn.read_only && statement_writes(statement) {
            return Ok(Err(sql_err!(
                sqlstate::READ_ONLY_SQL_TRANSACTION,
                "cannot execute {} in a read-only transaction",
                statement_tag(statement)
            )));
        }
        // Historical row images currently share the committed table
        // definition. Prevent a concurrent definition rewrite from making an
        // old row undecodable by joining the same wait graph as relation and
        // row locks.
        if statement_changes_schema(statement)
            && let Some(blocker) = self.storage.schema_lock_blocker(txn.txid)
        {
            if let Err(error) = self.storage.wait_for_transaction(txn.txid, blocker) {
                return Ok(Err(error));
            }
            return Ok(Err(sql_err!(
                sqlstate::INTERNAL_LOCK_WAIT,
                "statement is waiting for a schema lock"
            )));
        }
        // A new command: advance the command-id (so this statement's writes are
        // tagged with it) and reset reads to full own-write visibility. A
        // data-modifying WITH statement lowers the read snapshot itself; the
        // reset here guarantees it never leaks into the next statement.
        txn.begin_command();
        self.storage.set_read_snapshot(crate::storage::SNAPSHOT_ALL);
        let takes_snapshot = !matches!(
            statement,
            Stmt::Begin(_)
                | Stmt::Commit
                | Stmt::Rollback
                | Stmt::Savepoint(_)
                | Stmt::ReleaseSavepoint(_)
                | Stmt::RollbackToSavepoint(_)
                | Stmt::LockTable { .. }
                | Stmt::SetTransaction(_)
        );
        let commit_snapshot = if takes_snapshot {
            let snapshot = txn.statement_snapshot(self.storage.lsn());
            if matches!(
                txn.isolation,
                IsolationLevel::RepeatableRead | IsolationLevel::Serializable
            ) && let Err(error) = self.storage.register_snapshot(txn.txid, snapshot)
            {
                return Ok(Err(error));
            }
            if txn.isolation == IsolationLevel::Serializable
                && let Err(error) = self.storage.begin_serializable(txn.txid)
            {
                return Ok(Err(error));
            }
            snapshot
        } else {
            self.storage.lsn()
        };
        self.storage.set_commit_snapshot(commit_snapshot);
        match statement {
            Stmt::Explain { options, statement } => {
                let plan = match statement {
                    Stmt::Select(select) => {
                        let planned_select = if select.with.is_empty() {
                            select
                        } else {
                            match query::expand_ctes(select, &self.storage, txn.txid, arena) {
                                Ok(expanded) => expanded,
                                Err(error) => return Ok(Err(error)),
                            }
                        };
                        explain::plan_select(&self.storage, txn.txid, planned_select, arena)
                    }
                    Stmt::SetQuery(set_query) => {
                        let body = match query::expand_set_tree(
                            set_query.with,
                            set_query.body,
                            &self.storage,
                            txn.txid,
                            arena,
                        ) {
                            Ok(body) => body,
                            Err(error) => return Ok(Err(error)),
                        };
                        let planned = ast::SetQuery {
                            with: &[],
                            body,
                            ..*set_query
                        };
                        explain::plan_set_query(&self.storage, txn.txid, &planned, arena)
                    }
                    Stmt::Insert(_) | Stmt::Update(_) | Stmt::Delete(_) | Stmt::Merge(_) => {
                        explain::plan_modification(&self.storage, txn.txid, statement, arena)
                    }
                    Stmt::With { statement, .. } => {
                        explain::plan_modification(&self.storage, txn.txid, statement, arena)
                    }
                    _ => Err(sql_err!(
                        sqlstate::FEATURE_NOT_SUPPORTED,
                        "EXPLAIN does not support this statement type"
                    )),
                };
                let plan = match plan {
                    Ok(plan) => plan,
                    Err(error) => return Ok(Err(error)),
                };
                let actual = if options.analyze {
                    let before = self.storage.block_io_stats();
                    let (before_wal_records, before_wal_bytes) = self.wal.stage_stats(txn.txid);
                    let touched_mark = txn.touched().len();
                    let started = std::time::Instant::now();
                    responder.begin_discard_query_output(options.serialize);
                    let execution = self
                        .execute_explained_statement(statement, arena, params, txn, guc, responder);
                    let output = responder.finish_discard_query_output();
                    match execution {
                        Err(wire) => return Err(wire),
                        Ok(Err(error)) => return Ok(Err(error)),
                        Ok(Ok(())) => {}
                    }
                    let elapsed_micros =
                        started.elapsed().as_micros().min(u128::from(u64::MAX)) as u64;
                    let (after_wal_records, after_wal_bytes) = self.wal.stage_stats(txn.txid);
                    let (row_wal_records, row_wal_bytes) =
                        match self.explained_row_wal_stats(txn, touched_mark) {
                            Ok(stats) => stats,
                            Err(error) => return Ok(Err(error)),
                        };
                    Some(explain::ExplainActual {
                        rows: explained_root_rows(statement, output.rows),
                        elapsed_micros,
                        io: self.storage.block_io_stats().saturating_sub(before),
                        serialized_bytes: output.serialized_bytes,
                        serialization_micros: output.serialization_micros,
                        wal_records: after_wal_records
                            .saturating_sub(before_wal_records)
                            .saturating_add(row_wal_records),
                        wal_bytes: after_wal_bytes
                            .saturating_sub(before_wal_bytes)
                            .saturating_add(row_wal_bytes),
                    })
                } else {
                    None
                };
                explain::emit_plan(&plan, *options, actual, responder)
            }
            Stmt::With { ctes, statement } => self.execute_with_data_modification(
                ctes, statement, arena, params, txn, guc, responder, capture,
            ),
            Stmt::Select(select) => {
                self.execute_select(select, arena, params, txn, sqlprep, cursors, guc, responder)
            }
            Stmt::SetQuery(q) => {
                let sequence = sequence::SeqEval::new(&self.storage, guc.seq_session(), txn.txid);
                query::set_query(
                    &self.storage,
                    txn.txid,
                    q,
                    &self.work,
                    params,
                    Some(&sequence),
                    responder,
                )
            }
            Stmt::CreateTable(c) => {
                exec::create_table(&mut self.storage, &mut self.wal, txn, c, arena, responder)
            }
            Stmt::DropTable(d) => {
                exec::drop_table(&mut self.storage, &mut self.wal, txn, d, responder)
            }
            Stmt::CreateView {
                name,
                or_replace,
                sql,
            } => exec::create_view(
                &mut self.storage,
                &mut self.wal,
                txn,
                exec::CreateViewCommand {
                    name,
                    or_replace: *or_replace,
                    sql,
                    raw_path: guc.search_path().as_str(),
                },
                arena,
                responder,
            ),
            Stmt::CreateRoutine(routine) => exec::create_routine(
                &mut self.storage,
                &mut self.wal,
                txn,
                routine,
                arena,
                responder,
            ),
            Stmt::Call { name, arguments } => self.execute_call(
                *name, arguments, arena, params, txn, sqlprep, cursors, guc, responder,
            ),
            Stmt::AlterRoutine {
                kind,
                routine,
                action,
            } => exec::alter_routine(
                &mut self.storage,
                &mut self.wal,
                txn,
                *kind,
                routine,
                *action,
                responder,
            ),
            Stmt::DropFunction {
                functions,
                if_exists,
                cascade,
            } => exec::drop_routine(
                &mut self.storage,
                &mut self.wal,
                txn,
                exec::DropRoutineCommand {
                    routines: functions,
                    if_exists: *if_exists,
                    cascade: *cascade,
                    kind: crate::sql::ast::RoutineTargetKind::Function,
                },
                responder,
            ),
            Stmt::DropProcedure {
                procedures,
                if_exists,
                cascade,
            } => exec::drop_routine(
                &mut self.storage,
                &mut self.wal,
                txn,
                exec::DropRoutineCommand {
                    routines: procedures,
                    if_exists: *if_exists,
                    cascade: *cascade,
                    kind: crate::sql::ast::RoutineTargetKind::Procedure,
                },
                responder,
            ),
            Stmt::DropRoutine {
                routines,
                if_exists,
                cascade,
            } => exec::drop_routine(
                &mut self.storage,
                &mut self.wal,
                txn,
                exec::DropRoutineCommand {
                    routines,
                    if_exists: *if_exists,
                    cascade: *cascade,
                    kind: crate::sql::ast::RoutineTargetKind::Either,
                },
                responder,
            ),
            Stmt::DropView {
                names,
                if_exists,
                cascade,
            } => exec::drop_view(
                &mut self.storage,
                &mut self.wal,
                txn,
                names,
                *if_exists,
                *cascade,
                responder,
            ),
            Stmt::CreatePublication {
                name,
                all_tables,
                tables,
                schemas,
                publish,
            } => exec::create_publication(
                &mut self.storage,
                &mut self.wal,
                txn,
                name,
                *all_tables,
                tables,
                schemas,
                *publish,
                responder,
            ),
            Stmt::AlterPublication { name, action } => exec::alter_publication(
                &mut self.storage,
                &mut self.wal,
                txn,
                name,
                *action,
                responder,
            ),
            Stmt::DropPublication { names, if_exists } => exec::drop_publication(
                &mut self.storage,
                &mut self.wal,
                txn,
                names,
                *if_exists,
                responder,
            ),
            Stmt::CreateSubscription {
                name,
                connection,
                publications,
                options,
            } => exec::create_subscription(
                &mut self.storage,
                &mut self.wal,
                txn,
                exec::CreateSubscriptionCommand {
                    name,
                    connection,
                    publications,
                    options: *options,
                },
                responder,
            ),
            Stmt::AlterSubscription { name, action } => exec::alter_subscription(
                &mut self.storage,
                &mut self.wal,
                txn,
                name,
                *action,
                responder,
            ),
            Stmt::DropSubscription { names, if_exists } => exec::drop_subscription(
                &mut self.storage,
                &mut self.wal,
                txn,
                names,
                *if_exists,
                responder,
            ),
            Stmt::CreateTrigger(trigger) => {
                exec::create_trigger(&mut self.storage, &mut self.wal, txn, trigger, responder)
            }
            Stmt::DropTrigger {
                triggers,
                if_exists,
            } => exec::drop_trigger(
                &mut self.storage,
                &mut self.wal,
                txn,
                triggers,
                *if_exists,
                responder,
            ),
            Stmt::AlterTrigger { trigger, action } => exec::alter_trigger(
                &mut self.storage,
                &mut self.wal,
                txn,
                trigger,
                *action,
                responder,
            ),
            Stmt::CreateTableAs {
                name,
                columns,
                sql,
                with_data,
                if_not_exists,
                materialized,
            } => exec::create_table_as(
                &mut self.storage,
                &mut self.wal,
                txn,
                name,
                columns,
                sql,
                *with_data,
                *if_not_exists,
                *materialized,
                guc.search_path().as_str(),
                arena,
                params,
                responder,
            ),
            Stmt::RefreshMaterializedView { name } => exec::refresh_materialized_view(
                &mut self.storage,
                &mut self.wal,
                txn,
                name,
                arena,
                params,
                responder,
            ),
            Stmt::DropMaterializedView {
                names,
                if_exists,
                cascade,
            } => exec::drop_materialized_view(
                &mut self.storage,
                &mut self.wal,
                txn,
                names,
                *if_exists,
                *cascade,
                responder,
            ),
            Stmt::CreateSequence {
                name,
                if_not_exists,
                options,
            } => exec::create_sequence(
                &mut self.storage,
                &mut self.wal,
                txn,
                name,
                *if_not_exists,
                options,
                responder,
            ),
            Stmt::AlterSequence {
                name,
                if_exists,
                options,
            } => exec::alter_sequence(
                &mut self.storage,
                &mut self.wal,
                txn,
                name,
                *if_exists,
                options,
                responder,
            ),
            Stmt::DropSequence {
                names,
                if_exists,
                cascade,
            } => exec::drop_sequence(
                &mut self.storage,
                &mut self.wal,
                txn,
                names,
                *if_exists,
                *cascade,
                responder,
            ),
            Stmt::CreateDomain(d) => {
                exec::create_domain(&mut self.storage, &mut self.wal, txn, d, arena, responder)
            }
            Stmt::AlterDomain { name, action } => exec::alter_domain(
                &mut self.storage,
                &mut self.wal,
                txn,
                name,
                action,
                arena,
                responder,
            ),
            Stmt::DropDomain {
                names,
                if_exists,
                cascade,
            } => exec::drop_domain(
                &mut self.storage,
                &mut self.wal,
                txn,
                &mut self.scratch,
                names,
                *if_exists,
                *cascade,
                &self.work,
                guc.seq_session(),
                responder,
            ),
            Stmt::CreateEnum { name, labels } => exec::create_enum(
                &mut self.storage,
                &mut self.wal,
                txn,
                name,
                labels,
                responder,
            ),
            Stmt::AlterType { name, action } => exec::alter_type(
                &mut self.storage,
                &mut self.wal,
                txn,
                name,
                action,
                arena,
                responder,
            ),
            Stmt::DropType {
                names,
                if_exists,
                cascade,
            } => exec::drop_enum(
                &mut self.storage,
                &mut self.wal,
                txn,
                &mut self.scratch,
                names,
                *if_exists,
                *cascade,
                &self.work,
                guc.seq_session(),
                responder,
            ),
            Stmt::CreateIndex {
                name,
                table,
                columns,
                include_columns,
                nulls_not_distinct,
                predicate,
                predicate_text,
                unique,
            } => exec::create_index(
                &mut self.storage,
                &mut self.wal,
                txn,
                name,
                table,
                columns,
                include_columns,
                *nulls_not_distinct,
                *predicate,
                *predicate_text,
                arena,
                *unique,
                responder,
            ),
            Stmt::AlterIndex {
                name,
                if_exists,
                action,
            } => exec::alter_index(
                &mut self.storage,
                &mut self.wal,
                txn,
                name,
                *if_exists,
                *action,
                responder,
            ),
            Stmt::DropIndex { names, if_exists } => exec::drop_index(
                &mut self.storage,
                &mut self.wal,
                txn,
                names,
                *if_exists,
                responder,
            ),
            Stmt::Reindex {
                target,
                name,
                concurrently,
            } => exec::reindex(
                &mut self.storage,
                txn,
                *target,
                name,
                *concurrently,
                responder,
            ),
            Stmt::Insert(_) | Stmt::Update(_) | Stmt::Delete(_) => Self::execute_data_modification(
                &mut self.storage,
                &mut self.scratch,
                &self.work,
                statement,
                txn,
                params,
                guc,
                responder,
                capture,
            ),
            Stmt::Merge(m) => exec::merge(
                &mut self.storage,
                txn,
                &mut self.scratch,
                m,
                arena,
                params,
                guc.seq_session(),
                responder,
            ),
            Stmt::Comment { target, text } => exec::comment(
                &mut self.storage,
                &mut self.wal,
                txn,
                target,
                *text,
                arena,
                responder,
            ),
            Stmt::Truncate {
                tables,
                restart_identity,
                cascade,
            } => exec::truncate(
                &mut self.storage,
                txn,
                &mut self.scratch,
                arena,
                guc.seq_session(),
                tables,
                *restart_identity,
                *cascade,
                responder,
            ),
            Stmt::CreateSchema {
                name,
                authorization,
                if_not_exists,
                elements,
            } => {
                let out = exec::create_schema(
                    &mut self.storage,
                    &mut self.wal,
                    txn,
                    name,
                    *authorization,
                    *if_not_exists,
                    responder,
                )?;
                if let Err(e) = out {
                    return Ok(Err(e));
                }
                // Schema elements run with the new schema as their creation
                // target; an element naming a different schema is refused, as
                // PostgreSQL has it (42P15).
                for element in *elements {
                    let requalified = match requalify_schema_element(element, name, arena) {
                        Ok(r) => r,
                        Err(e) => return Ok(Err(e)),
                    };
                    let result = if let Stmt::CreateView {
                        name,
                        or_replace,
                        sql,
                    } = requalified
                    {
                        let schema = name
                            .schema
                            .expect("CREATE SCHEMA requalification assigns a schema");
                        let schema_path = match eval::quote_ident_str(schema, arena) {
                            Ok(path) => path,
                            Err(e) => return Ok(Err(e)),
                        };
                        let role = guc.current_role();
                        let path = self
                            .storage
                            .compute_path(schema_path, role.as_str(), txn.txid);
                        let old_path = self.storage.swap_path(path);
                        let result = exec::create_view(
                            &mut self.storage,
                            &mut self.wal,
                            txn,
                            exec::CreateViewCommand {
                                name,
                                or_replace: *or_replace,
                                sql,
                                raw_path: schema_path,
                            },
                            arena,
                            responder,
                        );
                        self.storage.swap_path(old_path);
                        result
                    } else {
                        self.execute_stmt(
                            requalified,
                            arena,
                            params,
                            txn,
                            sqlprep,
                            cursors,
                            guc,
                            responder,
                        )
                    };
                    let result = result?;
                    if let Err(e) = result {
                        return Ok(Err(e));
                    }
                }
                Ok(Ok(()))
            }
            Stmt::DropSchema {
                names,
                if_exists,
                cascade,
            } => exec::drop_schema(
                &mut self.storage,
                &mut self.wal,
                txn,
                &mut self.scratch,
                names,
                *if_exists,
                *cascade,
                arena,
                guc.seq_session(),
                responder,
            ),
            Stmt::AlterOwner {
                kind,
                name,
                role,
                if_exists,
            } => exec::alter_owner(
                &mut self.storage,
                txn,
                *kind,
                name,
                role,
                *if_exists,
                responder,
            ),
            Stmt::CreateRole {
                name,
                options,
                memberships,
            } => exec::create_role(
                &mut self.storage,
                &mut self.wal,
                txn,
                name,
                options,
                memberships,
                responder,
            ),
            Stmt::AlterRole { name, options } => exec::alter_role(
                &mut self.storage,
                &mut self.wal,
                txn,
                name,
                options,
                responder,
            ),
            Stmt::AlterRoleRename { name, new_name } => exec::rename_role(
                &mut self.storage,
                &mut self.wal,
                txn,
                name,
                new_name,
                responder,
            ),
            Stmt::DropRole { names, if_exists } => exec::drop_role(
                &mut self.storage,
                &mut self.wal,
                txn,
                names,
                *if_exists,
                responder,
            ),
            Stmt::SetRole { role, local, reset } => {
                exec::set_role(&self.storage, txn, guc, *role, *local, *reset, responder)
            }
            Stmt::SetSessionAuthorization { role, local, reset } => {
                exec::set_session_authorization(
                    &self.storage,
                    txn,
                    guc,
                    *role,
                    *local,
                    *reset,
                    responder,
                )
            }
            Stmt::GrantRole {
                roles,
                members,
                options,
            } => exec::grant_role(
                &mut self.storage,
                &mut self.wal,
                txn,
                roles,
                members,
                *options,
                responder,
            ),
            Stmt::RevokeRole {
                roles,
                members,
                admin_option_only,
            } => exec::revoke_role(
                &mut self.storage,
                &mut self.wal,
                txn,
                roles,
                members,
                *admin_option_only,
                responder,
            ),
            Stmt::GrantPrivileges {
                privileges,
                target,
                grantees,
                grant_option,
            } => exec::grant_privileges(
                &mut self.storage,
                txn,
                privileges,
                *target,
                grantees,
                *grant_option,
                responder,
            ),
            Stmt::RevokePrivileges {
                grant_option_only,
                privileges,
                target,
                grantees,
                cascade,
            } => exec::revoke_privileges(
                &mut self.storage,
                txn,
                *grant_option_only,
                privileges,
                *target,
                grantees,
                *cascade,
                responder,
            ),
            Stmt::AlterDefaultPrivileges {
                roles,
                schemas,
                action,
            } => exec::alter_default_privileges(
                &mut self.storage,
                txn,
                roles,
                schemas,
                *action,
                responder,
            ),
            Stmt::ReassignOwned { roles, new_owner } => {
                exec::reassign_owned(&mut self.storage, txn, roles, new_owner, responder)
            }
            Stmt::DropOwned { roles, cascade } => exec::drop_owned(
                &mut self.storage,
                &mut self.wal,
                txn,
                &mut self.scratch,
                roles,
                *cascade,
                arena,
                guc.seq_session(),
                responder,
            ),
            Stmt::DeclareCursor {
                name,
                binary,
                scroll,
                hold,
                sql,
            } => {
                if !txn.is_explicit() {
                    return Ok(Err(sql_err!(
                        crate::sql::eval::sqlstate::NO_ACTIVE_SQL_TRANSACTION,
                        "DECLARE CURSOR can only be used in transaction blocks"
                    )));
                }
                let at = match cursors.open(name, *scroll, *hold) {
                    Ok(at) => at,
                    Err(e) => return Ok(Err(e)),
                };
                // Materialize the whole result now — PostgreSQL's insensitive
                // cursor snapshot — by running the SELECT with a responder
                // aimed at the cursor's own buffer.
                let out = {
                    let mut inner = match Parser::new(sql, arena) {
                        Ok(p) => p,
                        Err(e) => {
                            cursors.abandon(at);
                            return Ok(Err(SqlError {
                                sqlstate: e.sqlstate,
                                message: stack_format!(192, "{}", e.message.as_str()),
                            }));
                        }
                    };
                    let parsed = match inner.next_stmt() {
                        Ok(Some(p)) => p,
                        _ => {
                            cursors.abandon(at);
                            return Ok(Err(sql_err!(
                                sqlstate::SYNTAX_ERROR,
                                "DECLARE CURSOR requires a SELECT"
                            )));
                        }
                    };
                    let mut capture = if *binary {
                        Responder::for_binary_cursor(cursors.result_buffer(at))
                    } else {
                        Responder::new(cursors.result_buffer(at))
                    };
                    capture.set_render(guc.render());
                    let sequence =
                        sequence::SeqEval::new(&self.storage, guc.seq_session(), txn.txid);
                    match &parsed {
                        Stmt::Select(sel) => {
                            let sel = match query::expand_ctes_exec(
                                sel,
                                &self.storage,
                                txn.txid,
                                &self.work,
                                params,
                                &[],
                            ) {
                                Ok(x) => x,
                                Err(e) => {
                                    cursors.abandon(at);
                                    return Ok(Err(e));
                                }
                            };
                            if let Err(e) = query::validate_locking(sel) {
                                cursors.abandon(at);
                                return Ok(Err(e));
                            }
                            if sel.from.is_none() {
                                query::constant_select(
                                    &self.storage,
                                    txn.txid,
                                    sel,
                                    &self.work,
                                    params,
                                    Some(&sequence),
                                    &mut capture,
                                )
                            } else {
                                query::select_query(
                                    &self.storage,
                                    txn.txid,
                                    sel,
                                    &self.work,
                                    params,
                                    Some(&sequence),
                                    &mut capture,
                                )
                            }
                        }
                        Stmt::SetQuery(q) => query::set_query(
                            &self.storage,
                            txn.txid,
                            q,
                            &self.work,
                            params,
                            Some(&sequence),
                            &mut capture,
                        ),
                        _ => {
                            cursors.abandon(at);
                            return Ok(Err(sql_err!(
                                sqlstate::SYNTAX_ERROR,
                                "DECLARE CURSOR requires a SELECT"
                            )));
                        }
                    }
                };
                match out {
                    Ok(Ok(())) => {}
                    Ok(Err(e)) => {
                        cursors.abandon(at);
                        return Ok(Err(e));
                    }
                    Err(WireFull) => {
                        cursors.abandon(at);
                        return Ok(Err(sql_err!(
                            sqlstate::PROGRAM_LIMIT_EXCEEDED,
                            "cursor result exceeds cursor_bytes; raise it or narrow the query"
                        )));
                    }
                }
                if let Err(e) = cursors.seal(at) {
                    cursors.abandon(at);
                    return Ok(Err(e));
                }
                responder.command_complete("DECLARE CURSOR")?;
                Ok(Ok(()))
            }
            Stmt::FetchCursor {
                name,
                motion,
                move_only,
            } => {
                let count = match cursors.fetch(name, *motion) {
                    Ok(c) => c,
                    Err(e) => return Ok(Err(e)),
                };
                if !*move_only {
                    let (description, rows) = cursors.wire_parts(name).expect("fetch found it");
                    responder.raw(description)?;
                    for &(offset, len) in cursors.emitted() {
                        let (offset, len) = (offset as usize, len as usize);
                        responder.raw(&rows[offset..offset + len])?;
                    }
                    responder.command_complete(stack_format!(32, "FETCH {}", count).as_str())?;
                } else {
                    responder.command_complete(stack_format!(32, "MOVE {}", count).as_str())?;
                }
                Ok(Ok(()))
            }
            Stmt::CloseCursor(name) => {
                match name {
                    Some(n) => {
                        if !cursors.close(n) {
                            return Ok(Err(sql_err!(
                                crate::sql::eval::sqlstate::UNDEFINED_CURSOR,
                                "cursor \"{}\" does not exist",
                                n
                            )));
                        }
                    }
                    None => cursors.close_all(),
                }
                responder.command_complete("CLOSE CURSOR")?;
                Ok(Ok(()))
            }
            Stmt::Begin(characteristics) => {
                let characteristics = match transaction_characteristics(characteristics) {
                    Ok(characteristics) => characteristics,
                    Err(characteristic) => {
                        return Ok(Err(sql_err!(
                            sqlstate::FEATURE_NOT_SUPPORTED,
                            "transaction characteristic \"{}\" is not supported",
                            characteristic
                        )));
                    }
                };
                if txn.is_explicit() {
                    // PostgreSQL warns and continues.
                    responder.warning(
                        crate::sql::eval::sqlstate::ACTIVE_SQL_TRANSACTION,
                        "there is already a transaction in progress",
                    )?;
                }
                self.ensure_txn(txn, TxnMode::Explicit, guc);
                txn.set_characteristics(
                    characteristics.isolation.unwrap_or(txn.isolation),
                    characteristics.read_only.unwrap_or(txn.read_only),
                    characteristics.deferrable.unwrap_or(txn.deferrable),
                );
                responder.command_complete("BEGIN")?;
                Ok(Ok(()))
            }
            Stmt::Commit => {
                if !txn.is_explicit() {
                    responder.warning("25P01", "there is no transaction in progress")?;
                }
                let tag = if txn.failed { "ROLLBACK" } else { "COMMIT" };
                if txn.failed {
                    self.rollback_txn(txn, guc);
                    cursors.on_rollback();
                } else {
                    if let Err(e) = self.commit_txn(txn, guc) {
                        return Ok(Err(e));
                    }
                    cursors.on_commit();
                }
                responder.command_complete(tag)?;
                // Later statements in this message get a fresh implicit txn.
                // Freeze this statement's clock before anything anchors a
                // transaction to it.
                datetime::begin_statement();
                self.ensure_txn(txn, TxnMode::Implicit, guc);
                Ok(Ok(()))
            }
            Stmt::Rollback => {
                if !txn.is_explicit() {
                    responder.warning("25P01", "there is no transaction in progress")?;
                }
                self.rollback_txn(txn, guc);
                cursors.on_rollback();
                responder.command_complete("ROLLBACK")?;
                // Freeze this statement's clock before anything anchors a
                // transaction to it.
                datetime::begin_statement();
                self.ensure_txn(txn, TxnMode::Implicit, guc);
                Ok(Ok(()))
            }
            Stmt::LockTable {
                tables,
                mode,
                nowait,
            } => {
                if !txn.is_explicit() {
                    return Ok(Err(sql_err!(
                        sqlstate::NO_ACTIVE_SQL_TRANSACTION,
                        "LOCK TABLE can only be used in transaction blocks"
                    )));
                }
                let mut slots = [usize::MAX; 32];
                for (index, table) in tables.iter().enumerate() {
                    slots[index] = match exec::resolve_dml_table(&self.storage, table, txn.txid) {
                        Ok(slot) => slot,
                        Err(error) => return Ok(Err(error)),
                    };
                }
                for &slot in &slots[..tables.len()] {
                    if let Err(error) = self.storage.lock_table(txn.txid, slot, *mode, *nowait) {
                        return Ok(Err(error));
                    }
                }
                responder.command_complete("LOCK TABLE")?;
                Ok(Ok(()))
            }
            Stmt::Savepoint(name) => {
                if !txn.is_explicit() {
                    return Ok(Err(sql_err!(
                        crate::sql::eval::sqlstate::NO_ACTIVE_SQL_TRANSACTION,
                        "SAVEPOINT can only be used in transaction blocks"
                    )));
                }
                let mark = self.wal.stage_mark(txn.txid);
                if let Err(e) = txn.savepoint(name, mark, self.storage.lock_mark()) {
                    return Ok(Err(e));
                }
                guc.savepoint();
                responder.command_complete("SAVEPOINT")?;
                Ok(Ok(()))
            }
            Stmt::ReleaseSavepoint(name) => {
                if !txn.is_explicit() {
                    return Ok(Err(sql_err!(
                        crate::sql::eval::sqlstate::NO_ACTIVE_SQL_TRANSACTION,
                        "RELEASE SAVEPOINT can only be used in transaction blocks"
                    )));
                }
                match txn.savepoint_index(name) {
                    Some(index) => {
                        txn.release_savepoints_from(index);
                        guc.release_savepoints_from(index);
                        responder.command_complete("RELEASE")?;
                        Ok(Ok(()))
                    }
                    None => Ok(Err(sql_err!(
                        crate::sql::eval::sqlstate::INVALID_SAVEPOINT_SPECIFICATION,
                        "savepoint \"{}\" does not exist",
                        name
                    ))),
                }
            }
            Stmt::RollbackToSavepoint(name) => {
                if !txn.is_explicit() {
                    return Ok(Err(sql_err!(
                        crate::sql::eval::sqlstate::NO_ACTIVE_SQL_TRANSACTION,
                        "ROLLBACK TO SAVEPOINT can only be used in transaction blocks"
                    )));
                }
                let Some(index) = txn.savepoint_index(name) else {
                    return Ok(Err(sql_err!(
                        crate::sql::eval::sqlstate::INVALID_SAVEPOINT_SPECIFICATION,
                        "savepoint \"{}\" does not exist",
                        name
                    )));
                };
                self.rollback_to_savepoint(txn, index, guc);
                responder.command_complete("ROLLBACK")?;
                Ok(Ok(()))
            }
            Stmt::Set { name, value, local } => {
                if *local && !txn.is_explicit() {
                    responder.warning(
                        sqlstate::NO_ACTIVE_SQL_TRANSACTION,
                        "SET LOCAL can only be used in transaction blocks",
                    )?;
                }
                match guc.set(name, value, *local) {
                    Ok(()) => {
                        responder.command_complete("SET")?;
                        Ok(Ok(()))
                    }
                    Err(e) => Ok(Err(e)),
                }
            }
            Stmt::Reset(name) => {
                let result = match name {
                    Some(name) => guc.reset(name),
                    None => {
                        guc.reset_all();
                        Ok(())
                    }
                };
                match result {
                    Ok(()) => {
                        responder.command_complete("RESET")?;
                        Ok(Ok(()))
                    }
                    Err(e) => Ok(Err(e)),
                }
            }
            Stmt::SetTransaction(characteristics) => {
                let characteristics = match transaction_characteristics(characteristics) {
                    Ok(characteristics) => characteristics,
                    Err(characteristic) => {
                        return Ok(Err(sql_err!(
                            sqlstate::FEATURE_NOT_SUPPORTED,
                            "transaction characteristic \"{}\" is not supported",
                            characteristic
                        )));
                    }
                };
                if !txn.is_explicit() {
                    responder.warning(
                        sqlstate::NO_ACTIVE_SQL_TRANSACTION,
                        "SET TRANSACTION can only be used in transaction blocks",
                    )?;
                    responder.command_complete("SET")?;
                    return Ok(Ok(()));
                }
                if characteristics.isolation.is_some() && txn.snapshot_taken() {
                    return Ok(Err(sql_err!(
                        sqlstate::ACTIVE_SQL_TRANSACTION,
                        "SET TRANSACTION ISOLATION LEVEL must be called before any query"
                    )));
                }
                txn.set_characteristics(
                    characteristics.isolation.unwrap_or(txn.isolation),
                    characteristics.read_only.unwrap_or(txn.read_only),
                    characteristics.deferrable.unwrap_or(txn.deferrable),
                );
                responder.command_complete("SET")?;
                Ok(Ok(()))
            }
            Stmt::Show(name) => self.show(name, guc, responder),
            Stmt::ShowAll => self.show_all(guc, responder),
            Stmt::Copy(c) => {
                // COPY (query) TO STDOUT streams a query's rows, not a table's.
                if let Some(sql) = c.query {
                    let seq = sequence::SeqEval::new(&self.storage, guc.seq_session(), txn.txid);
                    return Ok(
                        match exec::copy_out_query(
                            &self.storage,
                            txn.txid,
                            sql,
                            &c.options,
                            Some(&seq),
                            arena,
                            params,
                            responder,
                        ) {
                            Ok(count) => {
                                responder.command_complete(
                                    crate::stack_format!(32, "COPY {count}").as_str(),
                                )?;
                                Ok(())
                            }
                            Err(e) => Err(e),
                        },
                    );
                }
                let setup = match exec::copy_begin(&self.storage, c, txn.txid) {
                    Ok(s) => s,
                    Err(e) => return Ok(Err(e)),
                };
                let mode = if c.to {
                    ast::TableLockMode::AccessShare
                } else {
                    ast::TableLockMode::RowExclusive
                };
                if let Err(error) =
                    self.storage
                        .lock_table(txn.txid, setup.table_index, mode, false)
                {
                    return Ok(Err(error));
                }
                if c.to {
                    match exec::copy_out(&self.storage, txn.txid, &setup, arena, responder) {
                        Ok(count) => {
                            responder.command_complete(
                                crate::stack_format!(32, "COPY {count}").as_str(),
                            )?;
                            Ok(Ok(()))
                        }
                        Err(e) => Ok(Err(e)),
                    }
                } else {
                    // COPY FROM STDIN: the statement's work has only begun —
                    // the connection takes over, streaming CopyData into
                    // copy_row_line under this same (implicit or explicit)
                    // transaction, and the command tag waits for CopyDone.
                    self.ensure_txn(txn, txn.mode, guc);
                    responder.copy_in_response(setup.n_targets, setup.fmt.binary)?;
                    self.pending_copy = Some(setup);
                    Ok(Ok(()))
                }
            }
            Stmt::Checkpoint => match self.checkpoint() {
                Ok(_) => {
                    responder.command_complete("CHECKPOINT")?;
                    Ok(Ok(()))
                }
                Err(e) => Ok(Err(e)),
            },
            // VACUUM reclaims space; in this LSM that is a checkpoint (flush +
            // compaction, pruning superseded versions and tombstones). The
            // options and per-table targets are parsed; a checkpoint compacts
            // the whole store, which subsumes any named table. Without object
            // storage there is nothing to compact to, and — as VACUUM on a
            // table with nothing to reclaim does in PostgreSQL — it succeeds.
            Stmt::Vacuum { targets, analyze } => {
                if let Err(error) = self.lock_maintenance_targets(targets, txn.txid) {
                    return Ok(Err(error));
                }
                let validation = if *analyze {
                    self.analyze_targets(targets, txn).map(|_| ())
                } else {
                    self.validate_maintenance_targets(targets, txn.txid)
                };
                if let Err(error) = validation {
                    return Ok(Err(error));
                }
                if self.ckpt.is_some()
                    && let Err(e) = self.checkpoint()
                {
                    return Ok(Err(e));
                }
                responder.command_complete("VACUUM")?;
                Ok(Ok(()))
            }
            // ANALYZE resolves every requested relation/column and walks its
            // MVCC-visible row state. Cardinality and widths are exact for that
            // snapshot; distinct counts use the fixed-size estimator.
            Stmt::Analyze(targets) => {
                if let Err(error) = self.lock_maintenance_targets(targets, txn.txid) {
                    return Ok(Err(error));
                }
                if let Err(error) = self.analyze_targets(targets, txn) {
                    return Ok(Err(error));
                }
                responder.command_complete("ANALYZE")?;
                Ok(Ok(()))
            }
            Stmt::Listen(channel) => {
                let op = notify::ListenOp::Listen {
                    conn_id: self.current_conn_id,
                    channel: notify::channel(channel),
                };
                if let Err(e) = txn.buffer_listen_op(op) {
                    return Ok(Err(e));
                }
                responder.command_complete("LISTEN")?;
                Ok(Ok(()))
            }
            Stmt::Unlisten(channel) => {
                let op = match channel {
                    Some(name) => notify::ListenOp::Unlisten {
                        conn_id: self.current_conn_id,
                        channel: notify::channel(name),
                    },
                    None => notify::ListenOp::UnlistenAll {
                        conn_id: self.current_conn_id,
                    },
                };
                if let Err(e) = txn.buffer_listen_op(op) {
                    return Ok(Err(e));
                }
                responder.command_complete("UNLISTEN")?;
                Ok(Ok(()))
            }
            Stmt::Notify { channel, payload } => {
                // Validate the payload length (PostgreSQL's 8000-byte limit)
                // before buffering the raw text.
                let payload = match payload {
                    Some(text) => match notify::payload(text) {
                        Ok(p) => p,
                        Err(e) => return Ok(Err(e)),
                    },
                    None => notify::Payload::new(),
                };
                if let Err(e) = txn.buffer_notify(
                    self.current_conn_id,
                    notify::channel(channel),
                    payload.as_str(),
                ) {
                    return Ok(Err(e));
                }
                responder.command_complete("NOTIFY")?;
                Ok(Ok(()))
            }
            Stmt::AlterTable(a) => exec::alter_table(
                &mut self.storage,
                &mut self.wal,
                txn,
                &mut self.scratch,
                a,
                arena,
                guc.seq_session(),
                responder,
            ),
            Stmt::Prepare {
                name,
                sql,
                param_types,
            } => {
                // Resolve declared parameter types up front; an unknown type is
                // an error, never quietly ignored.
                let mut types = [ColType::Bool; parser::MAX_LIST];
                for (i, tn) in param_types.iter().enumerate() {
                    match ColType::from_sql_name(tn) {
                        Some(ct) => types[i] = ct,
                        None => {
                            return Ok(Err(SqlError {
                                sqlstate: sqlstate::UNDEFINED_OBJECT,
                                message: stack_format!(192, "type \"{}\" does not exist", tn),
                            }));
                        }
                    }
                }
                match sqlprep.store(name, sql, &types[..param_types.len()]) {
                    Ok(()) => {
                        responder.command_complete("PREPARE")?;
                        Ok(Ok(()))
                    }
                    Err(e) => Ok(Err(e)),
                }
            }
            Stmt::ExecutePrepared { name, args } => {
                let Some(text) = sqlprep.get(name) else {
                    return Ok(Err(SqlError {
                        sqlstate: sqlstate::INVALID_SQL_STATEMENT_NAME,
                        message: stack_format!(
                            192,
                            "prepared statement \"{}\" does not exist",
                            name
                        ),
                    }));
                };
                // Snapshot the declared parameter types before releasing the
                // pool borrow.
                let mut decl = [ColType::Bool; parser::MAX_LIST];
                let n_decl = sqlprep
                    .get_types(name)
                    .map(|ts| {
                        decl[..ts.len()].copy_from_slice(ts);
                        ts.len()
                    })
                    .unwrap_or(0);
                // Copy to the arena so the pool borrow ends before the
                // recursive dispatch below.
                let text = match arena.alloc_str(text) {
                    Ok(t) => t,
                    Err(_) => {
                        return Ok(Err(SqlError {
                            sqlstate: sqlstate::PROGRAM_LIMIT_EXCEEDED,
                            message: stack_format!(192, "statement too large for SQL arena"),
                        }));
                    }
                };
                // If the statement declared parameter types, the argument count
                // must match and each argument is coerced to its declared type.
                if n_decl > 0 && args.len() != n_decl {
                    return Ok(Err(SqlError {
                        sqlstate: sqlstate::PROTOCOL_VIOLATION,
                        message: stack_format!(
                            192,
                            "wrong number of parameters for prepared statement \"{}\": expected {}, got {}",
                            name,
                            n_decl,
                            args.len()
                        ),
                    }));
                }
                // Argument expressions become the inner statement's $n
                // parameters, coerced to the declared types when present.
                let mut inner_params = [Datum::Null; parser::MAX_LIST];
                for (i, a) in args.iter().enumerate() {
                    let v = match eval(a, arena, params, &NoColumns) {
                        Ok(v) => v,
                        Err(e) => return Ok(Err(e)),
                    };
                    inner_params[i] = if i < n_decl {
                        match eval::cast(v, decl[i].internal_name(), arena) {
                            Ok(v) => v,
                            Err(e) => return Ok(Err(e)),
                        }
                    } else {
                        v
                    };
                }
                let mut inner = match Parser::new(text, arena) {
                    Ok(p) => p,
                    Err(e) => {
                        return Ok(Err(SqlError {
                            sqlstate: sqlstate::SYNTAX_ERROR,
                            message: stack_format!(192, "{}", e.message.as_str()),
                        }));
                    }
                };
                match inner.next_stmt() {
                    Ok(Some(statement)) => self.execute_stmt(
                        &statement,
                        arena,
                        &inner_params[..args.len()],
                        txn,
                        sqlprep,
                        cursors,
                        guc,
                        responder,
                    ),
                    Ok(None) => Ok(Ok(())),
                    Err(e) => Ok(Err(SqlError {
                        sqlstate: sqlstate::SYNTAX_ERROR,
                        message: stack_format!(192, "{}", e.message.as_str()),
                    })),
                }
            }
            Stmt::Deallocate(name) => {
                match name {
                    Some(n) => {
                        if !sqlprep.remove(n) {
                            return Ok(Err(SqlError {
                                sqlstate: sqlstate::INVALID_SQL_STATEMENT_NAME,
                                message: stack_format!(
                                    192,
                                    "prepared statement \"{}\" does not exist",
                                    n
                                ),
                            }));
                        }
                    }
                    None => sqlprep.clear(),
                }
                responder.command_complete("DEALLOCATE")?;
                Ok(Ok(()))
            }
        }
    }

    fn show(
        &mut self,
        name: &str,
        guc: &GucState,
        responder: &mut Responder,
    ) -> Result<Result<(), SqlError>, WireFull> {
        // Session GUCs come from the per-session store; the rest are fixed
        // server parameters.
        let owned = guc.get_owned(name);
        let value = if let Some(value) = fixed_setting(name) {
            value
        } else if let Some(value) = owned.as_ref() {
            value.as_str()
        } else {
            return Ok(Err(SqlError {
                sqlstate: sqlstate::UNDEFINED_OBJECT,
                message: stack_format!(192, "unrecognized configuration parameter \"{}\"", name),
            }));
        };
        // The column titles as PostgreSQL canonicalizes them: most parameters
        // are lowercase, but a few keep their registered mixed case.
        let title = if name.eq_ignore_ascii_case("timezone") {
            "TimeZone"
        } else if name.eq_ignore_ascii_case("datestyle") {
            "DateStyle"
        } else if name.eq_ignore_ascii_case("intervalstyle") {
            "IntervalStyle"
        } else {
            name
        };
        responder.row_description(&[ColDesc::new(title, types::oid::TEXT, -1)])?;
        responder.data_row(&[Datum::Text(value)])?;
        responder.command_complete("SHOW")?;
        Ok(Ok(()))
    }

    /// SHOW ALL: every readable setting as (name, setting, description). Tools
    /// read name/setting; descriptions are left empty.
    fn show_all(
        &mut self,
        guc: &GucState,
        responder: &mut Responder,
    ) -> Result<Result<(), SqlError>, WireFull> {
        responder.row_description(&[
            ColDesc::new("name", types::oid::TEXT, -1),
            ColDesc::new("setting", types::oid::TEXT, -1),
            ColDesc::new("description", types::oid::TEXT, -1),
        ])?;
        for &name in SETTING_NAMES {
            let owned = guc.get_owned(name);
            if let Some(value) =
                fixed_setting(name).or_else(|| owned.as_ref().map(|value| value.as_str()))
            {
                responder.data_row(&[Datum::Text(name), Datum::Text(value), Datum::Text("")])?;
            }
        }
        responder.command_complete("SHOW")?;
        Ok(Ok(()))
    }
}

/// Fixed server parameters not backed by the per-session GUC store.
fn fixed_setting(name: &str) -> Option<&'static str> {
    match name {
        "server_version" => Some(crate::pg::REPORTED_SERVER_VERSION),
        "server_version_num" => Some(crate::pg::REPORTED_SERVER_VERSION_NUM),
        "server_encoding" => Some("UTF8"),
        "standard_conforming_strings" => Some("on"),
        "integer_datetimes" => Some("on"),
        "transaction_isolation" => Some("read committed"),
        "is_superuser" => Some("on"),
        _ => None,
    }
}

/// Every setting readable through `SHOW`, `SHOW ALL`, and `current_setting` —
/// the fixed server parameters plus the per-session GUCs. Names carry
/// PostgreSQL's canonical case for the mixed-case ones.
pub(crate) const SETTING_NAMES: &[&str] = &[
    "application_name",
    "bytea_output",
    "check_function_bodies",
    "client_encoding",
    "client_min_messages",
    "DateStyle",
    "default_table_access_method",
    "default_tablespace",
    "extra_float_digits",
    "idle_in_transaction_session_timeout",
    "integer_datetimes",
    "IntervalStyle",
    "is_superuser",
    "lock_timeout",
    "row_security",
    "search_path",
    "server_encoding",
    "server_version",
    "server_version_num",
    "standard_conforming_strings",
    "statement_timeout",
    "synchronize_seqscans",
    "TimeZone",
    "transaction_isolation",
    "transaction_timeout",
    "xmloption",
];

/// Emits the warnings a statement's parse raised, ahead of running it —
/// PostgreSQL reports them in that order (e.g. `timestamp(7)` clamping).
fn emit_parse_warnings(
    parser: &mut parser::Parser,
    responder: &mut Responder,
) -> Result<(), WireFull> {
    let (messages, n) = parser.take_warnings();
    for message in &messages[..n] {
        responder.warning(eval::sqlstate::INVALID_PARAMETER_VALUE, message.as_str())?;
    }
    Ok(())
}

fn report_parse_error(responder: &mut Responder, e: &ParseError) -> Result<(), WireFull> {
    responder.error(e.sqlstate, e.message.as_str())
}

pub(crate) fn parse_error_to_sql(error: &ParseError) -> SqlError {
    SqlError {
        sqlstate: error.sqlstate,
        message: stack_format!(192, "{}", error.message.as_str()),
    }
}

/// Rewrites a CREATE SCHEMA element to create inside the new schema. An
/// element that already names that schema passes through; one naming another
/// schema is PostgreSQL's 42P15.
fn requalify_schema_element<'a>(
    element: &'a ast::CreateSchemaElement<'a>,
    schema: &'a str,
    arena: &'a Arena,
) -> Result<&'a Stmt<'a>, SqlError> {
    let requalify = |name: ast::QualName<'a>| -> Result<ast::QualName<'a>, SqlError> {
        match name.schema {
            None => Ok(ast::QualName {
                schema: Some(schema),
                name: name.name,
            }),
            Some(s) if s == schema => Ok(name),
            Some(s) => Err(sql_err!(
                crate::sql::eval::sqlstate::INVALID_SCHEMA_DEFINITION,
                "CREATE specifies a schema ({}) different from the one being created ({})",
                s,
                schema
            )),
        }
    };
    let rewritten = match element {
        ast::CreateSchemaElement::Table(c) => Stmt::CreateTable(ast::CreateTable {
            name: requalify(c.name)?,
            ..*c
        }),
        ast::CreateSchemaElement::View {
            name,
            or_replace,
            sql,
        } => Stmt::CreateView {
            name: requalify(*name)?,
            or_replace: *or_replace,
            sql,
        },
        ast::CreateSchemaElement::Index {
            name,
            table,
            columns,
            include_columns,
            nulls_not_distinct,
            predicate,
            predicate_text,
            unique,
        } => Stmt::CreateIndex {
            name,
            table: requalify(*table)?,
            columns,
            include_columns,
            nulls_not_distinct: *nulls_not_distinct,
            predicate: *predicate,
            predicate_text: *predicate_text,
            unique: *unique,
        },
        ast::CreateSchemaElement::Sequence {
            name,
            if_not_exists,
            options,
        } => Stmt::CreateSequence {
            name: requalify(*name)?,
            if_not_exists: *if_not_exists,
            options: *options,
        },
        ast::CreateSchemaElement::Domain(domain) => Stmt::CreateDomain(ast::CreateDomain {
            name: requalify(domain.name)?,
            ..*domain
        }),
        ast::CreateSchemaElement::Enum { name, labels } => Stmt::CreateEnum {
            name: requalify(*name)?,
            labels,
        },
    };
    arena
        .alloc(rewritten)
        .map(|r| &*r)
        .map_err(|_| query::arena_full_pub())
}

/// Reapplies one journal record to storage during recovery.
fn apply_wal_op(storage: &mut Storage, lsn: u64, operator: WalOp) -> Result<(), SqlError> {
    match operator {
        WalOp::Commit { .. } => {}
        WalOp::Truncate { .. } => {}
        WalOp::CreateReplicationSlot { name, restart_lsn } => {
            storage.create_replication_slot(crate::storage::SqlName::parse(name)?, restart_lsn)?;
        }
        WalOp::DropReplicationSlot { name } => storage.drop_replication_slot(name)?,
        WalOp::AdvanceReplicationSlot {
            name,
            confirmed_flush_lsn,
        } => {
            let advance = storage.prepare_replication_slot_advance(name, confirmed_flush_lsn)?;
            storage.apply_replication_slot_advance(advance);
        }
        WalOp::CreateTrigger {
            name,
            table_schema,
            table,
            function_schema,
            function,
            timing,
            level,
            events,
            update_columns,
            old_table,
            new_table,
            when,
            arguments,
            argument_count,
        } => {
            let Some(crate::storage::ResolvedRelation::Table(table_slot)) =
                storage.resolve_relation(Some(table_schema), table, 0)
            else {
                return Err(sql_err!(
                    sqlstate::UNDEFINED_TABLE,
                    "journal trigger targets unknown relation \"{}.{}\"",
                    table_schema,
                    table
                ));
            };
            let Some(function_slot) =
                storage.routine_slot_by_signature(function_schema, function, &[], 0)
            else {
                return Err(sql_err!(
                    sqlstate::UNDEFINED_FUNCTION,
                    "journal trigger references unknown function \"{}.{}\"",
                    function_schema,
                    function
                ));
            };
            if !matches!(
                storage.routine(function_slot).kind,
                crate::storage::RoutineKind::Trigger
            ) {
                return Err(sql_err!(
                    sqlstate::INVALID_OBJECT_DEFINITION,
                    "journal trigger function has invalid return type"
                ));
            }
            let slot = storage.create_trigger(
                crate::storage::TriggerSpec {
                    name: crate::storage::SqlName::parse(name)?,
                    table: table_slot,
                    function: function_slot,
                    timing,
                    level,
                    events,
                    update_columns,
                    transition_tables: crate::storage::TriggerTransitionTables::from_names(
                        old_table, new_table,
                    )
                    .ok_or_else(|| {
                        sql_err!(
                            sqlstate::INVALID_OBJECT_DEFINITION,
                            "journal trigger has duplicate transition table names"
                        )
                    })?,
                    when: when
                        .map(crate::storage::trigger_when_stackstr)
                        .transpose()?,
                    arguments: crate::storage::TriggerArguments::parse(
                        &arguments[..argument_count],
                    )?,
                },
                0,
            )?;
            storage.commit_trigger_create(slot);
        }
        WalOp::DropTrigger {
            name,
            table_schema,
            table,
        } => {
            let Some(crate::storage::ResolvedRelation::Table(table_slot)) =
                storage.resolve_relation(Some(table_schema), table, 0)
            else {
                return Err(sql_err!(
                    sqlstate::UNDEFINED_TABLE,
                    "journal trigger targets unknown relation"
                ));
            };
            if let Some(slot) = storage.trigger_slot(table_slot, name, 0) {
                storage.drop_trigger(slot, 0);
                storage.commit_trigger_drop(slot);
            }
        }
        WalOp::AlterTrigger {
            name,
            table_schema,
            table,
            new_name,
            enabled,
        } => {
            let Some(crate::storage::ResolvedRelation::Table(table_slot)) =
                storage.resolve_relation(Some(table_schema), table, 0)
            else {
                return Err(sql_err!(
                    sqlstate::UNDEFINED_TABLE,
                    "journal trigger targets unknown relation"
                ));
            };
            let slot = storage.trigger_slot(table_slot, name, 0).ok_or_else(|| {
                sql_err!(
                    sqlstate::UNDEFINED_OBJECT,
                    "journal alters unknown trigger \"{}\"",
                    name
                )
            })?;
            storage.alter_trigger(slot, crate::storage::SqlName::parse(new_name)?, enabled, 0)?;
            storage.commit_trigger_alter(slot, 0);
        }
        WalOp::CreateTable(def) => {
            // A journal written before its schema existed cannot occur going
            // forward (CreateSchema precedes in LSN order), but a pre-schema
            // journal names only public, which always exists.
            if !storage.complete_replay_table_rewrite(def)? {
                storage.create_table(def)?;
            }
        }
        WalOp::BeginTableRewrite {
            previous_schema,
            previous_name,
            column_mapping,
        } => {
            storage.begin_replay_table_rewrite(previous_schema, previous_name, column_mapping)?;
        }
        WalOp::SequenceSet {
            schema,
            table,
            column,
            last,
        } => {
            let Some(index) = storage.find_table(schema, table) else {
                return Err(SqlError {
                    sqlstate: sqlstate::UNDEFINED_TABLE,
                    message: stack_format!(
                        192,
                        "journal sets a sequence of unknown table \"{}\"",
                        table
                    ),
                });
            };
            let t = storage.table_mut(index);
            if (column as usize) < crate::storage::MAX_COLUMNS {
                t.serial_last[column as usize] = last;
            }
        }
        WalOp::Analyze {
            schema,
            table,
            statistics,
        } => {
            let Some(index) = storage.find_table(schema, table) else {
                return Err(sql_err!(
                    sqlstate::UNDEFINED_TABLE,
                    "journal analyzes unknown table \"{}\"",
                    table
                ));
            };
            storage.replay_table_statistics(index, statistics.materialize()?);
        }
        WalOp::DropTable { schema, name } => {
            let Some(index) = storage.find_table(schema, name) else {
                return Err(SqlError {
                    sqlstate: sqlstate::UNDEFINED_TABLE,
                    message: stack_format!(192, "journal drops unknown table \"{}\"", name),
                });
            };
            storage.drop_table(index);
            storage.drop_indexes_for(schema, name, 0);
            storage.commit_indexes_for(schema, name, 0);
        }
        WalOp::Upsert {
            schema,
            table,
            rowid,
            row,
            ..
        } => {
            let Some(index) = storage.find_table(schema, table) else {
                return Err(SqlError {
                    sqlstate: sqlstate::UNDEFINED_TABLE,
                    message: stack_format!(192, "journal writes to unknown table \"{}\"", table),
                });
            };
            let (loc, slice) = storage.heap.append(row.len())?;
            slice.copy_from_slice(row);
            storage.observe_rowid(rowid);
            storage
                .table_mut(index)
                .rows
                .insert(rowid, crate::storage::RowState::committed_only_at(loc, lsn))
                .map_err(|e| SqlError {
                    sqlstate: sqlstate::PROGRAM_LIMIT_EXCEEDED,
                    message: stack_format!(192, "journal replay overflows {}", e.what),
                })?;
        }
        WalOp::Delete {
            schema,
            table,
            rowid,
            ..
        } => {
            let Some(index) = storage.find_table(schema, table) else {
                return Err(SqlError {
                    sqlstate: sqlstate::UNDEFINED_TABLE,
                    message: stack_format!(192, "journal deletes from unknown table \"{}\"", table),
                });
            };
            storage.remove_committed(index, rowid, lsn);
        }
        WalOp::CreateView {
            schema,
            name,
            sql,
            path,
            dependencies,
        } => {
            // Replay reconstructs committed state: create then promote.
            let mut buffer = crate::util::StackStr::<{ crate::storage::VIEW_SQL_MAX }>::new();
            use core::fmt::Write;
            let _ = write!(buffer, "{sql}");
            let mut creation_path = crate::util::StackStr::<128>::new();
            let _ = write!(creation_path, "{path}");
            let dependencies =
                storage.rebind_stored_query_dependencies(dependencies.materialize()?, 0)?;
            let (new_slot, old_slot) = storage.create_view(
                crate::storage::SqlName::parse(schema)?,
                crate::storage::SqlName::parse(name)?,
                crate::storage::StoredQueryDefinition {
                    sql: buffer,
                    creation_path,
                    dependencies,
                },
                true,
                0,
            )?;
            storage.commit_view_create(new_slot);
            if let Some(old) = old_slot {
                storage.commit_view_drop(old);
            }
        }
        WalOp::DropView { schema, name } => {
            if let Some(slot) = storage.drop_view(schema, name, 0)? {
                storage.commit_view_drop(slot);
            }
        }
        WalOp::CreatePublication {
            name,
            owner,
            all_tables,
            tables,
            table_count,
            schemas,
            schema_count,
            publish_insert,
            publish_update,
            publish_delete,
            publish_truncate,
        } => {
            let slot = storage.create_publication(
                crate::storage::PublicationSpec {
                    name: crate::storage::SqlName::parse(name)?,
                    all_tables,
                    tables: &tables[..table_count],
                    schemas: &schemas[..schema_count],
                    publish_insert,
                    publish_update,
                    publish_delete,
                    publish_truncate,
                },
                0,
            )?;
            storage.restore_publication_owner(slot, owner);
            storage.commit_publication_create(slot);
        }
        WalOp::DropPublication { name } => {
            if let Some(slot) = storage.drop_publication(name, 0)? {
                storage.commit_publication_drop(slot);
            }
        }
        WalOp::AlterPublication {
            name,
            all_tables,
            tables,
            table_count,
            schemas,
            schema_count,
            publish_insert,
            publish_update,
            publish_delete,
            publish_truncate,
        } => {
            let definition = crate::storage::PublicationDefinition {
                all_tables,
                tables,
                table_count,
                schemas,
                schema_count,
                publish_insert,
                publish_update,
                publish_delete,
                publish_truncate,
            };
            let (slot, _) = storage.alter_publication(name, definition, 0)?;
            storage.commit_publication_alter(slot, 0);
        }
        WalOp::SetPublicationOwner { name, owner } => {
            let (slot, _) = storage.publication_definition(name, 0).ok_or_else(|| {
                sql_err!(
                    sqlstate::UNDEFINED_OBJECT,
                    "journal owner change for unknown publication \"{}\"",
                    name
                )
            })?;
            storage.restore_publication_owner(slot, owner);
        }
        WalOp::RenamePublication { name, new_name } => {
            let (slot, _) = storage.publication_definition(name, 0).ok_or_else(|| {
                sql_err!(
                    sqlstate::UNDEFINED_OBJECT,
                    "journal rename for unknown publication \"{}\"",
                    name
                )
            })?;
            storage.rename_publication(slot, crate::storage::SqlName::parse(new_name)?, 0)?;
            storage.commit_publication_rename(slot, 0);
        }
        WalOp::CreateSubscription {
            name,
            owner,
            connection,
            publications,
            publication_count,
            enabled,
            slot_name,
        } => {
            let connection = crate::storage::SubscriptionConnInfo::parse(connection)?;
            if enabled {
                validate_recovered_enabled_subscription(connection)?;
            }
            let slot = storage.create_subscription(
                crate::storage::SubscriptionSpec {
                    name: crate::storage::SqlName::parse(name)?,
                    connection,
                    publications: &publications[..publication_count],
                    enabled,
                    slot_name: crate::storage::SqlName::parse(slot_name)?,
                },
                0,
            )?;
            storage.restore_subscription_owner(slot, owner);
            storage.commit_subscription_create(slot);
        }
        WalOp::DropSubscription { name } => {
            if let Some(slot) = storage.drop_subscription(name, 0)? {
                storage.commit_subscription_drop(slot);
            }
        }
        WalOp::AdvanceSubscription {
            name,
            confirmed_lsn,
        } => {
            if let Some(advance) = storage.subscription_advance(name, confirmed_lsn, 0)? {
                storage.apply_subscription_advance(advance);
            }
        }
        WalOp::SetSubscriptionEnabled { name, enabled } => {
            let (slot, subscription) = storage.subscription(name, 0).ok_or_else(|| {
                sql_err!(
                    sqlstate::UNDEFINED_OBJECT,
                    "journal subscription state change for unknown subscription \"{}\"",
                    name
                )
            })?;
            if enabled {
                validate_recovered_enabled_subscription(subscription.connection)?;
            }
            if matches!(
                storage.set_subscription_enabled(slot, enabled, 0)?,
                crate::storage::SubscriptionEnabledChange::Changed { .. }
            ) {
                storage.commit_subscription_enabled(slot, 0);
            }
        }
        WalOp::AlterSubscription {
            name,
            connection,
            publications,
            publication_count,
        } => {
            let (slot, subscription) = storage.subscription(name, 0).ok_or_else(|| {
                sql_err!(
                    sqlstate::UNDEFINED_OBJECT,
                    "journal subscription definition change for unknown subscription \"{}\"",
                    name
                )
            })?;
            let connection = crate::storage::SubscriptionConnInfo::parse(connection)?;
            if subscription.enabled_to(0) {
                validate_recovered_enabled_subscription(connection)?;
            }
            if storage
                .set_subscription_definition(
                    slot,
                    connection,
                    &publications[..publication_count],
                    0,
                )?
                .changed
            {
                storage.commit_subscription_definition(slot, 0);
            }
        }
        WalOp::RenameIndex {
            schema,
            name,
            new_name,
        } => {
            let slot = storage.index_slot(schema, name, 0).ok_or_else(|| {
                sql_err!(
                    sqlstate::UNDEFINED_OBJECT,
                    "journal rename for unknown index \"{}.{}\"",
                    schema,
                    name
                )
            })?;
            storage.rename_index(slot, crate::storage::SqlName::parse(new_name)?, 0)?;
            storage.commit_index_rename(slot, 0);
        }
        WalOp::CreateMatview {
            schema,
            name,
            sql,
            path,
            dependencies,
            populated,
        } => {
            use core::fmt::Write;
            let mut buffer = crate::util::StackStr::<{ crate::storage::VIEW_SQL_MAX }>::new();
            let _ = write!(buffer, "{sql}");
            let mut creation_path = crate::util::StackStr::<128>::new();
            let _ = write!(creation_path, "{path}");
            let dependencies =
                storage.rebind_stored_query_dependencies(dependencies.materialize()?, 0)?;
            let slot = storage.create_matview(
                crate::storage::SqlName::parse(schema)?,
                crate::storage::SqlName::parse(name)?,
                crate::storage::StoredQueryDefinition {
                    sql: buffer,
                    creation_path,
                    dependencies,
                },
                populated,
                0,
            )?;
            storage.commit_matview_create(slot);
        }
        WalOp::DropMatview { schema, name } => {
            if let Some(slot) = storage.drop_matview(schema, name, 0)? {
                storage.commit_matview_drop(slot);
            }
        }
        WalOp::SetMatviewPopulated {
            schema,
            name,
            populated,
        } => {
            if let Some(slot) = storage.matview_slot(schema, name, 0) {
                storage.set_matview_populated(slot, populated);
            }
        }
        WalOp::CreateSequence {
            schema,
            name,
            data_type,
            increment,
            min_value,
            max_value,
            start_value,
            cache,
            cycle,
            owner,
            generator_for,
        } => {
            let spec = crate::storage::SeqSpec {
                data_type: crate::storage::SeqType::from_u8(data_type),
                increment,
                min_value,
                max_value,
                start_value,
                cache,
                cycle,
            };
            // An ALTER replays as CreateSequence: if the sequence already exists,
            // redefine it in place; otherwise create it.
            if let Some(slot) = storage.sequence_slot(schema, name, 0) {
                storage.stage_sequence_alter(slot, spec, owner, generator_for, None, 0)?;
                storage.commit_sequence_alter(slot, 0);
            } else {
                let slot = storage.create_sequence(
                    crate::storage::SqlName::parse(schema)?,
                    crate::storage::SqlName::parse(name)?,
                    spec,
                    owner,
                    generator_for,
                    0,
                )?;
                storage.commit_sequence_create(slot);
            }
        }
        WalOp::DropSequence { schema, name } => {
            if let Some(slot) = storage.drop_sequence(schema, name, 0)? {
                storage.commit_sequence_drop(slot);
            }
        }
        WalOp::SequenceAdvance {
            schema,
            name,
            last,
            is_called,
        } => {
            storage.apply_sequence_advance(schema, name, last, is_called);
        }
        WalOp::CreateDomain(def) => {
            // An ALTER replays as a redefinition: redefine in place if it
            // exists, else create it committed (txid 0).
            let spec = crate::storage::DomainSpec {
                base_domain: def.base_domain,
                base: def.base,
                base_type_mod: def.base_type_mod,
                not_null: def.not_null,
                default_expr: def.default_expr,
                checks: def.checks,
                n_checks: def.n_checks,
            };
            if let Some(slot) = storage.domain_slot(def.schema.as_str(), def.name.as_str(), 0) {
                storage.stage_domain_alter(slot, spec, 0)?;
                storage.commit_domain_alter(slot, 0);
            } else {
                storage.create_domain(def.schema, def.name, spec, 0)?;
            }
        }
        WalOp::DropDomain { schema, name } => {
            if let Some(slot) = storage.drop_domain(schema, name, 0)? {
                storage.commit_domain_drop(slot);
            }
        }
        WalOp::CreateRoutine(definition) => storage.replay_create_routine(definition)?,
        WalOp::DropRoutine {
            schema,
            name,
            argument_type_codes,
        } => {
            let mut argument_types =
                [crate::sql::types::ColType::Text; crate::storage::MAX_ROUTINE_ARGUMENTS];
            if argument_type_codes.len() > argument_types.len() {
                return Err(sql_err!(
                    sqlstate::PROGRAM_LIMIT_EXCEEDED,
                    "too many routine arguments in WAL"
                ));
            }
            for (index, code) in argument_type_codes.iter().enumerate() {
                argument_types[index] =
                    crate::sql::types::ColType::from_code(*code).ok_or_else(|| {
                        sql_err!(
                            sqlstate::DATA_EXCEPTION,
                            "invalid routine argument type in WAL"
                        )
                    })?;
            }
            if let Some(slot) = storage.routine_slot_by_signature(
                schema,
                name,
                &argument_types[..argument_type_codes.len()],
                0,
            ) {
                storage.drop_routine(slot, 0);
                storage.commit_routine_drop(slot);
            }
        }
        WalOp::AlterRoutineIdentity {
            schema,
            name,
            argument_type_codes,
            new_schema,
            new_name,
        } => {
            let mut argument_types =
                [crate::sql::types::ColType::Text; crate::storage::MAX_ROUTINE_ARGUMENTS];
            if argument_type_codes.len() > argument_types.len() {
                return Err(sql_err!(
                    crate::sql::eval::sqlstate::PROGRAM_LIMIT_EXCEEDED,
                    "journal routine has too many arguments"
                ));
            }
            for (index, code) in argument_type_codes.iter().enumerate() {
                argument_types[index] =
                    crate::sql::types::ColType::from_code(*code).ok_or_else(|| {
                        sql_err!(
                            crate::sql::eval::sqlstate::INTERNAL_ERROR,
                            "journal routine has invalid argument type"
                        )
                    })?;
            }
            let slot = storage
                .routine_slot_by_signature(
                    schema,
                    name,
                    &argument_types[..argument_type_codes.len()],
                    0,
                )
                .ok_or_else(|| {
                    sql_err!(
                        crate::sql::eval::sqlstate::UNDEFINED_FUNCTION,
                        "journal identity change for unknown routine \"{}\"",
                        name
                    )
                })?;
            storage.alter_routine_identity(
                slot,
                crate::storage::SqlName::parse(new_schema)?,
                crate::storage::SqlName::parse(new_name)?,
                0,
            )?;
            storage.commit_routine_identity(slot, 0);
        }
        WalOp::CreateEnum(def) => {
            // An ALTER ... ADD VALUE replays as a redefinition: redefine in
            // place if the enum exists, else create it committed (txid 0).
            let spec = crate::storage::EnumSpec {
                members: def.members,
                n_members: def.n_members,
            };
            if let Some(slot) = storage.enum_slot(def.schema.as_str(), def.name.as_str(), 0) {
                let mut definition = storage.enum_for(slot, 0);
                definition.members = spec.members;
                definition.n_members = spec.n_members;
                storage.stage_enum_alter(slot, definition, 0)?;
                storage.commit_enum_alter(slot, 0);
            } else {
                storage.create_enum(def.schema, def.name, spec, 0)?;
            }
        }
        WalOp::DropEnum { schema, name } => {
            if let Some(slot) = storage.drop_enum(schema, name, 0)? {
                storage.commit_enum_drop(slot);
            }
        }
        WalOp::RenameEnum {
            schema,
            old_name,
            new_name,
        } => {
            let slot = storage.enum_slot(schema, old_name, 0).ok_or_else(|| {
                sql_err!(
                    eval::sqlstate::UNDEFINED_OBJECT,
                    "enum type \"{}\" for WAL rename does not exist",
                    old_name
                )
            })?;
            let mut definition = storage.enum_for(slot, 0);
            definition.name = crate::storage::SqlName::parse(new_name)?;
            storage.stage_enum_alter(slot, definition, 0)?;
            storage.commit_enum_alter(slot, 0);
        }
        WalOp::AlterDomainIdentity {
            schema,
            name,
            new_schema,
            new_name,
        } => {
            let slot = storage.domain_slot(schema, name, 0).ok_or_else(|| {
                sql_err!(
                    eval::sqlstate::UNDEFINED_OBJECT,
                    "domain \"{}\" for WAL identity change does not exist",
                    name
                )
            })?;
            storage.stage_domain_identity(
                slot,
                crate::storage::SqlName::parse(new_schema)?,
                crate::storage::SqlName::parse(new_name)?,
                0,
            )?;
            storage.commit_domain_alter(slot, 0);
        }
        WalOp::Comment {
            class,
            schema,
            name,
            subid,
            text,
        } => {
            let stored = text.map(crate::storage::comment_stackstr).transpose()?;
            let class = crate::storage::CommentClass::from_u8(class).ok_or_else(|| {
                sql_err!(
                    sqlstate::INTERNAL_ERROR,
                    "corrupt WAL comment class {}",
                    class
                )
            })?;
            storage.apply_comment(
                class,
                crate::storage::SqlName::parse(schema)?,
                crate::storage::SqlName::parse(name)?,
                subid,
                stored,
            )?;
        }
        WalOp::CreateIndex {
            schema,
            name,
            table,
            columns,
            include_columns,
            descending,
            nulls_first,
            n_cols,
            n_include_cols,
            nulls_not_distinct,
            predicate,
            expressions,
            unique,
        } => {
            let mut stored_expressions = [None; crate::storage::MAX_INDEX_COLS];
            for (index, expression) in expressions.into_iter().enumerate() {
                stored_expressions[index] = expression
                    .map(crate::storage::index_expression_stackstr)
                    .transpose()?;
            }
            let slot = storage.create_index(
                crate::storage::IndexDef {
                    schema: crate::storage::SqlName::parse(schema)?,
                    name: crate::storage::SqlName::parse(name)?,
                    pending_name: None,
                    table: crate::storage::SqlName::parse(table)?,
                    ownership: crate::storage::Ownership::BOOTSTRAP,
                    columns,
                    expressions: stored_expressions,
                    include_columns,
                    descending,
                    nulls_first,
                    n_cols,
                    n_include_cols,
                    nulls_not_distinct,
                    predicate: predicate
                        .map(crate::storage::index_predicate_stackstr)
                        .transpose()?,
                    unique,
                    ddl_state: crate::storage::CatalogDdlState::Present,
                },
                0,
            )?;
            storage.commit_index_create(slot);
        }
        WalOp::DropIndex { schema, name } => {
            if let Some(slot) = storage.drop_index(schema, name, 0)? {
                storage.commit_index_drop(slot);
            }
        }
        WalOp::CreateSchema(name) => {
            storage.create_schema(crate::storage::SqlName::parse(name)?)?;
        }
        WalOp::DropSchema(name) => {
            if let Some(slot) = storage.find_schema(name) {
                storage.drop_schema(slot);
            }
        }
        WalOp::SetTableSchema {
            schema,
            name,
            new_schema,
        } => {
            let Some(index) = storage.find_table(schema, name) else {
                return Err(SqlError {
                    sqlstate: sqlstate::UNDEFINED_TABLE,
                    message: stack_format!(192, "journal moves unknown table \"{}\"", name),
                });
            };
            storage.move_table_schema(index, crate::storage::SqlName::parse(new_schema)?);
        }
        WalOp::DropTableFk {
            schema,
            table,
            fk_name,
        } => {
            let Some(index) = storage.find_table(schema, table) else {
                return Err(SqlError {
                    sqlstate: sqlstate::UNDEFINED_TABLE,
                    message: stack_format!(
                        192,
                        "journal severs a key of unknown table \"{}\"",
                        table
                    ),
                });
            };
            let _ = storage.drop_fk(index, fk_name);
        }
        WalOp::UpsertRole { name, attributes } => {
            storage.install_role(crate::storage::SqlName::parse(name)?, attributes)?;
        }
        WalOp::DropRole { name } => storage.remove_role(name),
        WalOp::UpsertRoleMembership {
            role,
            member,
            grantor,
            options,
        } => {
            storage.install_role_membership(role, member, grantor, options)?;
        }
        WalOp::DropRoleMembership { role, member } => {
            storage.remove_role_membership(role, member);
        }
        WalOp::SetObjectOwner {
            class,
            object_oid,
            schema,
            name,
            owner,
        } => {
            let class = crate::storage::AccessClass::from_u8(class).ok_or_else(|| {
                sql_err!(
                    sqlstate::INTERNAL_ERROR,
                    "corrupt WAL object class {}",
                    class
                )
            })?;
            let object = (if class == crate::storage::AccessClass::Routine {
                storage
                    .routine_slot_by_oid(object_oid, 0)
                    .map(Storage::routine_access_object)
            } else {
                storage.resolve_access_object(class, schema, name, 0)
            })
            .ok_or_else(|| {
                sql_err!(
                    sqlstate::UNDEFINED_OBJECT,
                    "WAL ownership target \"{}\" does not exist",
                    name
                )
            })?;
            let owner = storage.find_role(owner).ok_or_else(|| {
                sql_err!(
                    sqlstate::UNDEFINED_OBJECT,
                    "WAL owner role \"{}\" does not exist",
                    owner
                )
            })?;
            let old_owner = storage.object_owner(object, 0) as u16;
            let acl_count = storage.acl_entries().count();
            for slot in 0..acl_count {
                let entry = *storage.acl_entry(slot);
                if entry.object != object || entry.object.slot == u16::MAX {
                    continue;
                }
                let (grantee, grantor) = storage.acl_identity(slot, 0);
                if grantee == old_owner || grantor == old_owner {
                    storage.change_acl_identity(
                        slot,
                        if grantee == old_owner {
                            owner as u16
                        } else {
                            grantee
                        },
                        if grantor == old_owner {
                            owner as u16
                        } else {
                            grantor
                        },
                        0,
                    );
                }
            }
            storage.set_object_owner(object, owner, 0);
        }
        WalOp::SetObjectAcl {
            class,
            object_oid,
            schema,
            name,
            grantee,
            grantor,
            privileges,
            grant_options,
        } => {
            let class = crate::storage::AccessClass::from_u8(class).ok_or_else(|| {
                sql_err!(
                    sqlstate::INTERNAL_ERROR,
                    "corrupt WAL object class {}",
                    class
                )
            })?;
            let object = if class == crate::storage::AccessClass::Routine {
                storage
                    .routine_slot_by_oid(object_oid, 0)
                    .map(Storage::routine_access_object)
            } else {
                storage.resolve_access_object(class, schema, name, 0)
            };
            let Some(object) = object else {
                if privileges.0 == 0 {
                    storage.set_lsn(lsn);
                    return Ok(());
                }
                return Err(sql_err!(
                    sqlstate::UNDEFINED_OBJECT,
                    "WAL privilege target \"{}\" does not exist",
                    name
                ));
            };
            let grantee = if grantee == "PUBLIC" {
                crate::storage::PUBLIC_ROLE
            } else {
                let Some(grantee_slot) = storage.find_role(grantee) else {
                    if privileges.0 == 0 {
                        storage.set_lsn(lsn);
                        return Ok(());
                    }
                    return Err(sql_err!(
                        sqlstate::UNDEFINED_OBJECT,
                        "WAL grantee role \"{}\" does not exist",
                        grantee
                    ));
                };
                grantee_slot as u16
            };
            let Some(grantor_slot) = storage.find_role(grantor) else {
                if privileges.0 == 0 {
                    storage.set_lsn(lsn);
                    return Ok(());
                }
                return Err(sql_err!(
                    sqlstate::UNDEFINED_OBJECT,
                    "WAL grantor role \"{}\" does not exist",
                    grantor
                ));
            };
            let grantor = grantor_slot as u16;
            storage.change_acl(object, grantee, grantor, privileges, grant_options, 0)?;
        }
        WalOp::SetDefaultAcl {
            owner,
            schema,
            class,
            grantee,
            defined,
            privileges,
            grant_options,
        } => {
            let class = crate::storage::DefaultPrivilegeClass::from_u8(class).ok_or_else(|| {
                sql_err!(
                    sqlstate::INTERNAL_ERROR,
                    "corrupt WAL default privilege class {}",
                    class
                )
            })?;
            let Some(owner_slot) = storage.find_role(owner) else {
                if !defined {
                    storage.set_lsn(lsn);
                    return Ok(());
                }
                return Err(sql_err!(
                    sqlstate::UNDEFINED_OBJECT,
                    "WAL default privilege owner \"{}\" does not exist",
                    owner
                ));
            };
            let owner = owner_slot as u16;
            let schema = if schema.is_empty() {
                crate::storage::DEFAULT_ACL_ALL_SCHEMAS
            } else {
                let Some(schema_slot) = storage.find_schema(schema) else {
                    if !defined {
                        storage.set_lsn(lsn);
                        return Ok(());
                    }
                    return Err(sql_err!(
                        sqlstate::INVALID_SCHEMA_NAME,
                        "WAL default privilege schema \"{}\" does not exist",
                        schema
                    ));
                };
                schema_slot as u16
            };
            let grantee = if grantee == "PUBLIC" {
                crate::storage::PUBLIC_ROLE
            } else {
                let Some(grantee_slot) = storage.find_role(grantee) else {
                    if !defined {
                        storage.set_lsn(lsn);
                        return Ok(());
                    }
                    return Err(sql_err!(
                        sqlstate::UNDEFINED_OBJECT,
                        "WAL default privilege grantee \"{}\" does not exist",
                        grantee
                    ));
                };
                grantee_slot as u16
            };
            storage.change_default_acl(
                crate::storage::DefaultAclKey {
                    owner,
                    schema,
                    class,
                    grantee,
                },
                defined,
                privileges,
                grant_options,
                0,
            )?;
        }
    }
    storage.set_lsn(lsn);
    Ok(())
}

fn validate_recovered_enabled_subscription(
    connection: crate::storage::SubscriptionConnInfo,
) -> Result<(), SqlError> {
    let endpoint = connection.require_endpoint()?;
    if endpoint.application_name().is_none() {
        return Err(sql_err!(
            sqlstate::SYNTAX_ERROR,
            "enabled subscriptions require application_name in the connection string"
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests;
