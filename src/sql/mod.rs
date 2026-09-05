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
pub(crate) mod event_trigger;
pub mod exec;
mod explain;
pub(crate) mod external;
pub(crate) mod foreign;
pub mod full_text;
pub mod geometry;
pub mod guc;
pub mod json;
pub(crate) mod large_object;
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
pub(crate) mod two_phase;
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
use ast::{Delete, Expr, Insert, Stmt, TransactionIsolation, TransactionTarget, Update};
use eval::{
    EvalHooks, NO_HOOKS, NO_PARAMS, NoColumns, SequenceAccess, SqlError, SqlState, eval, sqlstate,
};
use exec::MAX_PROJ;
use guc::GucState;
use parser::{ParseError, Parser};
use prep::SqlPreparedPool;
use txn::{DdlUndo, StatisticsUndo, TxnMode, TxnState};
use types::{ColDesc, ColType, Datum};

type ReturningCapture<'a> = dyn for<'row> FnMut(&[Datum<'row>]) -> Result<(), SqlError> + 'a;

/// Complete durable input for binding one startup-reserved subscription worker.
/// Values are copied out of the catalog so the reactor never holds a catalog
/// borrow while it drives a network socket or applies a remote transaction.
#[derive(Clone, Copy)]
pub(crate) struct SubscriptionRuntime {
    pub stream: crate::storage::SubscriptionStream,
    pub endpoint: crate::pg::replication_client::ConnectionInfo,
    pub publications: [SqlName; crate::storage::MAX_SUBSCRIPTION_PUBLICATIONS],
    pub publication_count: usize,
    pub slot: Option<SqlName>,
    pub manage_slot_behavior: bool,
    pub bootstrap_slot: Option<SqlName>,
    pub drop_bootstrap_slot: bool,
    pub confirmed_lsn: u64,
    pub bootstrap: crate::storage::SubscriptionBootstrap,
    pub enabled: bool,
    pub behavior: crate::storage::SubscriptionBehavior,
}

#[derive(Clone, Copy)]
pub(crate) struct SubscriptionCleanupRuntime {
    pub created_at: u64,
    pub name: SqlName,
    pub endpoint: crate::pg::replication_client::ConnectionInfo,
    pub slot: SqlName,
}

#[derive(Debug)]
pub enum EngineSetupError {
    Budget(BudgetError),
    Wal(WalSetupError),
    Checkpoint(CheckpointSetupError),
    /// A storage operation during recovery failed loudly — e.g. the recovered
    /// data exceeds the configured value-index capacity.
    Storage(SqlError),
    ForeignTransport(String),
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
            Self::ForeignTransport(e) => write!(f, "{e}"),
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
    source: ast::MaterializedCteSource::Inline(&[]),
};

#[derive(Clone, Copy)]
pub(crate) struct ReplicationEmission<'a> {
    pub publications: &'a [SqlName],
    pub binary: bool,
    pub origin: crate::storage::SubscriptionOrigin,
    pub protocol: crate::pg::pgoutput::ProtocolVersion,
}

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
    /// Scratch for sorting SST entries at checkpoint.
    scratch: FixedVec<(u64, RowHome)>,
    /// Mutable physical-row identities selected by DML. Kept separate from
    /// checkpoint sort entries because a logical partitioned relation needs
    /// its leaf owner alongside the row identifier.
    dml_scratch: exec::DmlScratch,
    /// Physical images inserted by the active streamed COPY statement. This
    /// stays separate from trigger DML scratch so nested trigger statements
    /// cannot erase the AFTER STATEMENT transition relation.
    copy_transition_scratch: exec::DmlScratch,
    /// Scratch for heap compaction: every live row image across tables.
    compact_scratch: FixedVec<(u32, u64, u8, RowLoc)>,
    /// Shared execution arena: one query's materialized rows (ORDER BY /
    /// DISTINCT / GROUP BY buffers) live here, separate from the small
    /// per-connection AST arena. Single-threaded execution means one
    /// instance serves every connection; reset at the start of each
    /// statement. This is the `work_mem` analogue.
    work: Arena,
    next_txid: u32,
    max_connections: u32,
    max_prepared_transactions: usize,
    prepared_transactions: two_phase::PreparedTransactions,
    /// LISTEN/NOTIFY registry and delivery outbox, shared across every
    /// connection (see [`notify`]).
    notify: notify::NotifyState,
    /// The connection id whose message is currently being executed, set at each
    /// `execute_simple`/`execute_extended` entry so LISTEN/UNLISTEN/NOTIFY can
    /// stamp their buffered ops without threading the id through every arm.
    current_conn_id: i32,
    /// Snapshot exports are connection-scoped protocol capabilities. Their
    /// fixed registry both authenticates imports and pins row history until
    /// the exporting connection advances or closes.
    exported_snapshots: FixedVec<(i32, u64)>,
    /// Stable identity exposed by the replication protocol. It is derived
    /// from the durable namespace rather than process-local state, so a
    /// restarted or cold-recovered server remains the same publisher.
    replication_system_id: u64,
    /// Authenticated sessions per fixed role slot, used to enforce
    /// `CONNECTION LIMIT` without allocating in the server loop.
    role_connections: [u16; crate::storage::MAX_ROLES],
    database_connections: [u16; crate::storage::MAX_DATABASES],
    active_system_settings: [Option<ActiveSystemSetting>; crate::storage::MAX_SYSTEM_SETTINGS],
    system_settings_reloaded: bool,
    discard_protocol_state: bool,
}

#[derive(Clone, Copy)]
struct ActiveSystemSetting {
    name: crate::storage::SqlName,
    value: crate::util::StackStr<{ crate::storage::ROLE_SETTING_VALUE_MAX }>,
}

struct ConfigurationReloadScope {
    engine: *mut Engine,
    guc: *const GucState,
}

impl ConfigurationReloadScope {
    fn new(engine: &mut Engine, guc: &GucState) -> Self {
        eval::funcs::system::clear_configuration_reload_request();
        Self { engine, guc }
    }
}

impl Drop for ConfigurationReloadScope {
    fn drop(&mut self) {
        if !eval::funcs::system::take_configuration_reload_request() {
            return;
        }
        // The scope drops after statement evaluation has released every
        // Engine and GUC borrow; the pointers refer to its enclosing call.
        let engine = unsafe { &mut *self.engine };
        let guc = unsafe { &*self.guc };
        engine.reload_system_settings();
        engine
            .apply_system_settings(guc)
            .expect("stored system settings were validated before publication");
    }
}

#[derive(Clone, Copy)]
pub(crate) struct RoleLogin {
    pub slot: u16,
    pub can_login: bool,
    pub valid: bool,
    pub superuser: bool,
    pub replication: bool,
    pub connection_limit: i32,
    pub password: Option<crate::storage::RoleCredential>,
}

#[derive(Clone, Copy)]
pub(crate) struct DatabaseLogin {
    pub slot: u16,
    pub oid: crate::storage::DatabaseOid,
    pub allow_connections: bool,
    pub connection_limit: i32,
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
        | Stmt::PrepareTransaction(_)
        | Stmt::Savepoint(_)
        | Stmt::ReleaseSavepoint(_)
        | Stmt::RollbackToSavepoint(_)
        | Stmt::SetConstraints { .. }
        | Stmt::LockTable { .. }
        | Stmt::Set { .. }
        | Stmt::SetCatalog(_)
        | Stmt::Reset(_)
        | Stmt::SetTransaction { .. }
        | Stmt::SetTransactionSnapshot(_)
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
        Stmt::Call { .. } | Stmt::Do { .. } => true,
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
        | Stmt::AlterView { .. }
        | Stmt::AlterMaterializedView { .. }
        | Stmt::CreateRule(_)
        | Stmt::AlterRule { .. }
        | Stmt::DropRule(_)
        | Stmt::CreateRoutine(_)
        | Stmt::CreateAggregate(_)
        | Stmt::CreateCast(_)
        | Stmt::DropCast { .. }
        | Stmt::CreateTransform(_)
        | Stmt::DropTransform(_)
        | Stmt::CreateOperator(_)
        | Stmt::AlterOperator { .. }
        | Stmt::DropOperator { .. }
        | Stmt::CreateOperatorFamily { .. }
        | Stmt::AlterOperatorFamily { .. }
        | Stmt::DropOperatorFamily { .. }
        | Stmt::CreateOperatorClass(_)
        | Stmt::AlterOperatorClass { .. }
        | Stmt::DropOperatorClass { .. }
        | Stmt::CreateLanguage(_)
        | Stmt::AlterLanguage { .. }
        | Stmt::DropLanguage { .. }
        | Stmt::AlterRoutine { .. }
        | Stmt::AlterAggregate { .. }
        | Stmt::DropFunction { .. }
        | Stmt::DropProcedure { .. }
        | Stmt::DropRoutine { .. }
        | Stmt::DropAggregate { .. }
        | Stmt::CreateExtension { .. }
        | Stmt::AlterExtension { .. }
        | Stmt::AlterMaterializedViewExtensionDependency { .. }
        | Stmt::DropExtension { .. }
        | Stmt::DropView { .. }
        | Stmt::CreateCollation(_)
        | Stmt::AlterCollation { .. }
        | Stmt::DropCollation { .. }
        | Stmt::CreateConversion(_)
        | Stmt::AlterConversion { .. }
        | Stmt::DropConversion { .. }
        | Stmt::CreateForeignDataWrapper(_)
        | Stmt::AlterForeignDataWrapper { .. }
        | Stmt::DropForeignDataWrapper { .. }
        | Stmt::CreateForeignServer(_)
        | Stmt::AlterForeignServer { .. }
        | Stmt::DropForeignServer { .. }
        | Stmt::CreateUserMapping(_)
        | Stmt::AlterUserMapping(_)
        | Stmt::DropUserMapping(_)
        | Stmt::CreateForeignTable(_)
        | Stmt::AlterForeignTable(_)
        | Stmt::DropForeignTable(_)
        | Stmt::ImportForeignSchema(_)
        | Stmt::CreateTextSearchParser(_)
        | Stmt::CreateTextSearchTemplate(_)
        | Stmt::CreateTextSearchDictionary(_)
        | Stmt::CreateTextSearchConfiguration(_)
        | Stmt::AlterTextSearch { .. }
        | Stmt::DropTextSearch { .. }
        | Stmt::CreateEventTrigger(_)
        | Stmt::AlterEventTrigger { .. }
        | Stmt::DropEventTrigger { .. }
        | Stmt::CreatePublication { .. }
        | Stmt::AlterPublication { .. }
        | Stmt::DropPublication { .. }
        | Stmt::CreateSubscription { .. }
        | Stmt::AlterSubscription { .. }
        | Stmt::DropSubscription { .. }
        | Stmt::CreateTrigger(_)
        | Stmt::AlterTrigger { .. }
        | Stmt::DropTrigger { .. }
        | Stmt::CreatePolicy(_)
        | Stmt::AlterPolicy(_)
        | Stmt::DropPolicy { .. }
        | Stmt::CreateStatistics(_)
        | Stmt::AlterStatistics { .. }
        | Stmt::DropStatistics { .. }
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
        | Stmt::CreateComposite { .. }
        | Stmt::AlterType { .. }
        | Stmt::DropType { .. }
        | Stmt::CreateIndex { .. }
        | Stmt::AlterIndex { .. }
        | Stmt::AlterIndexesTablespace { .. }
        | Stmt::AlterTablesTablespace { .. }
        | Stmt::DropIndex { .. }
        | Stmt::CreateAccessMethod { .. }
        | Stmt::DropAccessMethod { .. }
        | Stmt::Reindex { .. }
        | Stmt::Cluster { .. }
        | Stmt::Checkpoint
        | Stmt::AlterSystem { .. }
        | Stmt::AlterTable(_)
        | Stmt::CreateSchema { .. }
        | Stmt::DropSchema { .. }
        | Stmt::AlterSchema { .. }
        | Stmt::Vacuum { .. }
        | Stmt::Notify { .. }
        | Stmt::Comment { .. }
        | Stmt::SecurityLabel { .. }
        | Stmt::Load(_)
        | Stmt::AlterOwner { .. }
        | Stmt::AlterLargeObjectOwner { .. }
        | Stmt::CreateRole { .. }
        | Stmt::AlterRole { .. }
        | Stmt::AlterRoleRename { .. }
        | Stmt::AlterRoleSetting { .. }
        | Stmt::DropRole { .. } => true,
        Stmt::Discard(_) => true,
        Stmt::GrantRole { .. }
        | Stmt::RevokeRole { .. }
        | Stmt::GrantPrivileges { .. }
        | Stmt::RevokePrivileges { .. }
        | Stmt::GrantParameterPrivileges { .. }
        | Stmt::RevokeParameterPrivileges { .. }
        | Stmt::AlterDefaultPrivileges { .. }
        | Stmt::ReassignOwned { .. }
        | Stmt::DropOwned { .. } => true,
        Stmt::CreateTablespace { .. }
        | Stmt::AlterTablespace { .. }
        | Stmt::DropTablespace { .. }
        | Stmt::CreateDatabase { .. }
        | Stmt::AlterDatabase { .. }
        | Stmt::DropDatabase { .. }
        | Stmt::CommitPrepared(_)
        | Stmt::RollbackPrepared(_) => true,
    }
}

fn apply_current_transaction_setting(
    txn: &mut TxnState,
    characteristics: ast::TransactionCharacteristics,
) -> Result<(), SqlError> {
    txn.apply_characteristics(characteristics)
}

fn created_access_object(undo: txn::DdlUndo) -> Option<crate::storage::AccessObject> {
    use crate::storage::{AccessClass, AccessObject};
    use txn::DdlUndo;
    let (class, slot) = match undo {
        DdlUndo::Created(slot) => (AccessClass::Table, slot),
        DdlUndo::ViewCreated(slot) => (AccessClass::View, slot),
        DdlUndo::MatviewCreated(slot) => (AccessClass::MaterializedView, slot),
        DdlUndo::RoutineCreated(slot) => (AccessClass::Routine, slot),
        DdlUndo::SequenceCreated(slot) => (AccessClass::Sequence, slot),
        DdlUndo::DomainCreated(slot) => (AccessClass::Domain, slot),
        DdlUndo::EnumCreated(slot) => (AccessClass::Enum, slot),
        DdlUndo::CompositeCreated(slot) => (AccessClass::Composite, slot),
        DdlUndo::IndexCreated(slot) => (AccessClass::Index, slot),
        DdlUndo::StatisticsCreated(slot) => (AccessClass::Statistics, slot),
        DdlUndo::SchemaCreated(slot) => (AccessClass::Schema, slot),
        _ => return None,
    };
    Some(AccessObject {
        class,
        slot: slot as u16,
    })
}

struct ExtensionScriptText<'a> {
    source: &'a str,
    schema: &'a str,
    substitute_schema: bool,
    owner: &'a str,
    required_names: &'a [crate::storage::SqlName],
    required_schemas: &'a [&'a str],
}

impl core::fmt::Display for ExtensionScriptText<'_> {
    fn fmt(&self, formatter: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        let mut rest = self.source;
        while let Some(at) = rest.find('@') {
            formatter.write_str(&rest[..at])?;
            rest = &rest[at..];
            if self.substitute_schema
                && let Some(after) = rest.strip_prefix("@extschema@")
            {
                formatter.write_str(self.schema)?;
                rest = after;
            } else if let Some(after) = rest.strip_prefix("@extowner@") {
                formatter.write_str(self.owner)?;
                rest = after;
            } else if let Some(after_prefix) = rest.strip_prefix("@extschema:")
                && let Some(end) = after_prefix.find('@')
            {
                let name = &after_prefix[..end];
                let position = self
                    .required_names
                    .iter()
                    .position(|required| required.as_str() == name)
                    .unwrap_or(usize::MAX);
                if position == usize::MAX {
                    formatter.write_str("@")?;
                    rest = &rest[1..];
                } else {
                    formatter.write_str(self.required_schemas[position])?;
                    rest = &after_prefix[end + 1..];
                }
            } else {
                formatter.write_str("@")?;
                rest = &rest[1..];
            }
        }
        formatter.write_str(rest)
    }
}

fn validate_extension_script_substitutions(
    source: &str,
    required_names: &[crate::storage::SqlName],
) -> Result<(), SqlError> {
    let mut rest = source;
    while let Some(at) = rest.find("@extschema:") {
        let after = &rest[at + "@extschema:".len()..];
        let Some(end) = after.find('@') else {
            return Err(sql_err!(
                sqlstate::SYNTAX_ERROR,
                "unterminated extension schema substitution"
            ));
        };
        let name = &after[..end];
        if !required_names
            .iter()
            .any(|required| required.as_str() == name)
        {
            return Err(sql_err!(
                sqlstate::UNDEFINED_OBJECT,
                "extension schema substitution references undeclared prerequisite \"{}\"",
                name
            ));
        }
        rest = &after[end + 1..];
    }
    Ok(())
}

fn extension_config_dump_arguments<'a>(statement: &'a Stmt<'a>) -> Option<[&'a Expr<'a>; 2]> {
    let Stmt::Select(select) = statement else {
        return None;
    };
    if select.items.len() != 1
        || select.distinct
        || !select.distinct_on.is_empty()
        || select.from.is_some()
        || select.where_clause.is_some()
        || !select.group_by.is_empty()
        || !select.grouping_sets.is_empty()
        || select.having.is_some()
        || !select.order_by.is_empty()
        || select.limit.is_some()
        || select.offset.is_some()
        || !select.with.is_empty()
        || select.set_body.is_some()
        || !select.locking.is_empty()
    {
        return None;
    }
    let ast::SelectItem::Expr { expression, .. } = select.items[0] else {
        return None;
    };
    let Expr::Call {
        name,
        args,
        star: false,
        distinct: false,
        order_by,
        over: None,
        filter: None,
        ..
    } = expression
    else {
        return None;
    };
    if !order_by.is_empty()
        || args.len() != 2
        || !(name.eq_ignore_ascii_case("pg_extension_config_dump")
            || name.eq_ignore_ascii_case("pg_catalog.pg_extension_config_dump"))
    {
        return None;
    }
    Some([args[0], args[1]])
}

fn write_extension_identifier<const N: usize>(
    output: &mut crate::util::StackStr<N>,
    name: &str,
) -> Result<(), SqlError> {
    use core::fmt::Write as _;
    write!(output, "\"").map_err(|_| query::arena_full_pub())?;
    for character in name.chars() {
        if character == '"' {
            write!(output, "\"").map_err(|_| query::arena_full_pub())?;
        }
        write!(output, "{}", character).map_err(|_| query::arena_full_pub())?;
    }
    write!(output, "\"").map_err(|_| query::arena_full_pub())
}

fn write_extension_qualified_identifier<const N: usize>(
    output: &mut crate::util::StackStr<N>,
    schema: &str,
    name: &str,
) -> Result<(), SqlError> {
    use core::fmt::Write as _;
    if !schema.is_empty() {
        write_extension_identifier(output, schema)?;
        write!(output, ".").map_err(|_| query::arena_full_pub())?;
    }
    write_extension_identifier(output, name)
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
        Stmt::Cluster { .. } => "CLUSTER",
        Stmt::Reindex { .. } => "REINDEX",
        Stmt::Notify { .. } => "NOTIFY",
        _ => "DDL",
    }
}

fn event_trigger_tag(statement: &Stmt<'_>) -> Option<&'static str> {
    Some(match statement {
        Stmt::CreateTable(_) => "CREATE TABLE",
        Stmt::DropTable(_) => "DROP TABLE",
        Stmt::Truncate { .. } => return None,
        Stmt::AlterTable(_) => "ALTER TABLE",
        Stmt::CreateView { .. } => "CREATE VIEW",
        Stmt::AlterView { .. } => "ALTER VIEW",
        Stmt::AlterMaterializedView { .. } => "ALTER MATERIALIZED VIEW",
        Stmt::DropView { .. } => "DROP VIEW",
        Stmt::CreateRule(_) => "CREATE RULE",
        Stmt::AlterRule { .. } => "ALTER RULE",
        Stmt::DropRule(_) => "DROP RULE",
        Stmt::RefreshMaterializedView { .. } => "REFRESH MATERIALIZED VIEW",
        Stmt::DropMaterializedView { .. } => "DROP MATERIALIZED VIEW",
        Stmt::CreateTableAs { kind, .. } => match kind {
            ast::CreateTableAsKind::Table => "CREATE TABLE AS",
            ast::CreateTableAsKind::MaterializedView => "CREATE MATERIALIZED VIEW",
            ast::CreateTableAsKind::SelectInto => "SELECT INTO",
        },
        Stmt::CreateRoutine(routine) => match routine.kind {
            ast::RoutineCreateKind::Procedure => "CREATE PROCEDURE",
            ast::RoutineCreateKind::Function { .. }
            | ast::RoutineCreateKind::OutputFunction { .. }
            | ast::RoutineCreateKind::TableFunction { .. }
            | ast::RoutineCreateKind::Trigger
            | ast::RoutineCreateKind::EventTrigger => "CREATE FUNCTION",
        },
        Stmt::AlterRoutine { kind, .. } => match kind {
            ast::RoutineTargetKind::Function => "ALTER FUNCTION",
            ast::RoutineTargetKind::Procedure => "ALTER PROCEDURE",
            ast::RoutineTargetKind::Aggregate => "ALTER AGGREGATE",
            ast::RoutineTargetKind::Either => "ALTER ROUTINE",
        },
        Stmt::DropFunction { .. } => "DROP FUNCTION",
        Stmt::DropProcedure { .. } => "DROP PROCEDURE",
        Stmt::DropRoutine { .. } => "DROP ROUTINE",
        Stmt::CreateAggregate(_) => "CREATE AGGREGATE",
        Stmt::AlterAggregate { .. } => "ALTER AGGREGATE",
        Stmt::DropAggregate { .. } => "DROP AGGREGATE",
        Stmt::CreateCast(_) => "CREATE CAST",
        Stmt::DropCast { .. } => "DROP CAST",
        Stmt::CreateOperator(_) => "CREATE OPERATOR",
        Stmt::AlterOperator { .. } => "ALTER OPERATOR",
        Stmt::DropOperator { .. } => "DROP OPERATOR",
        Stmt::CreateOperatorFamily { .. } => "CREATE OPERATOR FAMILY",
        Stmt::AlterOperatorFamily { .. } => "ALTER OPERATOR FAMILY",
        Stmt::DropOperatorFamily { .. } => "DROP OPERATOR FAMILY",
        Stmt::CreateOperatorClass(_) => "CREATE OPERATOR CLASS",
        Stmt::AlterOperatorClass { .. } => "ALTER OPERATOR CLASS",
        Stmt::DropOperatorClass { .. } => "DROP OPERATOR CLASS",
        Stmt::CreateLanguage(_) => "CREATE LANGUAGE",
        Stmt::AlterLanguage { .. } => "ALTER LANGUAGE",
        Stmt::DropLanguage { .. } => "DROP LANGUAGE",
        Stmt::CreateExtension { .. } => "CREATE EXTENSION",
        Stmt::AlterExtension { .. } => "ALTER EXTENSION",
        Stmt::AlterMaterializedViewExtensionDependency { .. } => "ALTER MATERIALIZED VIEW",
        Stmt::DropExtension { .. } => "DROP EXTENSION",
        Stmt::CreateCollation(_) => "CREATE COLLATION",
        Stmt::AlterCollation { .. } => "ALTER COLLATION",
        Stmt::DropCollation { .. } => "DROP COLLATION",
        Stmt::CreateConversion(_) => "CREATE CONVERSION",
        Stmt::AlterConversion { .. } => "ALTER CONVERSION",
        Stmt::DropConversion { .. } => "DROP CONVERSION",
        Stmt::CreateForeignDataWrapper(_) => "CREATE FOREIGN DATA WRAPPER",
        Stmt::AlterForeignDataWrapper { .. } => "ALTER FOREIGN DATA WRAPPER",
        Stmt::DropForeignDataWrapper { .. } => "DROP FOREIGN DATA WRAPPER",
        Stmt::CreateForeignServer(_) => "CREATE SERVER",
        Stmt::AlterForeignServer { .. } => "ALTER SERVER",
        Stmt::DropForeignServer { .. } => "DROP SERVER",
        Stmt::CreateUserMapping(_) => "CREATE USER MAPPING",
        Stmt::AlterUserMapping(_) => "ALTER USER MAPPING",
        Stmt::DropUserMapping(_) => "DROP USER MAPPING",
        Stmt::CreateForeignTable(_) => "CREATE FOREIGN TABLE",
        Stmt::AlterForeignTable(_) => "ALTER FOREIGN TABLE",
        Stmt::DropForeignTable(_) => "DROP FOREIGN TABLE",
        Stmt::ImportForeignSchema(_) => "IMPORT FOREIGN SCHEMA",
        Stmt::CreateTextSearchParser(_) => "CREATE TEXT SEARCH PARSER",
        Stmt::CreateTextSearchTemplate(_) => "CREATE TEXT SEARCH TEMPLATE",
        Stmt::CreateTextSearchDictionary(_) => "CREATE TEXT SEARCH DICTIONARY",
        Stmt::CreateTextSearchConfiguration(_) => "CREATE TEXT SEARCH CONFIGURATION",
        Stmt::AlterTextSearch { kind, .. } => match kind {
            ast::TextSearchObjectKind::Parser => "ALTER TEXT SEARCH PARSER",
            ast::TextSearchObjectKind::Template => "ALTER TEXT SEARCH TEMPLATE",
            ast::TextSearchObjectKind::Dictionary => "ALTER TEXT SEARCH DICTIONARY",
            ast::TextSearchObjectKind::Configuration => "ALTER TEXT SEARCH CONFIGURATION",
        },
        Stmt::DropTextSearch { kind, .. } => match kind {
            ast::TextSearchObjectKind::Parser => "DROP TEXT SEARCH PARSER",
            ast::TextSearchObjectKind::Template => "DROP TEXT SEARCH TEMPLATE",
            ast::TextSearchObjectKind::Dictionary => "DROP TEXT SEARCH DICTIONARY",
            ast::TextSearchObjectKind::Configuration => "DROP TEXT SEARCH CONFIGURATION",
        },
        Stmt::CreatePublication { .. } => "CREATE PUBLICATION",
        Stmt::AlterPublication { .. } => "ALTER PUBLICATION",
        Stmt::DropPublication { .. } => "DROP PUBLICATION",
        Stmt::CreateSubscription { .. } => "CREATE SUBSCRIPTION",
        Stmt::AlterSubscription { .. } => "ALTER SUBSCRIPTION",
        Stmt::DropSubscription { .. } => "DROP SUBSCRIPTION",
        Stmt::CreateTrigger(_) => "CREATE TRIGGER",
        Stmt::AlterTrigger { .. } => "ALTER TRIGGER",
        Stmt::DropTrigger { .. } => "DROP TRIGGER",
        Stmt::CreatePolicy(_) => "CREATE POLICY",
        Stmt::AlterPolicy(_) => "ALTER POLICY",
        Stmt::DropPolicy { .. } => "DROP POLICY",
        Stmt::CreateStatistics(_) => "CREATE STATISTICS",
        Stmt::AlterStatistics { .. } => "ALTER STATISTICS",
        Stmt::DropStatistics { .. } => "DROP STATISTICS",
        Stmt::CreateSequence { .. } => "CREATE SEQUENCE",
        Stmt::AlterSequence { .. } => "ALTER SEQUENCE",
        Stmt::DropSequence { .. } => "DROP SEQUENCE",
        Stmt::CreateDomain(_) => "CREATE DOMAIN",
        Stmt::AlterDomain { .. } => "ALTER DOMAIN",
        Stmt::DropDomain { .. } => "DROP DOMAIN",
        Stmt::CreateEnum { .. } | Stmt::CreateComposite { .. } => "CREATE TYPE",
        Stmt::AlterType { .. } => "ALTER TYPE",
        Stmt::DropType { .. } => "DROP TYPE",
        Stmt::CreateIndex { .. } => "CREATE INDEX",
        Stmt::AlterIndex { .. } | Stmt::AlterIndexesTablespace { .. } => "ALTER INDEX",
        Stmt::AlterTablesTablespace { .. } => "ALTER TABLE",
        Stmt::DropIndex { .. } => "DROP INDEX",
        Stmt::CreateAccessMethod { .. } => "CREATE ACCESS METHOD",
        Stmt::DropAccessMethod { .. } => "DROP ACCESS METHOD",
        Stmt::Cluster { .. } => "CLUSTER",
        Stmt::Reindex { .. } => "REINDEX",
        Stmt::CreateSchema { .. } => "CREATE SCHEMA",
        Stmt::DropSchema { .. } => "DROP SCHEMA",
        Stmt::AlterSchema { .. } => "ALTER SCHEMA",
        Stmt::Comment { target, .. }
            if !matches!(
                target,
                ast::CommentTarget::Tablespace(_)
                    | ast::CommentTarget::Database(_)
                    | ast::CommentTarget::EventTrigger(_)
            ) =>
        {
            "COMMENT"
        }
        Stmt::AlterOwner { kind, .. } => match kind {
            ast::AlterOwnerKind::Schema => "ALTER SCHEMA",
            ast::AlterOwnerKind::Type => "ALTER TYPE",
            ast::AlterOwnerKind::Domain => "ALTER DOMAIN",
            ast::AlterOwnerKind::Table => "ALTER TABLE",
            ast::AlterOwnerKind::ForeignTable => "ALTER FOREIGN TABLE",
            ast::AlterOwnerKind::View => "ALTER VIEW",
            ast::AlterOwnerKind::MaterializedView => "ALTER MATERIALIZED VIEW",
            ast::AlterOwnerKind::Sequence => "ALTER SEQUENCE",
            ast::AlterOwnerKind::Statistics => "ALTER STATISTICS",
        },
        Stmt::AlterLargeObjectOwner { .. } => "ALTER LARGE OBJECT",
        Stmt::GrantPrivileges { target, .. } if privilege_target_is_database_local(*target) => {
            "GRANT"
        }
        Stmt::RevokePrivileges { target, .. } if privilege_target_is_database_local(*target) => {
            "REVOKE"
        }
        Stmt::AlterDefaultPrivileges { .. } => "ALTER DEFAULT PRIVILEGES",
        Stmt::DropOwned { .. } => "DROP OWNED",
        Stmt::Analyze(_) | Stmt::Vacuum { .. } => return None,
        Stmt::CreateEventTrigger(_)
        | Stmt::AlterEventTrigger { .. }
        | Stmt::DropEventTrigger { .. } => return None,
        _ => return None,
    })
}

fn event_trigger_drop_command(statement: &Stmt<'_>) -> bool {
    match statement {
        Stmt::AlterTable(alter) => alter.actions.iter().any(|action| {
            matches!(
                action,
                ast::AlterAction::DropColumn { .. }
                    | ast::AlterAction::DropConstraint { .. }
                    | ast::AlterAction::DropNotNull { .. }
            )
        }),
        Stmt::DropOwned { .. } => true,
        statement => matches!(
            statement,
            Stmt::DropTable(_)
                | Stmt::DropView { .. }
                | Stmt::DropRule(_)
                | Stmt::DropMaterializedView { .. }
                | Stmt::DropFunction { .. }
                | Stmt::DropProcedure { .. }
                | Stmt::DropRoutine { .. }
                | Stmt::DropAggregate { .. }
                | Stmt::DropCast { .. }
                | Stmt::DropOperator { .. }
                | Stmt::DropOperatorFamily { .. }
                | Stmt::DropOperatorClass { .. }
                | Stmt::DropLanguage { .. }
                | Stmt::DropExtension { .. }
                | Stmt::DropCollation { .. }
                | Stmt::DropConversion { .. }
                | Stmt::DropTextSearch { .. }
                | Stmt::DropPublication { .. }
                | Stmt::DropSubscription { .. }
                | Stmt::DropTrigger { .. }
                | Stmt::DropPolicy { .. }
                | Stmt::DropStatistics { .. }
                | Stmt::DropSequence { .. }
                | Stmt::DropDomain { .. }
                | Stmt::DropType { .. }
                | Stmt::DropIndex { .. }
                | Stmt::DropAccessMethod { .. }
                | Stmt::DropSchema { .. }
        ),
    }
}

fn privilege_target_is_database_local(target: ast::PrivilegeTarget<'_>) -> bool {
    !matches!(
        target,
        ast::PrivilegeTarget::Objects {
            kind: ast::PrivilegeObjectKind::Database | ast::PrivilegeObjectKind::Tablespace,
            ..
        }
    )
}

#[derive(Clone, Copy)]
enum EventTriggerInvocation<'a> {
    Login,
    DdlCommandStart { tag: &'a str },
    DdlCommandEnd { tag: &'a str },
    SqlDrop { tag: &'a str },
    TableRewrite { relation_oid: i32, reason: i32 },
}

struct EventTriggerExecution<'a, 'response> {
    txn: &'a mut TxnState,
    cursors: &'a mut cursor::CursorPool,
    guc: &'a GucState,
    arena: &'a Arena,
    responder: &'a mut Responder<'response>,
}

fn has_event_trigger(
    storage: &Storage,
    txn: &TxnState,
    guc: &GucState,
    event: ast::EventTriggerEvent,
    tag: &str,
) -> bool {
    guc.event_triggers()
        && storage
            .event_triggers_visible_to(txn.txid)
            .any(|(_, trigger)| {
                trigger.event == event
                    && trigger.tags.matches(tag)
                    && if txn.replication_apply {
                        trigger.enabled.fires_for_replication()
                    } else {
                        trigger.enabled.fires_for_origin()
                    }
            })
}

impl<'a> EventTriggerInvocation<'a> {
    const fn event(self) -> ast::EventTriggerEvent {
        match self {
            Self::Login => ast::EventTriggerEvent::Login,
            Self::DdlCommandStart { .. } => ast::EventTriggerEvent::DdlCommandStart,
            Self::DdlCommandEnd { .. } => ast::EventTriggerEvent::DdlCommandEnd,
            Self::SqlDrop { .. } => ast::EventTriggerEvent::SqlDrop,
            Self::TableRewrite { .. } => ast::EventTriggerEvent::TableRewrite,
        }
    }

    const fn tag(self) -> &'a str {
        match self {
            Self::Login => "LOGIN",
            Self::DdlCommandStart { tag } | Self::DdlCommandEnd { tag } | Self::SqlDrop { tag } => {
                tag
            }
            Self::TableRewrite { .. } => "ALTER TABLE",
        }
    }
}

fn table_rewrite_target(
    statement: &Stmt<'_>,
    storage: &Storage,
    txid: u32,
) -> Option<(usize, i32)> {
    let Stmt::AlterTable(alter) = statement else {
        return None;
    };
    let Some(crate::storage::ResolvedRelation::Table(slot)) =
        storage.resolve_relation(alter.table.schema, alter.table.name, txid)
    else {
        return None;
    };
    let definition = storage.table_def(slot, txid);
    alter
        .actions
        .iter()
        .any(|action| {
            let ast::AlterAction::AlterColumnType {
                column,
                type_name,
                type_mod,
                using,
                ..
            } = action
            else {
                return false;
            };
            let Some(column) = definition.column_index(column) else {
                return false;
            };
            let Some(target) = types::ColType::from_sql_name(type_name) else {
                return false;
            };
            let source = definition.columns()[column];
            alter_column_requires_rewrite(
                source.ctype,
                source.type_mod,
                source.name.as_str(),
                target,
                *type_mod,
                *using,
            )
        })
        .then_some((slot, 4))
}

fn alter_column_requires_rewrite(
    source: types::ColType,
    source_type_mod: i32,
    column: &str,
    target: types::ColType,
    target_type_mod: i32,
    using: Option<&Expr<'_>>,
) -> bool {
    let (expression_type, expression_type_mod) = match using {
        Some(expression) => {
            match relabelled_column_type(expression, column, source, source_type_mod) {
                Some(result) => result,
                None => return true,
            }
        }
        None => (source, source_type_mod),
    };
    type_change_requires_rewrite(expression_type, target)
        || typmod_change_requires_rewrite(target, expression_type_mod, target_type_mod)
}

fn relabelled_column_type(
    expression: &Expr<'_>,
    column: &str,
    source: types::ColType,
    source_type_mod: i32,
) -> Option<(types::ColType, i32)> {
    match expression {
        Expr::Column { name, .. } if name.eq_ignore_ascii_case(column) => {
            Some((source, source_type_mod))
        }
        Expr::Cast {
            operand,
            type_name,
            type_mod,
        } => {
            let (from, from_type_mod) =
                relabelled_column_type(operand, column, source, source_type_mod)?;
            let to = types::ColType::from_sql_name(type_name)?;
            (!type_change_requires_rewrite(from, to)
                && !typmod_change_requires_rewrite(to, from_type_mod, *type_mod))
            .then_some((to, *type_mod))
        }
        Expr::Collate { operand, .. } => {
            relabelled_column_type(operand, column, source, source_type_mod)
        }
        _ => None,
    }
}

fn type_change_requires_rewrite(source: types::ColType, target: types::ColType) -> bool {
    use types::ColType::*;
    if source == target {
        return false;
    }
    let oid_reference = |ctype| {
        matches!(
            ctype,
            Oid | Regtype
                | Regproc
                | Regprocedure
                | Regoper
                | Regoperator
                | Regclass
                | Regnamespace
                | Regrole
        )
    };
    !matches!(
        (source, target),
        (Text, Varchar)
            | (Varchar, Text)
            | (Cidr, Inet)
            | (Regproc, Regprocedure)
            | (Regprocedure, Regproc)
            | (Regoper, Regoperator)
            | (Regoperator, Regoper)
    ) && !(source == Int4 && oid_reference(target))
        && !(target == Int4 && oid_reference(source))
        && !(source == Oid && oid_reference(target))
        && !(target == Oid && oid_reference(source))
}

fn typmod_change_requires_rewrite(ctype: types::ColType, old: i32, new: i32) -> bool {
    use types::TypeMod;
    let (old, new) = (TypeMod::decode(ctype, old), TypeMod::decode(ctype, new));
    match (old, new) {
        (old, new) if old == new => false,
        (_, TypeMod::None) => false,
        (TypeMod::None, _) => true,
        (TypeMod::Length(old), TypeMod::Length(new)) => new < old,
        (
            TypeMod::NumericPS {
                precision: old_precision,
                scale: old_scale,
            },
            TypeMod::NumericPS {
                precision: new_precision,
                scale: new_scale,
            },
        ) => new_scale != old_scale || new_precision < old_precision,
        (TypeMod::TemporalPrecision(old), TypeMod::TemporalPrecision(new)) => new < old,
        _ => true,
    }
}

pub(crate) fn event_trigger_tag_supported(tag: &str) -> bool {
    [
        "ALTER AGGREGATE",
        "ALTER COLLATION",
        "ALTER CONVERSION",
        "ALTER DEFAULT PRIVILEGES",
        "ALTER DOMAIN",
        "ALTER EXTENSION",
        "ALTER FUNCTION",
        "ALTER INDEX",
        "ALTER LANGUAGE",
        "ALTER OPERATOR",
        "ALTER OPERATOR CLASS",
        "ALTER OPERATOR FAMILY",
        "ALTER MATERIALIZED VIEW",
        "ALTER SCHEMA",
        "ALTER VIEW",
        "ALTER POLICY",
        "ALTER PROCEDURE",
        "ALTER PUBLICATION",
        "ALTER RULE",
        "ALTER ROUTINE",
        "ALTER SEQUENCE",
        "ALTER STATISTICS",
        "ALTER SUBSCRIPTION",
        "ALTER TABLE",
        "ALTER TRIGGER",
        "ALTER TYPE",
        "COMMENT",
        "CREATE AGGREGATE",
        "CREATE ACCESS METHOD",
        "CREATE CAST",
        "CREATE COLLATION",
        "CREATE CONVERSION",
        "CREATE DOMAIN",
        "CREATE EXTENSION",
        "CREATE FUNCTION",
        "CREATE INDEX",
        "CREATE LANGUAGE",
        "CREATE MATERIALIZED VIEW",
        "CREATE OPERATOR",
        "CREATE OPERATOR CLASS",
        "CREATE OPERATOR FAMILY",
        "CREATE POLICY",
        "CREATE PROCEDURE",
        "CREATE PUBLICATION",
        "CREATE RULE",
        "CREATE SCHEMA",
        "CREATE SEQUENCE",
        "CREATE STATISTICS",
        "CREATE SUBSCRIPTION",
        "CREATE TABLE",
        "CREATE TABLE AS",
        "CREATE TRIGGER",
        "CREATE TYPE",
        "CREATE VIEW",
        "DROP AGGREGATE",
        "DROP ACCESS METHOD",
        "DROP CAST",
        "DROP COLLATION",
        "DROP CONVERSION",
        "DROP DOMAIN",
        "DROP EXTENSION",
        "DROP FUNCTION",
        "DROP INDEX",
        "DROP LANGUAGE",
        "DROP MATERIALIZED VIEW",
        "DROP OWNED",
        "DROP OPERATOR",
        "DROP OPERATOR CLASS",
        "DROP OPERATOR FAMILY",
        "DROP POLICY",
        "DROP PROCEDURE",
        "DROP PUBLICATION",
        "DROP RULE",
        "DROP ROUTINE",
        "DROP SCHEMA",
        "DROP SEQUENCE",
        "DROP STATISTICS",
        "DROP SUBSCRIPTION",
        "DROP TABLE",
        "DROP TRIGGER",
        "DROP TYPE",
        "DROP VIEW",
        "GRANT",
        "REFRESH MATERIALIZED VIEW",
        "REINDEX",
        "REVOKE",
        "SELECT INTO",
    ]
    .iter()
    .any(|known| known.eq_ignore_ascii_case(tag))
}

fn top_level_only_command(statement: &Stmt<'_>) -> Option<&'static str> {
    match statement {
        Stmt::Vacuum { .. } => Some("VACUUM"),
        // A relation-specific CLUSTER is transactional; the all-relations
        // form controls its own work across relations and is not.
        Stmt::Cluster {
            target: ast::ClusterTarget::All,
            ..
        } => Some("CLUSTER"),
        Stmt::AlterSystem { .. } => Some("ALTER SYSTEM"),
        Stmt::Discard(ast::DiscardTarget::All) => Some("DISCARD ALL"),
        Stmt::CommitPrepared(_) => Some("COMMIT PREPARED"),
        Stmt::RollbackPrepared(_) => Some("ROLLBACK PREPARED"),
        Stmt::CreateDatabase { .. } => Some("CREATE DATABASE"),
        Stmt::DropDatabase { .. } => Some("DROP DATABASE"),
        Stmt::CreateTablespace { .. } => Some("CREATE TABLESPACE"),
        Stmt::DropTablespace { .. } => Some("DROP TABLESPACE"),
        Stmt::AlterDatabase {
            action: ast::AlterDatabaseAction::SetTablespace(_),
            ..
        } => Some("ALTER DATABASE SET TABLESPACE"),
        Stmt::CreateIndex {
            build: ast::IndexBuildMode::Concurrent,
            ..
        } => Some("CREATE INDEX CONCURRENTLY"),
        Stmt::DropIndex {
            build: ast::IndexBuildMode::Concurrent,
            ..
        } => Some("DROP INDEX CONCURRENTLY"),
        Stmt::Reindex {
            options:
                ast::ReindexOptions {
                    build: ast::IndexBuildMode::Concurrent,
                    ..
                },
            ..
        } => Some("REINDEX CONCURRENTLY"),
        Stmt::AlterSubscription {
            action: ast::AlterSubscriptionAction::SetOptions(patch),
            ..
        } if patch.failover.is_some() || patch.two_phase.is_some() => {
            Some("ALTER SUBSCRIPTION SET")
        }
        _ => None,
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

#[derive(Clone, Copy)]
struct LogicalReplicaIdentity {
    wire: pgoutput::ReplicaIdentity,
    key_mask: u64,
}

impl LogicalReplicaIdentity {
    fn usable_for_change(self) -> bool {
        self.key_mask != 0
    }
}

fn logical_replica_identity(
    storage: &Storage,
    table_slot: usize,
) -> Result<LogicalReplicaIdentity, SqlError> {
    let definition = storage.table_def(table_slot, 0);
    let all_columns = if definition.n_columns == crate::storage::MAX_COLUMNS {
        u64::MAX
    } else {
        (1u64 << definition.n_columns) - 1
    };
    match definition.replica_identity {
        crate::storage::ReplicaIdentityMode::Nothing => Ok(LogicalReplicaIdentity {
            wire: pgoutput::ReplicaIdentity::Nothing,
            key_mask: 0,
        }),
        crate::storage::ReplicaIdentityMode::Full => Ok(LogicalReplicaIdentity {
            wire: pgoutput::ReplicaIdentity::Full,
            key_mask: all_columns,
        }),
        crate::storage::ReplicaIdentityMode::Default => {
            let key_mask = definition
                .columns()
                .iter()
                .enumerate()
                .fold(0u64, |mask, (column, definition)| {
                    mask | (u64::from(definition.primary) << column)
                });
            Ok(LogicalReplicaIdentity {
                wire: pgoutput::ReplicaIdentity::Default,
                key_mask,
            })
        }
        crate::storage::ReplicaIdentityMode::Index => {
            let mut selected = None;
            for slot in 0..storage.index_count() {
                let Some(index) = storage.index_visible_to(slot, 0) else {
                    continue;
                };
                if storage.index_table_slot(slot) != Some(table_slot)
                    || !index.mutable_for(0).replica_identity
                {
                    continue;
                }
                if selected.replace(index).is_some() {
                    return Err(sql_err!(
                        sqlstate::INTERNAL_ERROR,
                        "relation has more than one replica identity index"
                    ));
                }
            }
            let key_mask = selected.map_or(0, |index| {
                index.columns[..index.n_cols]
                    .iter()
                    .fold(0u64, |mask, column| mask | (1u64 << column))
            });
            Ok(LogicalReplicaIdentity {
                wire: pgoutput::ReplicaIdentity::Index,
                key_mask,
            })
        }
    }
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
    table_slot: usize,
    definition: &crate::storage::TableDef,
    relation_id: u32,
    column_mask: u64,
    responder: &mut Responder,
    end_lsn: u64,
) -> Result<(), SqlError> {
    let mut selected_columns = [crate::storage::ColumnMeta::EMPTY; crate::storage::MAX_COLUMNS];
    let mut selected_key_columns = [false; crate::storage::MAX_COLUMNS];
    let mut selected_count = 0usize;
    let replica_identity = logical_replica_identity(storage, table_slot)?;
    for (index, column) in definition.columns().iter().enumerate() {
        if column_mask & (1u64 << index) != 0 {
            selected_columns[selected_count] = *column;
            selected_key_columns[selected_count] = replica_identity.key_mask & (1u64 << index) != 0;
            selected_count += 1;
        }
    }
    let columns = &selected_columns[..selected_count];
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
                    pgoutput::Relation {
                        relation_id,
                        schema: definition.schema.as_str(),
                        name: definition.name.as_str(),
                        columns,
                        type_oids: &type_oids[..columns.len()],
                        replica_identity: replica_identity.wire,
                        key_columns: &selected_key_columns[..columns.len()],
                    },
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
            let output_slot = publication_output_relation(
                storage,
                publication_names,
                table_slot,
                PublicationOperation::Truncate,
            )?;
            let definition = storage.table_def(output_slot, 0);
            let relation_id = output_slot as u32 + 1;
            if relation_ids[..relation_count].contains(&relation_id) {
                continue;
            }
            emit_replication_relation(
                storage,
                output_slot,
                definition,
                relation_id,
                u64::MAX,
                responder,
                end_lsn,
            )?;
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

/// Finds the explicit publication member that makes `table_slot` publishable.
/// PostgreSQL makes every partition an implicit member when an ancestor is
/// published; the leaf itself wins when both appear explicitly.
pub(crate) fn publication_partition_member(
    storage: &Storage,
    publication: &crate::storage::PublicationDef,
    table_slot: usize,
) -> Option<usize> {
    let mut current = table_slot;
    loop {
        if let Some(index) = publication.tables[..publication.table_count]
            .iter()
            .position(|member| usize::from(*member) == current)
        {
            return Some(index);
        }
        let crate::storage::PartitionAttachment { parent, .. } =
            storage.table_def(current, 0).partition.attachment?;
        current = usize::from(parent);
    }
}

/// Schema publications inherit through a partition tree too: a parent in the
/// selected schema publishes a leaf even when that leaf lives in another one.
pub(crate) fn publication_partition_schema_member(
    storage: &Storage,
    publication: &crate::storage::PublicationDef,
    table_slot: usize,
) -> bool {
    let mut current = table_slot;
    loop {
        let schema_selected = storage
            .find_schema(storage.table_def(current, 0).schema.as_str())
            .is_some_and(|slot| {
                publication.schemas[..publication.schema_count].contains(&(slot as u8))
            });
        if schema_selected {
            return true;
        }
        let Some(crate::storage::PartitionAttachment { parent, .. }) =
            storage.table_def(current, 0).partition.attachment
        else {
            return false;
        };
        current = usize::from(parent);
    }
}

fn publication_selects(
    storage: &Storage,
    publication_names: &[SqlName],
    table_slot: usize,
    operation: PublicationOperation,
) -> Result<bool, SqlError> {
    Ok(publication_column_mask(storage, publication_names, table_slot, operation)?.is_some())
}

pub(crate) fn partition_root(storage: &Storage, table_slot: usize) -> usize {
    let mut current = table_slot;
    while let Some(attachment) = storage.table_def(current, 0).partition.attachment {
        current = usize::from(attachment.parent);
    }
    current
}

fn publication_output_relation(
    storage: &Storage,
    publication_names: &[SqlName],
    table_slot: usize,
    operation: PublicationOperation,
) -> Result<usize, SqlError> {
    for name in publication_names {
        let publication = storage.publication(name.as_str()).ok_or_else(|| {
            sql_err!(
                sqlstate::UNDEFINED_OBJECT,
                "publication \"{}\" does not exist",
                name.as_str()
            )
        })?;
        let publishes = match operation {
            PublicationOperation::Insert => publication.publish_insert,
            PublicationOperation::Update => publication.publish_update,
            PublicationOperation::Delete => publication.publish_delete,
            PublicationOperation::Truncate => publication.publish_truncate,
        };
        if publishes
            && publication.publish_via_partition_root
            && (publication.all_tables
                || publication_partition_schema_member(storage, publication, table_slot)
                || publication_partition_member(storage, publication, table_slot).is_some())
        {
            return Ok(partition_root(storage, table_slot));
        }
    }
    Ok(table_slot)
}

fn publication_projection_mask(
    storage: &Storage,
    publication: &crate::storage::PublicationDef,
    table_slot: usize,
) -> Option<u64> {
    let implicit_mask = || {
        storage
            .table_def(table_slot, 0)
            .columns()
            .iter()
            .enumerate()
            .filter(|(_, column)| {
                !column.default.is_generated()
                    || publication.publish_generated_columns
                        == crate::storage::PublishGeneratedColumns::Stored
            })
            .fold(0u64, |mask, (column, _)| mask | (1u64 << column))
    };
    if publication.all_tables
        || publication_partition_schema_member(storage, publication, table_slot)
    {
        return Some(implicit_mask());
    }
    let index = publication_partition_member(storage, publication, table_slot)?;
    if usize::from(publication.tables[index]) != table_slot
        && !publication.publish_via_partition_root
    {
        return Some(implicit_mask());
    }
    let mask = publication.table_column_masks[index];
    Some(if mask == 0 { implicit_mask() } else { mask })
}

fn mismatched_publication_columns(storage: &Storage, table_slot: usize) -> SqlError {
    let definition = storage.table_def(table_slot, 0);
    sql_err!(
        sqlstate::FEATURE_NOT_SUPPORTED,
        "cannot use different column lists for table \"{}.{}\" in different publications",
        definition.schema.as_str(),
        definition.name.as_str()
    )
}

/// Computes the sole pgoutput projection selected by matching publications.
/// PostgreSQL rejects a stream that assigns different column lists to the
/// same relation rather than combining them.
fn publication_column_mask(
    storage: &Storage,
    publication_names: &[SqlName],
    table_slot: usize,
    operation: PublicationOperation,
) -> Result<Option<u64>, SqlError> {
    if matches!(
        operation,
        PublicationOperation::Update | PublicationOperation::Delete
    ) && !logical_replica_identity(storage, table_slot)?.usable_for_change()
    {
        return Ok(None);
    }
    let mut selected = None;
    for name in publication_names {
        let publication = storage.publication(name.as_str()).ok_or_else(|| {
            sql_err!(
                sqlstate::UNDEFINED_OBJECT,
                "publication \"{}\" does not exist",
                name.as_str()
            )
        })?;
        let publishes = match operation {
            PublicationOperation::Insert => publication.publish_insert,
            PublicationOperation::Update => publication.publish_update,
            PublicationOperation::Delete => publication.publish_delete,
            PublicationOperation::Truncate => publication.publish_truncate,
        };
        if !publishes {
            continue;
        }
        if let Some(mask) = publication_projection_mask(storage, publication, table_slot) {
            if selected.is_some_and(|selected| selected != mask) {
                return Err(mismatched_publication_columns(storage, table_slot));
            }
            selected = Some(mask);
        }
    }
    Ok(selected)
}

/// True when the subscribed publication union selects this row.  A missing
/// filter admits every row; otherwise PostgreSQL combines the filters with OR
/// and treats NULL like false.
#[inline(never)]
fn publication_row_matches(
    storage: &Storage,
    publication_names: &[SqlName],
    table_slot: usize,
    operation: PublicationOperation,
    values: &[Datum],
    arena: &Arena,
) -> Result<bool, SqlError> {
    let definition = storage.table_def(table_slot, 0);
    for name in publication_names {
        let publication = storage.publication(name.as_str()).ok_or_else(|| {
            sql_err!(
                sqlstate::UNDEFINED_OBJECT,
                "publication \"{}\" does not exist",
                name.as_str()
            )
        })?;
        let publishes = match operation {
            PublicationOperation::Insert => publication.publish_insert,
            PublicationOperation::Update => publication.publish_update,
            PublicationOperation::Delete => publication.publish_delete,
            PublicationOperation::Truncate => publication.publish_truncate,
        };
        if !publishes {
            continue;
        }
        if publication.all_tables
            || publication_partition_schema_member(storage, publication, table_slot)
        {
            return Ok(true);
        }
        let Some(index) = publication_partition_member(storage, publication, table_slot) else {
            continue;
        };
        // With PostgreSQL's default leaf identity, an ancestor's row filter
        // does not become the leaf's filter.  Only an explicitly named leaf
        // has a filter in this representation.
        if usize::from(publication.tables[index]) != table_slot
            && !publication.publish_via_partition_root
        {
            return Ok(true);
        }
        let filter = publication.table_filters.get(index);
        if filter.is_empty() {
            return Ok(true);
        }
        let mark = arena.mark();
        let result = (|| {
            let expression = parser::parse_expr(filter, arena)?;
            let row = exec::RowCtx {
                def: definition,
                values,
                alias: None,
            };
            Ok(matches!(
                eval(expression, arena, NO_PARAMS, &row)?,
                Datum::Bool(true)
            ))
        })();
        // The parsed AST and every scalar temporary are consumed above.  A
        // filter never retains execution memory across rows or transactions.
        unsafe { arena.rewind_to(mark) };
        if result? {
            return Ok(true);
        }
    }
    Ok(false)
}

fn project_replication_values<'a>(
    values: &[Datum<'a>],
    column_mask: u64,
) -> ([Datum<'a>; crate::storage::MAX_COLUMNS], usize) {
    let mut projected = [Datum::Null; crate::storage::MAX_COLUMNS];
    let mut count = 0usize;
    for (index, value) in values.iter().enumerate() {
        if column_mask & (1u64 << index) != 0 {
            projected[count] = *value;
            count += 1;
        }
    }
    (projected, count)
}

impl Engine {
    pub(crate) fn subscription_cleanup_runtime(
        &self,
        slot: usize,
    ) -> Option<SubscriptionCleanupRuntime> {
        let (created_at, name, connection, remote_slot) =
            self.storage.subscription_cleanup(slot)?;
        Some(SubscriptionCleanupRuntime {
            created_at,
            name,
            endpoint: connection.endpoint()?.for_subscription(name),
            slot: remote_slot,
        })
    }

    pub(crate) fn complete_subscription_cleanup(
        &mut self,
        slot: usize,
        created_at: u64,
        name: SqlName,
    ) -> Result<(), SqlError> {
        self.next_txid = self.next_txid.wrapping_add(1).max(1);
        let transaction_id = self.next_txid;
        let lsn =
            self.storage.lsn().checked_add(1).ok_or_else(|| {
                sql_err!(sqlstate::PROGRAM_LIMIT_EXCEEDED, "WAL LSN space exhausted")
            })?;
        if let Err(error) = self.wal.stage(
            transaction_id,
            lsn,
            &WalOp::CompleteSubscriptionCleanup {
                name: name.as_str(),
                created_at,
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
        self.storage
            .complete_subscription_cleanup(slot, created_at)?;
        self.storage.set_lsn(commit_lsn);
        Ok(())
    }

    pub(crate) fn fail_subscription(
        &mut self,
        stream: crate::storage::SubscriptionStream,
        failure: crate::storage::SubscriptionFailure,
    ) -> Result<(), SqlError> {
        self.next_txid = self.next_txid.wrapping_add(1).max(1);
        let transaction_id = self.next_txid;
        let lsn =
            self.storage.lsn().checked_add(1).ok_or_else(|| {
                sql_err!(sqlstate::PROGRAM_LIMIT_EXCEEDED, "WAL LSN space exhausted")
            })?;
        if let Err(error) = self.wal.stage(
            transaction_id,
            lsn,
            &WalOp::FailSubscription {
                name: stream.name().as_str(),
                created_at: stream.created_at(),
                definition_generation: stream.definition_generation(),
                sqlstate: failure.sqlstate.as_str(),
                message: failure.message.as_str(),
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
        self.storage.fail_subscription(stream, failure)?;
        self.storage.set_lsn(commit_lsn);
        Ok(())
    }

    pub(crate) fn subscription_runtime(&self, slot: usize) -> Option<SubscriptionRuntime> {
        self.storage
            .subscriptions_with_slots_visible_to(0)
            .find(|(index, _subscription)| *index == slot)
            .filter(|(_, subscription)| {
                (subscription.failure.is_none()
                    || (!subscription.behavior.disable_on_error && subscription.enabled_to(0)))
                    && (subscription.enabled_to(0)
                        || !matches!(
                            subscription.bootstrap,
                            crate::storage::SubscriptionBootstrap::Deferred
                                | crate::storage::SubscriptionBootstrap::Ready
                        ))
            })
            .and_then(|(_, subscription)| {
                subscription.connection.endpoint().and_then(|endpoint| {
                    let publisher_slot = subscription
                        .slot
                        .name()
                        .map(crate::storage::ReplicationSlotName::sql_name);
                    let bootstrap_slot = match subscription.bootstrap {
                        crate::storage::SubscriptionBootstrap::CopyExternalSlot
                        | crate::storage::SubscriptionBootstrap::CopyWithoutSlot
                        | crate::storage::SubscriptionBootstrap::Refresh { .. } => {
                            let generated =
                                stack_format!(63, "pos3ql_{:x}_sync", subscription.created_at);
                            Some(
                                crate::storage::ReplicationSlotName::parse(generated.as_str())
                                    .ok()?
                                    .sql_name(),
                            )
                        }
                        _ => publisher_slot,
                    };
                    self.storage
                        .subscription_stream(slot, 0)
                        .map(|stream| SubscriptionRuntime {
                            stream,
                            endpoint: endpoint.for_subscription(subscription.name),
                            publications: subscription.publications,
                            publication_count: subscription.publication_count,
                            slot: publisher_slot,
                            manage_slot_behavior: matches!(
                                subscription.slot,
                                crate::storage::SubscriptionSlot::Managed(_)
                            ),
                            bootstrap_slot,
                            drop_bootstrap_slot: matches!(
                                subscription.bootstrap,
                                crate::storage::SubscriptionBootstrap::CopyExternalSlot
                                    | crate::storage::SubscriptionBootstrap::CopyWithoutSlot
                                    | crate::storage::SubscriptionBootstrap::Refresh { .. }
                            ),
                            confirmed_lsn: subscription.confirmed_lsn,
                            bootstrap: subscription.bootstrap,
                            enabled: subscription.enabled_to(0),
                            behavior: subscription.behavior,
                        })
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

    pub(crate) fn subscription_relation_is_ready(
        &self,
        stream: crate::storage::SubscriptionStream,
        schema: &str,
        table: &str,
    ) -> bool {
        self.storage
            .subscription_relation_is_ready(stream, schema, table)
    }

    #[cfg(test)]
    pub(crate) fn subscription_stream(
        &self,
        name: &str,
    ) -> Option<crate::storage::SubscriptionStream> {
        self.storage
            .subscription(name, 0)
            .and_then(|(slot, _)| self.storage.subscription_stream(slot, 0))
    }

    /// Opens the local transaction that will receive one publisher commit.
    /// The worker uses the ordinary engine transaction and durability path;
    /// replication cannot create a second, weaker write path.
    pub fn begin_subscription_apply(&mut self, txn: &mut TxnState, guc: &GucState) {
        self.ensure_txn(txn, TxnMode::Implicit, guc);
        txn.replication_apply = true;
        txn.begin_command();
        // pgoutput messages form one remote transaction, not independent SQL
        // statements.  Each later row operation must therefore see every
        // earlier local change from that same remote commit.
        self.storage.set_read_snapshot(crate::storage::SNAPSHOT_ALL);
    }

    pub(crate) fn begin_subscription_relation_refresh(
        &mut self,
        txn: &mut TxnState,
        stream: crate::storage::SubscriptionStream,
    ) -> Result<(), SqlError> {
        if !txn.is_active() || !txn.replication_apply {
            return Err(sql_err!(
                sqlstate::NO_ACTIVE_SQL_TRANSACTION,
                "subscription relation refresh requires an apply transaction"
            ));
        }
        self.storage
            .begin_subscription_relation_refresh(stream, txn.txid)?;
        let lsn = self.storage.bump_lsn();
        if let Err(error) = self.wal.stage(
            txn.txid,
            lsn,
            &WalOp::ResetSubscriptionRelations {
                name: stream.name().as_str(),
                created_at: stream.created_at(),
                definition_generation: stream.definition_generation(),
            },
        ) {
            self.storage
                .rollback_subscription_relation_refresh(txn.txid);
            return Err(error);
        }
        if let Err(error) = txn.record_ddl(DdlUndo::SubscriptionRelationsChanged) {
            self.storage
                .rollback_subscription_relation_refresh(txn.txid);
            return Err(error);
        }
        Ok(())
    }

    pub(crate) fn stage_subscription_relation(
        &mut self,
        txn: &mut TxnState,
        stream: crate::storage::SubscriptionStream,
        schema: &str,
        table: &str,
    ) -> Result<(), SqlError> {
        if !txn.is_active() || !txn.replication_apply {
            return Err(sql_err!(
                sqlstate::NO_ACTIVE_SQL_TRANSACTION,
                "subscription relation registration requires an apply transaction"
            ));
        }
        self.storage
            .stage_subscription_relation(stream, schema, table, txn.txid)?;
        let lsn = self.storage.bump_lsn();
        self.wal.stage(
            txn.txid,
            lsn,
            &WalOp::AddSubscriptionRelation {
                name: stream.name().as_str(),
                created_at: stream.created_at(),
                definition_generation: stream.definition_generation(),
                schema,
                table,
            },
        )
    }

    /// Couples a publisher commit position to the active local transaction.
    /// `false` means the position was already committed locally, so a replayed
    /// remote transaction must be skipped before it can mutate rows.
    pub(crate) fn stage_subscription_advance(
        &mut self,
        txn: &mut TxnState,
        stream: crate::storage::SubscriptionStream,
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
            .subscription_advance(stream, confirmed_lsn, txn.txid)?
        else {
            return Ok(false);
        };
        txn.record_subscription_advance(advance)?;
        Ok(true)
    }

    pub(crate) fn stage_subscription_skip(
        &mut self,
        txn: &mut TxnState,
        stream: crate::storage::SubscriptionStream,
        final_lsn: u64,
        confirmed_lsn: u64,
    ) -> Result<(), SqlError> {
        let current = self
            .storage
            .subscription_stream(stream.slot(), txn.txid)
            .filter(|current| {
                current.created_at() == stream.created_at()
                    && current.definition_generation() == stream.definition_generation()
            })
            .ok_or_else(|| {
                sql_err!(
                    sqlstate::OBJECT_NOT_IN_PREREQUISITE_STATE,
                    "subscription SKIP targets a replaced stream definition"
                )
            })?;
        let mut definition = self
            .storage
            .subscription_definition_to(current.slot(), txn.txid);
        if definition.behavior.skip_lsn != Some(final_lsn) {
            return Err(sql_err!(
                sqlstate::OBJECT_NOT_IN_PREREQUISITE_STATE,
                "subscription SKIP does not match the remote transaction finish LSN"
            ));
        }
        definition.behavior.skip_lsn = None;
        let change =
            self.storage
                .set_subscription_definition(current.slot(), definition, txn.txid)?;
        let lsn = self.storage.bump_lsn();
        if let Err(error) = self.wal.stage(
            txn.txid,
            lsn,
            &WalOp::AlterSubscription {
                name: current.name().as_str(),
                connection: definition.connection.as_str(),
                publications: definition.publication_array(),
                publication_count: definition.publication_count(),
                slot: definition.slot,
                behavior: definition.behavior,
            },
        ) {
            self.storage
                .restore_subscription_definition(current.slot(), change.prior);
            return Err(error);
        }
        if let Err(error) = txn.record_ddl(DdlUndo::SubscriptionDefinitionChanged {
            slot: current.slot() as u32,
            prior: change.prior,
        }) {
            self.storage
                .restore_subscription_definition(current.slot(), change.prior);
            return Err(error);
        }
        if !self.stage_subscription_advance(txn, current, confirmed_lsn)? {
            return Err(sql_err!(
                sqlstate::OBJECT_NOT_IN_PREREQUISITE_STATE,
                "subscription SKIP did not advance its durable frontier"
            ));
        }
        Ok(())
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

    pub(crate) fn apply_subscription_insert(
        &mut self,
        txn: &mut TxnState,
        stream: crate::storage::SubscriptionStream,
        binding: crate::pg::subscription_apply::RelationBinding,
        tuple: crate::pg::pginput::Tuple<'_>,
        arena: &Arena,
        trigger_context: &mut exec::ReplicationTriggerContext<'_, '_>,
    ) -> Result<(), SqlError> {
        let role = self.subscription_apply_role(stream, binding.table_slot(), txn.txid)?;
        let prior = eval::funcs::system::current_user_owned();
        eval::funcs::system::set_current_user(role.as_str());
        let result = exec::apply_replication_insert(
            &mut self.storage,
            txn,
            binding,
            tuple,
            arena,
            trigger_context,
        );
        eval::funcs::system::set_current_user(prior.as_str());
        result
    }

    pub(crate) fn apply_subscription_delete(
        &mut self,
        txn: &mut TxnState,
        stream: crate::storage::SubscriptionStream,
        binding: crate::pg::subscription_apply::RelationBinding,
        old: crate::pg::pginput::OldTuple<'_>,
        arena: &Arena,
        trigger_context: &mut exec::ReplicationTriggerContext<'_, '_>,
    ) -> Result<(), SqlError> {
        let role = self.subscription_apply_role(stream, binding.table_slot(), txn.txid)?;
        let prior = eval::funcs::system::current_user_owned();
        eval::funcs::system::set_current_user(role.as_str());
        let result = exec::apply_replication_delete(
            &mut self.storage,
            txn,
            binding,
            old,
            arena,
            trigger_context,
        );
        eval::funcs::system::set_current_user(prior.as_str());
        result
    }

    pub(crate) fn apply_subscription_update(
        &mut self,
        txn: &mut TxnState,
        stream: crate::storage::SubscriptionStream,
        binding: crate::pg::subscription_apply::RelationBinding,
        update: exec::ReplicationUpdate<'_>,
        arena: &Arena,
        trigger_context: &mut exec::ReplicationTriggerContext<'_, '_>,
    ) -> Result<(), SqlError> {
        let role = self.subscription_apply_role(stream, binding.table_slot(), txn.txid)?;
        let prior = eval::funcs::system::current_user_owned();
        eval::funcs::system::set_current_user(role.as_str());
        let result = exec::apply_replication_update(
            &mut self.storage,
            txn,
            binding,
            update,
            arena,
            trigger_context,
        );
        eval::funcs::system::set_current_user(prior.as_str());
        result
    }

    pub(crate) fn begin_subscription_subtransaction(
        &mut self,
        txn: &mut TxnState,
        guc: &GucState,
        xid: u32,
    ) -> Result<(), SqlError> {
        let name = crate::stack_format!(63, "pgoutput_{xid}");
        if txn.savepoint_index(name.as_str()).is_some() {
            return Ok(());
        }
        txn.savepoint(
            name.as_str(),
            self.wal.stage_mark(txn.txid),
            self.storage.lock_mark(),
        )?;
        guc.savepoint();
        Ok(())
    }

    pub(crate) fn rollback_subscription_subtransaction(
        &mut self,
        txn: &mut TxnState,
        guc: &GucState,
        xid: u32,
    ) -> bool {
        let name = crate::stack_format!(63, "pgoutput_{xid}");
        let Some(index) = txn.savepoint_index(name.as_str()) else {
            return false;
        };
        self.rollback_to_savepoint(txn, index, guc);
        txn.release_savepoints_from(index);
        guc.release_savepoints_from(index);
        true
    }

    pub(crate) fn apply_subscription_truncate(
        &mut self,
        txn: &mut TxnState,
        stream: crate::storage::SubscriptionStream,
        tables: &[usize],
        cascade: bool,
        restart_identity: bool,
    ) -> Result<(), SqlError> {
        if tables.is_empty() {
            return Err(sql_err!(
                sqlstate::PROTOCOL_VIOLATION,
                "subscription TRUNCATE has no relations"
            ));
        }
        if tables.len() > crate::sql::txn::MAX_TRUNCATE_TABLES {
            return Err(sql_err!(
                sqlstate::PROGRAM_LIMIT_EXCEEDED,
                "subscription TRUNCATE exceeds its fixed table capacity"
            ));
        }
        for &table in tables {
            self.subscription_apply_role(stream, table, txn.txid)?;
        }
        exec::apply_replication_truncate(&mut self.storage, txn, tables, cascade, restart_identity)
    }

    fn subscription_apply_role(
        &self,
        stream: crate::storage::SubscriptionStream,
        table_slot: usize,
        txid: u32,
    ) -> Result<SqlName, SqlError> {
        let subscription = self
            .storage
            .subscriptions_with_slots_visible_to(txid)
            .find(|(slot, subscription)| {
                *slot == stream.slot()
                    && subscription.created_at == stream.created_at()
                    && subscription.definition_generation == stream.definition_generation()
            })
            .map(|(_, subscription)| subscription)
            .ok_or_else(|| {
                sql_err!(
                    sqlstate::OBJECT_NOT_IN_PREREQUISITE_STATE,
                    "subscription apply authority targets a replaced stream"
                )
            })?;
        let owner = if subscription.behavior.run_as_owner {
            subscription.ownership.owner_to(txid) as usize
        } else {
            self.storage
                .object_owner(self.storage.table_access_object(table_slot, txid), txid)
        };
        Ok(self.storage.role(owner).name_to(txid))
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
        let valid = attributes.valid_until.as_ref().is_none_or(|valid_until| {
            valid_until.as_str().eq_ignore_ascii_case("infinity")
                || crate::sql::datetime::parse_timestamp(valid_until.as_str(), true)
                    .is_ok_and(|deadline| deadline >= crate::sql::datetime::now_micros())
        });
        Some(RoleLogin {
            slot: slot as u16,
            can_login: attributes.can_login,
            valid,
            superuser: attributes.superuser,
            replication: attributes.replication,
            connection_limit: attributes.connection_limit,
            password: attributes.password,
        })
    }

    pub(crate) fn database_login(&self, name: &str) -> Option<DatabaseLogin> {
        let slot = self.storage.database_slot(name, 0)?;
        let database = self.storage.database(slot);
        let definition = database.definition_for(0);
        Some(DatabaseLogin {
            slot: slot as u16,
            oid: database.oid,
            allow_connections: definition.allow_connections,
            connection_limit: definition.connection_limit,
        })
    }

    pub(crate) fn select_database(
        &mut self,
        database: crate::storage::DatabaseOid,
    ) -> Result<(), SqlError> {
        self.storage.select_database(database)?;
        self.wal.select_database(database);
        Ok(())
    }

    pub(crate) fn apply_role_settings(&self, role: u16, guc: &GucState) -> Result<(), SqlError> {
        use crate::storage::RoleSettingScope;
        let database = self.storage.current_database_oid();
        for scope in [
            RoleSettingScope::AllRolesInDatabase(database),
            RoleSettingScope::RoleAllDatabases(role),
            RoleSettingScope::RoleInDatabase { role, database },
        ] {
            let source = match scope {
                RoleSettingScope::AllRolesInDatabase(_) => guc::ConnectionDefaultSource::Database,
                RoleSettingScope::RoleAllDatabases(_) => guc::ConnectionDefaultSource::Role,
                RoleSettingScope::RoleInDatabase { .. } => {
                    guc::ConnectionDefaultSource::DatabaseRole
                }
            };
            for (_, setting) in self.storage.role_settings() {
                if setting.live && setting.scope == scope {
                    guc.set_connection_default(
                        setting.name.as_str(),
                        setting.value.as_str(),
                        source,
                    )?;
                }
            }
        }
        Ok(())
    }

    pub(crate) fn apply_system_settings(&self, guc: &GucState) -> Result<(), SqlError> {
        guc.reset_cluster_defaults();
        for setting in self.active_system_settings.iter().flatten() {
            guc.set_cluster_default(setting.name.as_str(), setting.value.as_str())?;
        }
        Ok(())
    }

    fn reload_system_settings(&mut self) {
        self.active_system_settings = [None; crate::storage::MAX_SYSTEM_SETTINGS];
        let mut count = 0usize;
        for (_, setting) in self.storage.system_settings() {
            if setting.live {
                self.active_system_settings[count] = Some(ActiveSystemSetting {
                    name: setting.name,
                    value: setting.value,
                });
                count += 1;
            }
        }
        self.system_settings_reloaded = true;
    }

    pub(crate) fn take_system_settings_reload(&mut self) -> bool {
        core::mem::take(&mut self.system_settings_reloaded)
    }

    pub(crate) fn role_can_connect(&self, role: u16) -> bool {
        self.storage
            .has_current_database_connect_privilege(role as usize, 0)
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

    pub(crate) fn reserve_database_connection(
        &mut self,
        database: DatabaseLogin,
        superuser: bool,
    ) -> bool {
        if !database.allow_connections && !superuser {
            return false;
        }
        let count = &mut self.database_connections[database.slot as usize];
        if !superuser
            && database.connection_limit >= 0
            && usize::from(*count) >= database.connection_limit as usize
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

    pub(crate) fn release_database_connection(&mut self, slot: u16) {
        let count = &mut self.database_connections[slot as usize];
        *count = count
            .checked_sub(1)
            .expect("an authenticated database connection is released once");
    }

    pub(crate) fn take_discard_protocol_state(&mut self) -> bool {
        core::mem::take(&mut self.discard_protocol_state)
    }

    pub(crate) fn dropped_database_connections(&self) -> [bool; crate::storage::MAX_DATABASES] {
        core::array::from_fn(|slot| {
            self.database_connections[slot] != 0
                && self.storage.database(slot).ddl_state == crate::storage::CatalogDdlState::Absent
        })
    }

    fn database_connection_count(&self, name: &str, txid: u32) -> u16 {
        self.storage
            .database_slot(name, txid)
            .map_or(0, |slot| self.database_connections[slot])
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
            + 2 * config.table_rows * size_of::<exec::PhysicalRow>()
            + (1 + crate::storage::MAX_PENDING_ROW_VERSIONS
                + crate::storage::MAX_COMMITTED_ROW_VERSIONS)
                * config.max_tables
                * config.table_rows
                * size_of::<(u32, u64, u8, RowLoc)>()
            + config.work_arena_bytes
            + config.wal_buffer_bytes
            + config.wal_upload_buffer_bytes.max(config.wal_buffer_bytes)
            + config.max_connections as usize * config.wal_buffer_bytes
            + config.max_connections as usize * size_of::<(i32, u64)>()
            + two_phase::PreparedTransactions::budget_bytes(config)
            + crate::pg::replication_client::ReplicationClient::budget_bytes(
                1,
                config.foreign_receive_bytes,
                config.foreign_send_bytes,
            )
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
        storage.load_extension_packages(config)?;
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
        let replay_floor = storage
            .prepared_transaction_catalog()
            .iter()
            .map(|prepared| prepared.first_lsn.saturating_sub(1))
            .min()
            .unwrap_or(floor)
            .min(floor);
        let expected_prepared: Vec<crate::util::StackStr<199>> = storage
            .prepared_transaction_catalog()
            .iter()
            .map(|prepared| prepared.gid)
            .collect();
        let mut wal = Wal::open(config, budget)?;
        let mut prepared_transactions = two_phase::PreparedTransactions::new(config, budget)?;
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
        let mut journal_tip = replay_floor;
        wal.replay(replay_floor, |lsn, record| {
            journal_tip = journal_tip.max(lsn);
            recovered.insert(lsn, record.to_vec());
            Ok(())
        })?;
        // RPO=0: merge any commit batches in the bucket newer than what the
        // local journal (possibly empty after disk loss) already covered.
        let mut segment_tip = replay_floor;
        if let Some(c) = ckpt.as_mut() {
            c.replay_commit_batches(replay_floor, |lsn, record| {
                segment_tip = segment_tip.max(lsn);
                recovered.entry(lsn).or_insert_with(|| record.to_vec());
                Ok(())
            })
            .map_err(EngineSetupError::Checkpoint)?;
        }
        replay_transaction_batches(&mut storage, &mut prepared_transactions, &recovered, floor)?;
        for expected in expected_prepared {
            let gid = ast::PreparedTransactionId::parse(expected.as_str()).ok_or({
                EngineSetupError::Checkpoint(crate::checkpoint::CheckpointSetupError::Corrupt(
                    "manifest prepared transaction identifier is invalid",
                ))
            })?;
            if prepared_transactions.find(gid).is_none() {
                return Err(EngineSetupError::Checkpoint(
                    crate::checkpoint::CheckpointSetupError::Corrupt(
                        "manifest prepared transaction has no retained commit batch",
                    ),
                ));
            }
        }
        let recovered_transaction_id = prepared_transactions
            .entries()
            .map(|(_, metadata)| metadata.transaction_id)
            .max()
            .unwrap_or(0);
        // WAL carries catalog identities as names where runtime slots are not
        // durable. Rebind each recovered database only after every replayed
        // definition exists.
        let mut recovered_databases =
            [crate::storage::DatabaseOid::POSTGRES; crate::storage::MAX_DATABASES];
        let mut recovered_database_count = 0usize;
        for (_, database) in storage.databases_visible_to(0) {
            recovered_databases[recovered_database_count] = database.oid;
            recovered_database_count += 1;
        }
        for database in &recovered_databases[..recovered_database_count] {
            storage.select_database(*database)?;
            wal.select_database(*database);
            storage.rebind_domain_base_types()?;
            storage.rebind_user_type_declarations()?;
            storage.rebind_routine_types()?;
            storage.rebind_all_stored_query_dependencies()?;
        }
        storage.select_database(crate::storage::DatabaseOid::POSTGRES)?;
        wal.select_database(crate::storage::DatabaseOid::POSTGRES);
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
        let mut active_system_settings = [None; crate::storage::MAX_SYSTEM_SETTINGS];
        let mut active_system_setting_count = 0usize;
        for (_, setting) in storage.system_settings() {
            if setting.live {
                active_system_settings[active_system_setting_count] = Some(ActiveSystemSetting {
                    name: setting.name,
                    value: setting.value,
                });
                active_system_setting_count += 1;
            }
        }
        let foreign_tls =
            crate::object_store::tls::build_client_config(&config.foreign_tls_ca_file)
                .map_err(EngineSetupError::ForeignTransport)?;
        let foreign_client = crate::pg::replication_client::ReplicationClient::new_unbound(
            budget,
            1,
            config.foreign_receive_bytes,
            config.foreign_send_bytes,
            Some(&foreign_tls),
        )
        .map_err(|error| EngineSetupError::ForeignTransport(error.to_string()))?;
        storage.install_foreign_client(foreign_client);
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
            dml_scratch: FixedVec::new(budget, "dml_scratch", config.table_rows)?,
            copy_transition_scratch: FixedVec::new(
                budget,
                "copy_transition_scratch",
                config.table_rows,
            )?,
            compact_scratch: FixedVec::new(
                budget,
                "compact_scratch",
                (1 + crate::storage::MAX_PENDING_ROW_VERSIONS
                    + crate::storage::MAX_COMMITTED_ROW_VERSIONS)
                    * config.max_tables
                    * config.table_rows,
            )?,
            work: Arena::new(budget, "work_arena", config.work_arena_bytes)?,
            next_txid: recovered_transaction_id,
            max_connections: config.max_connections,
            max_prepared_transactions: config.max_prepared_transactions,
            prepared_transactions,
            notify: notify::NotifyState::new(
                budget,
                config.max_connections as usize * notify::CHANNELS_PER_CONN,
                notify::OUTBOX,
            )?,
            current_conn_id: 0,
            exported_snapshots: FixedVec::new(
                budget,
                "exported_snapshots",
                config.max_connections as usize,
            )?,
            replication_system_id: crate::object_store::writer_id(config),
            role_connections: [0; crate::storage::MAX_ROLES],
            database_connections: [0; crate::storage::MAX_DATABASES],
            active_system_settings,
            system_settings_reloaded: false,
            discard_protocol_state: false,
        })
    }

    pub(crate) fn replication_identity(&self) -> (u64, u64) {
        (self.replication_system_id, self.wal.last_lsn())
    }

    fn exported_snapshot_owner(conn_id: i32) -> u32 {
        0x8000_0000 | conn_id as u32
    }

    pub(crate) fn export_replication_snapshot(
        &mut self,
        conn_id: i32,
        lsn: u64,
    ) -> Result<crate::util::StackStr<128>, SqlError> {
        if conn_id <= 0 {
            return Err(sql_err!(
                sqlstate::OBJECT_NOT_IN_PREREQUISITE_STATE,
                "replication snapshot requires a live connection"
            ));
        }
        if let Some(entry) = self
            .exported_snapshots
            .iter_mut()
            .find(|entry| entry.0 == conn_id)
        {
            self.storage
                .release_snapshot(Self::exported_snapshot_owner(conn_id));
            *entry = (conn_id, lsn);
        } else {
            self.exported_snapshots.push((conn_id, lsn)).map_err(|_| {
                sql_err!(
                    sqlstate::PROGRAM_LIMIT_EXCEEDED,
                    "too many exported replication snapshots"
                )
            })?;
        }
        if let Err(error) = self
            .storage
            .register_snapshot(Self::exported_snapshot_owner(conn_id), lsn)
        {
            if let Some(index) = self
                .exported_snapshots
                .iter()
                .position(|entry| entry.0 == conn_id)
            {
                self.exported_snapshots.swap_remove(index);
            }
            return Err(error);
        }
        Ok(stack_format!(128, "pos3ql:{conn_id}:{lsn:X}"))
    }

    pub(crate) fn invalidate_replication_snapshot(&mut self, conn_id: i32) {
        if let Some(index) = self
            .exported_snapshots
            .iter()
            .position(|entry| entry.0 == conn_id)
        {
            self.exported_snapshots.swap_remove(index);
            self.storage
                .release_snapshot(Self::exported_snapshot_owner(conn_id));
        }
    }

    fn import_replication_snapshot(
        &mut self,
        txn: &mut TxnState,
        name: &str,
    ) -> Result<(), SqlError> {
        let Some(rest) = name.strip_prefix("pos3ql:") else {
            return Err(sql_err!(
                sqlstate::INVALID_PARAMETER_VALUE,
                "invalid snapshot identifier"
            ));
        };
        let Some((connection, lsn)) = rest.split_once(':') else {
            return Err(sql_err!(
                sqlstate::INVALID_PARAMETER_VALUE,
                "invalid snapshot identifier"
            ));
        };
        let connection = connection.parse::<i32>().map_err(|_| {
            sql_err!(
                sqlstate::INVALID_PARAMETER_VALUE,
                "invalid snapshot identifier"
            )
        })?;
        let lsn = u64::from_str_radix(lsn, 16).map_err(|_| {
            sql_err!(
                sqlstate::INVALID_PARAMETER_VALUE,
                "invalid snapshot identifier"
            )
        })?;
        if !self.exported_snapshots.contains(&(connection, lsn)) {
            return Err(sql_err!(
                sqlstate::INVALID_PARAMETER_VALUE,
                "snapshot identifier is no longer valid"
            ));
        }
        txn.import_snapshot(lsn)?;
        self.storage.register_snapshot(txn.txid, lsn)
    }

    /// Creates a durable logical-replication resume point outside SQL
    /// transactions. Replication protocol commands have their own commit
    /// boundary, so this follows the same WAL-before-catalog order as a
    /// committed SQL transaction.
    pub(crate) fn create_replication_slot(
        &mut self,
        name: crate::storage::ReplicationSlotName,
        behavior: crate::storage::ReplicationSlotBehavior,
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
                behavior,
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
        self.storage
            .create_replication_slot(name, restart_lsn, behavior)?;
        self.storage.set_lsn(commit_lsn);
        Ok(restart_lsn)
    }

    pub(crate) fn drop_replication_slot(
        &mut self,
        name: crate::storage::ReplicationSlotName,
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

    pub(crate) fn alter_replication_slot(
        &mut self,
        name: crate::storage::ReplicationSlotName,
        behavior: crate::storage::ReplicationSlotBehavior,
    ) -> Result<(), SqlError> {
        let current = self
            .storage
            .replication_slot(name.as_str())
            .ok_or_else(|| {
                sql_err!(
                    sqlstate::UNDEFINED_OBJECT,
                    "replication slot \"{}\" does not exist",
                    name.as_str()
                )
            })?;
        if current.active {
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
            &WalOp::AlterReplicationSlot {
                name: name.as_str(),
                behavior,
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
        self.storage.alter_replication_slot(name, behavior)?;
        self.storage.set_lsn(commit_lsn);
        Ok(())
    }

    pub(crate) fn activate_replication_slot(&mut self, name: &str) -> Result<u64, SqlError> {
        self.storage.activate_replication_slot(name)
    }

    pub(crate) fn deactivate_replication_slot(&mut self, name: &str) {
        self.storage.deactivate_replication_slot(name);
    }

    /// Validates pgoutput's publication set before a replication slot is made
    /// active, so an invalid stream cannot acquire a transport cursor.
    pub(crate) fn validate_replication_publications(
        &self,
        publication_names: &[SqlName],
    ) -> Result<(), SqlError> {
        for name in publication_names {
            if self.storage.publication(name.as_str()).is_none() {
                return Err(sql_err!(
                    sqlstate::UNDEFINED_OBJECT,
                    "publication \"{}\" does not exist",
                    name.as_str()
                ));
            }
        }
        for (table_slot, _) in self.storage.live_tables() {
            let mut selected = None;
            for name in publication_names {
                let publication = self
                    .storage
                    .publication(name.as_str())
                    .expect("publication set was validated");
                let Some(mask) =
                    publication_projection_mask(&self.storage, publication, table_slot)
                else {
                    continue;
                };
                if selected.is_some_and(|selected| selected != mask) {
                    return Err(mismatched_publication_columns(&self.storage, table_slot));
                }
                selected = Some(mask);
            }
        }
        Ok(())
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
    #[cfg(test)]
    pub(crate) fn emit_replication_transaction(
        &mut self,
        floor: u64,
        publication_names: &[SqlName],
        binary: bool,
        proto_version: crate::pg::pgoutput::ProtocolVersion,
        scratch: &mut FixedBuf,
        responder: &mut Responder,
    ) -> Result<Option<(u64, bool)>, SqlError> {
        self.emit_replication_transaction_for_origin(
            floor,
            ReplicationEmission {
                publications: publication_names,
                binary,
                origin: crate::storage::SubscriptionOrigin::Any,
                protocol: proto_version,
            },
            scratch,
            responder,
        )
    }

    pub(crate) fn emit_replication_transaction_for_origin(
        &mut self,
        floor: u64,
        emission: ReplicationEmission<'_>,
        scratch: &mut FixedBuf,
        responder: &mut Responder,
    ) -> Result<Option<(u64, bool)>, SqlError> {
        let ReplicationEmission {
            publications: publication_names,
            binary,
            origin,
            protocol: proto_version,
        } = emission;
        self.validate_replication_publications(publication_names)?;
        self.work.reset();
        let storage = &self.storage;
        let filter_arena = &self.work;
        let mut emitted = false;
        let mut encode = |end_lsn, transaction: &[u8]| {
            let mut at = 0usize;
            let mut transaction_id = 0u32;
            let mut has_replication_origin = false;
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
                if matches!(
                    crate::wal::decode_record(&transaction[at + 16..at + total]),
                    Some(WalOp::AdvanceSubscription { .. })
                ) {
                    has_replication_origin = true;
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
            if origin == crate::storage::SubscriptionOrigin::None && has_replication_origin {
                return Ok(());
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
                        row,
                        is_update,
                        old_row,
                        ..
                    } => {
                        if storage.is_large_object_page_relation(schema, table) {
                            false
                        } else {
                            let table_slot =
                                storage.find_table(schema, table).ok_or_else(|| {
                                    sql_err!(
                                        sqlstate::UNDEFINED_TABLE,
                                        "replication WAL refers to unknown table \"{}\"",
                                        table
                                    )
                                })?;
                            let definition = storage.table_def(table_slot, 0);
                            let mut types = [ColType::Bool; crate::storage::MAX_COLUMNS];
                            let count = definition.schema(&mut types);
                            let mut values = [Datum::Null; crate::storage::MAX_COLUMNS];
                            crate::storage::rowenc::decode(row, &types[..count], &mut values)?;
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
                                    &types[..count],
                                    &mut old_values,
                                )?;
                                publication_row_matches(
                                    storage,
                                    publication_names,
                                    table_slot,
                                    PublicationOperation::Update,
                                    &values[..count],
                                    filter_arena,
                                )? || publication_row_matches(
                                    storage,
                                    publication_names,
                                    table_slot,
                                    PublicationOperation::Update,
                                    &old_values[..count],
                                    filter_arena,
                                )?
                            } else {
                                publication_row_matches(
                                    storage,
                                    publication_names,
                                    table_slot,
                                    PublicationOperation::Insert,
                                    &values[..count],
                                    filter_arena,
                                )?
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
                        if storage.is_large_object_page_relation(schema, table) {
                            false
                        } else {
                            let table_slot =
                                storage.find_table(schema, table).ok_or_else(|| {
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
                            if suppressed_by_truncate {
                                false
                            } else {
                                let old = old_row.ok_or_else(|| {
                                    sql_err!(
                                        sqlstate::PROTOCOL_VIOLATION,
                                        "delete WAL record lacks replica identity"
                                    )
                                })?;
                                let definition = storage.table_def(table_slot, 0);
                                let mut types = [ColType::Bool; crate::storage::MAX_COLUMNS];
                                let count = definition.schema(&mut types);
                                let mut values = [Datum::Null; crate::storage::MAX_COLUMNS];
                                crate::storage::rowenc::decode(old, &types[..count], &mut values)?;
                                publication_row_matches(
                                    storage,
                                    publication_names,
                                    table_slot,
                                    PublicationOperation::Delete,
                                    &values[..count],
                                    filter_arena,
                                )?
                            }
                        }
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
                        if storage.is_large_object_page_relation(schema, table) {
                            at += total;
                            continue;
                        }
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
                        if let Some(column_mask) = publication_column_mask(
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
                                let old_matches = publication_row_matches(
                                    storage,
                                    publication_names,
                                    table_slot,
                                    PublicationOperation::Update,
                                    &old_values[..column_count],
                                    filter_arena,
                                )?;
                                let new_matches = publication_row_matches(
                                    storage,
                                    publication_names,
                                    table_slot,
                                    PublicationOperation::Update,
                                    &values[..column_count],
                                    filter_arena,
                                )?;
                                if old_matches || new_matches {
                                    let output_slot = publication_output_relation(
                                        storage,
                                        publication_names,
                                        table_slot,
                                        PublicationOperation::Update,
                                    )?;
                                    let relation_id = output_slot as u32 + 1;
                                    let replica_identity =
                                        logical_replica_identity(storage, output_slot)?;
                                    emit_replication_relation(
                                        storage,
                                        output_slot,
                                        storage.table_def(output_slot, 0),
                                        relation_id,
                                        column_mask,
                                        responder,
                                        end_lsn,
                                    )?;
                                    responder
                                        .copy_data(&|message| {
                                            let (old_projected, old_count) =
                                                project_replication_values(
                                                    &old_values[..column_count],
                                                    replica_identity.key_mask,
                                                );
                                            let (projected, projected_count) =
                                                project_replication_values(
                                                    &values[..column_count],
                                                    column_mask,
                                                );
                                            pgoutput::xlog_data(message, lsn, end_lsn, |plugin| {
                                                match (old_matches, new_matches) {
                                                    (true, true) => pgoutput::update(
                                                        plugin,
                                                        relation_id,
                                                        &old_projected[..old_count],
                                                        &projected[..projected_count],
                                                        binary,
                                                        replica_identity.wire,
                                                    ),
                                                    (true, false) => pgoutput::delete(
                                                        plugin,
                                                        relation_id,
                                                        &old_projected[..old_count],
                                                        binary,
                                                        replica_identity.wire,
                                                    ),
                                                    (false, true) => pgoutput::insert(
                                                        plugin,
                                                        relation_id,
                                                        &projected[..projected_count],
                                                        binary,
                                                    ),
                                                    (false, false) => unreachable!(),
                                                }
                                            })
                                        })
                                        .map_err(|_| overflow())?;
                                }
                            } else {
                                if publication_row_matches(
                                    storage,
                                    publication_names,
                                    table_slot,
                                    PublicationOperation::Insert,
                                    &values[..column_count],
                                    filter_arena,
                                )? {
                                    let output_slot = publication_output_relation(
                                        storage,
                                        publication_names,
                                        table_slot,
                                        PublicationOperation::Insert,
                                    )?;
                                    let relation_id = output_slot as u32 + 1;
                                    emit_replication_relation(
                                        storage,
                                        output_slot,
                                        storage.table_def(output_slot, 0),
                                        relation_id,
                                        column_mask,
                                        responder,
                                        end_lsn,
                                    )?;
                                    responder
                                        .copy_data(&|message| {
                                            let (projected, projected_count) =
                                                project_replication_values(
                                                    &values[..column_count],
                                                    column_mask,
                                                );
                                            pgoutput::xlog_data(message, lsn, end_lsn, |plugin| {
                                                pgoutput::insert(
                                                    plugin,
                                                    relation_id,
                                                    &projected[..projected_count],
                                                    binary,
                                                )
                                            })
                                        })
                                        .map_err(|_| overflow())?;
                                }
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
                        if storage.is_large_object_page_relation(schema, table) {
                            at += total;
                            continue;
                        }
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
                        if let Some(column_mask) = publication_column_mask(
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
                            if publication_row_matches(
                                storage,
                                publication_names,
                                table_slot,
                                PublicationOperation::Delete,
                                &values[..column_count],
                                filter_arena,
                            )? {
                                let output_slot = publication_output_relation(
                                    storage,
                                    publication_names,
                                    table_slot,
                                    PublicationOperation::Delete,
                                )?;
                                let relation_id = output_slot as u32 + 1;
                                let replica_identity =
                                    logical_replica_identity(storage, output_slot)?;
                                emit_replication_relation(
                                    storage,
                                    output_slot,
                                    storage.table_def(output_slot, 0),
                                    relation_id,
                                    column_mask,
                                    responder,
                                    end_lsn,
                                )?;
                                responder
                                    .copy_data(&|message| {
                                        let (projected, projected_count) =
                                            project_replication_values(
                                                &values[..column_count],
                                                replica_identity.key_mask,
                                            );
                                        pgoutput::xlog_data(message, lsn, end_lsn, |plugin| {
                                            pgoutput::delete(
                                                plugin,
                                                relation_id,
                                                &projected[..projected_count],
                                                binary,
                                                replica_identity.wire,
                                            )
                                        })
                                    })
                                    .map_err(|_| overflow())?;
                            }
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
        let (isolation, read_only, deferrable) = guc.transaction_defaults();
        txn.set_characteristics(isolation, read_only, deferrable);
        txn.failed = false;
    }

    fn begin_command_snapshot(
        &mut self,
        txn: &mut TxnState,
        takes_snapshot: bool,
    ) -> Result<(), SqlError> {
        txn.begin_command();
        self.storage.set_read_snapshot(crate::storage::SNAPSHOT_ALL);
        let snapshot = if takes_snapshot {
            let snapshot = txn.statement_snapshot(self.storage.lsn());
            if matches!(
                txn.isolation,
                TransactionIsolation::RepeatableRead | TransactionIsolation::Serializable
            ) {
                self.storage.register_snapshot(txn.txid, snapshot)?;
            }
            if txn.isolation == TransactionIsolation::Serializable {
                self.storage.begin_serializable(txn.txid)?;
            }
            snapshot
        } else {
            self.storage.lsn()
        };
        self.storage.set_commit_snapshot(snapshot);
        Ok(())
    }

    pub fn commit_txn(&mut self, txn: &mut TxnState, guc: &GucState) -> Result<(), SqlError> {
        self.finish_txn(txn, guc, None)
    }

    /// Journals every final transaction image and either commits it or leaves
    /// its typed batch prepared. Only an ordinary commit promotes the
    /// transaction-owned storage overlays here.
    fn finish_txn(
        &mut self,
        txn: &mut TxnState,
        guc: &GucState,
        prepared_slot: Option<usize>,
    ) -> Result<(), SqlError> {
        // The next statement starts a fresh transaction clock.
        datetime::end_transaction();
        if !txn.is_active() {
            return Ok(());
        }
        if txn.has_deferred_triggers() {
            return Err(sql_err!(
                sqlstate::INTERNAL_ERROR,
                "commit reached durability before queued constraint triggers fired"
            ));
        }
        self.work.reset();
        if let Err(error) =
            exec::constraints::validate_deferred_constraints(&self.storage, txn, false, &self.work)
        {
            self.rollback_txn(txn, guc);
            return Err(error);
        }
        if txn.isolation == TransactionIsolation::Serializable
            && (!txn.touched().is_empty() || !txn.ddl().is_empty())
            && let Err(error) = self.storage.validate_serializable(txn.txid)
        {
            self.rollback_txn(txn, guc);
            return Err(error);
        }
        // This transaction no longer needs its historical view. Release it
        // before promotion so only other live snapshots cause old row images
        // to be retained.
        if prepared_slot.is_none() {
            self.storage.release_snapshot(txn.txid);
            self.storage.release_serializable(txn.txid);
            self.storage.release_table_locks(txn.txid);
            self.storage.release_row_locks(txn.txid);
        }
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
        for slot in 0..self.storage.extended_statistics_count() {
            let Some(statistics) = self
                .storage
                .pending_extended_statistics_data_for(slot, txn.txid)
            else {
                continue;
            };
            let definition = self
                .storage
                .extended_statistics(slot)
                .definition_for(txn.txid);
            let lsn = self.storage.lsn() + 1;
            if let Err(error) = self.wal.stage(
                txn.txid,
                lsn,
                &WalOp::AnalyzeExtendedStatistics {
                    schema: definition.schema.as_str(),
                    name: definition.name.as_str(),
                    statistics: crate::wal::WalExtendedStatisticsData::Captured(&statistics),
                },
            ) {
                self.rollback_txn(txn, guc);
                return Err(error);
            }
            self.storage.set_lsn(lsn);
        }
        for (position, undo) in txn.ddl().iter().enumerate() {
            let (slot, dropping) = match *undo {
                DdlUndo::ExtensionCreated(slot) | DdlUndo::ExtensionAltered { slot, .. } => {
                    (slot as usize, false)
                }
                DdlUndo::ExtensionDropped(slot) => (slot as usize, true),
                _ => continue,
            };
            if txn.ddl()[position + 1..].iter().any(|later| {
                matches!(
                    *later,
                    DdlUndo::ExtensionCreated(later_slot)
                        | DdlUndo::ExtensionDropped(later_slot)
                        | DdlUndo::ExtensionAltered { slot: later_slot, .. }
                        if later_slot as usize == slot
                )
            }) {
                continue;
            }
            let extension = *self.storage.extension(slot);
            let lsn = self.storage.lsn() + 1;
            let result = if dropping || !extension.visible_to(txn.txid) {
                self.wal.stage(
                    txn.txid,
                    lsn,
                    &WalOp::DropExtension {
                        name: extension.name.as_str(),
                    },
                )
            } else {
                let (namespace, relocatable, version) = extension.definition_to(txn.txid);
                let object = crate::storage::AccessObject {
                    class: crate::storage::AccessClass::Extension,
                    slot: slot as u16,
                };
                let owner = self.storage.object_owner(object, txn.txid);
                let owner = self.storage.role_name(owner, txn.txid);
                self.wal.stage(
                    txn.txid,
                    lsn,
                    &WalOp::UpsertExtension {
                        name: extension.name.as_str(),
                        schema: self.storage.schema_def(namespace as usize).name.as_str(),
                        version: version.as_str(),
                        relocatable,
                        owner: owner.as_str(),
                        created_at: extension.created_at,
                    },
                )
            };
            if let Err(error) = result {
                self.rollback_txn(txn, guc);
                return Err(error);
            }
            self.storage.set_lsn(lsn);
        }
        for (position, undo) in txn.ddl().iter().enumerate() {
            let DdlUndo::ExtensionDependencyChanged { slot, .. } = *undo else {
                continue;
            };
            if txn.ddl()[position + 1..].iter().any(|later| {
                matches!(
                    *later,
                    DdlUndo::ExtensionDependencyChanged { slot: later_slot, .. }
                        if later_slot == slot
                )
            }) {
                continue;
            }
            let dependency = *self.storage.extension_dependency(slot as usize);
            let exists = dependency.visible_to(txn.txid);
            if dependency.live == exists {
                continue;
            }
            if !self
                .storage
                .extension(dependency.extension as usize)
                .visible_to(txn.txid)
                || !self
                    .storage
                    .access_object_visible_to(dependency.object, txn.txid)
            {
                continue;
            }
            let extension = self.storage.extension(dependency.extension as usize).name;
            let (schema, name) = self
                .storage
                .access_object_name_to(dependency.object, txn.txid);
            let lsn = self.storage.lsn() + 1;
            if let Err(error) = self.wal.stage(
                txn.txid,
                lsn,
                &WalOp::SetExtensionDependency {
                    extension: extension.as_str(),
                    class: dependency.object.class,
                    object_oid: if dependency.object.class == crate::storage::AccessClass::Routine {
                        crate::storage::routine_oid(
                            &self
                                .storage
                                .routine_for(dependency.object.slot as usize, txn.txid),
                        )
                    } else {
                        0
                    },
                    schema: schema.as_str(),
                    name: name.as_str(),
                    kind: dependency.kind,
                    exists,
                },
            ) {
                self.rollback_txn(txn, guc);
                return Err(error);
            }
            self.storage.set_lsn(lsn);
        }
        for (position, undo) in txn.ddl().iter().enumerate() {
            let DdlUndo::ExtensionConfigChanged { slot, .. } = *undo else {
                continue;
            };
            if txn.ddl()[position + 1..].iter().any(|later| {
                matches!(
                    *later,
                    DdlUndo::ExtensionConfigChanged { slot: later_slot, .. }
                        if later_slot == slot
                )
            }) {
                continue;
            }
            let config = *self.storage.extension_config(slot as usize);
            let extension = self.storage.extension(config.extension as usize).name;
            let (schema, name) = self
                .storage
                .access_object_name_to(config.relation.access_object(), txn.txid);
            let lsn = self.storage.lsn() + 1;
            if let Err(error) = self.wal.stage(
                txn.txid,
                lsn,
                &WalOp::SetExtensionConfig {
                    extension: extension.as_str(),
                    ordinal: config.ordinal,
                    relation_kind: config.relation.kind(),
                    schema: schema.as_str(),
                    name: name.as_str(),
                    condition: config.condition_to(txn.txid).as_str(),
                    exists: config.visible_to(txn.txid),
                },
            ) {
                self.rollback_txn(txn, guc);
                return Err(error);
            }
            self.storage.set_lsn(lsn);
        }
        let foreign_target = |undo: DdlUndo| match undo {
            DdlUndo::ForeignDataWrapperCreated(slot)
            | DdlUndo::ForeignDataWrapperAltered { slot, .. }
            | DdlUndo::ForeignDataWrapperDropped(slot) => {
                Some((crate::storage::foreign::ForeignObjectClass::Wrapper, slot))
            }
            DdlUndo::ForeignServerCreated(slot)
            | DdlUndo::ForeignServerAltered { slot, .. }
            | DdlUndo::ForeignServerDropped(slot) => {
                Some((crate::storage::foreign::ForeignObjectClass::Server, slot))
            }
            DdlUndo::UserMappingCreated(slot)
            | DdlUndo::UserMappingAltered { slot, .. }
            | DdlUndo::UserMappingDropped(slot) => {
                Some((crate::storage::foreign::ForeignObjectClass::Mapping, slot))
            }
            DdlUndo::ForeignTableCreated(slot)
            | DdlUndo::ForeignTableAltered { slot, .. }
            | DdlUndo::ForeignTableDropped(slot) => {
                Some((crate::storage::foreign::ForeignObjectClass::Table, slot))
            }
            DdlUndo::ForeignOwnerChanged { class, slot, .. } => Some((class, slot)),
            _ => None,
        };
        for (position, undo) in txn.ddl().iter().copied().enumerate() {
            let Some((class, slot)) = foreign_target(undo) else {
                continue;
            };
            if txn.ddl()[position + 1..]
                .iter()
                .copied()
                .filter_map(foreign_target)
                .any(|later| later == (class, slot))
            {
                continue;
            }
            let operation = match class {
                crate::storage::foreign::ForeignObjectClass::Wrapper => {
                    let entry = self.storage.foreign_wrapper_entry(slot as usize);
                    WalOp::SetForeignDataWrapper {
                        slot: slot as u16,
                        created_at: entry.created_at,
                        owner: entry.ownership.owner_to(txn.txid),
                        definition: entry
                            .visible_to(txn.txid)
                            .then(|| entry.definition_for(txn.txid)),
                    }
                }
                crate::storage::foreign::ForeignObjectClass::Server => {
                    let entry = self.storage.foreign_server_entry(slot as usize);
                    WalOp::SetForeignServer {
                        slot: slot as u16,
                        created_at: entry.created_at,
                        owner: entry.ownership.owner_to(txn.txid),
                        definition: entry
                            .visible_to(txn.txid)
                            .then(|| entry.definition_for(txn.txid)),
                    }
                }
                crate::storage::foreign::ForeignObjectClass::Mapping => {
                    let entry = self.storage.foreign_mapping_entry(slot as usize);
                    WalOp::SetUserMapping {
                        slot: slot as u16,
                        created_at: entry.created_at,
                        definition: entry
                            .visible_to(txn.txid)
                            .then(|| entry.definition_for(txn.txid)),
                    }
                }
                crate::storage::foreign::ForeignObjectClass::Table => {
                    let entry = self.storage.foreign_table_entry(slot as usize);
                    WalOp::SetForeignTable {
                        slot: slot as u16,
                        created_at: entry.created_at,
                        definition: entry
                            .visible_to(txn.txid)
                            .then(|| entry.definition_for(txn.txid)),
                    }
                }
            };
            let lsn = self.storage.lsn() + 1;
            if let Err(error) = self.wal.stage(txn.txid, lsn, &operation) {
                self.rollback_txn(txn, guc);
                return Err(error);
            }
            self.storage.set_lsn(lsn);
        }
        for undo in txn.ddl() {
            let operation = match *undo {
                DdlUndo::LargeObjectCreated(slot) => {
                    let object = self.storage.large_object(slot as usize);
                    WalOp::CreateLargeObject {
                        oid: object.oid.get(),
                        created_at: object.created_at,
                        allocated: object.allocated,
                    }
                }
                DdlUndo::LargeObjectDropped(slot) => WalOp::DropLargeObject {
                    oid: self.storage.large_object(slot as usize).oid.get(),
                },
                _ => continue,
            };
            let lsn = self.storage.lsn() + 1;
            if let Err(error) = self.wal.stage(txn.txid, lsn, &operation) {
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
                DdlUndo::CompositeCreated(slot) => Some(crate::storage::AccessObject {
                    class: crate::storage::AccessClass::Composite,
                    slot: slot as u16,
                }),
                DdlUndo::IndexCreated(slot) => Some(crate::storage::AccessObject {
                    class: crate::storage::AccessClass::Index,
                    slot: slot as u16,
                }),
                DdlUndo::StatisticsCreated(slot) => Some(crate::storage::AccessObject {
                    class: crate::storage::AccessClass::Statistics,
                    slot: slot as u16,
                }),
                DdlUndo::SchemaCreated(slot) => Some(crate::storage::AccessObject {
                    class: crate::storage::AccessClass::Schema,
                    slot: slot as u16,
                }),
                DdlUndo::ExtensionCreated(slot) => Some(crate::storage::AccessObject {
                    class: crate::storage::AccessClass::Extension,
                    slot: slot as u16,
                }),
                DdlUndo::LargeObjectCreated(slot) => Some(crate::storage::AccessObject {
                    class: crate::storage::AccessClass::LargeObject,
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
                    object_oid: match object.class {
                        crate::storage::AccessClass::Routine => {
                            crate::storage::routine_oid(self.storage.routine(object.slot as usize))
                        }
                        crate::storage::AccessClass::LargeObject => {
                            self.storage.large_object(object.slot as usize).oid.get() as i32
                        }
                        _ => 0,
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
                    object_oid: match object.class {
                        crate::storage::AccessClass::Routine => {
                            crate::storage::routine_oid(self.storage.routine(object.slot as usize))
                        }
                        crate::storage::AccessClass::LargeObject => {
                            self.storage.large_object(object.slot as usize).oid.get() as i32
                        }
                        _ => 0,
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
            let DdlUndo::ColumnAclChanged { slot, .. } = *undo else {
                continue;
            };
            if txn.ddl()[position + 1..].iter().any(
                |later| matches!(later, DdlUndo::ColumnAclChanged { slot: later, .. } if *later == slot),
            ) {
                continue;
            }
            let entry = *self.storage.column_acl_entry(slot as usize);
            let relation = entry.target.relation();
            if !self.storage.access_object_visible_to(relation, txn.txid) {
                continue;
            }
            let (grantee, grantor) = self.storage.column_acl_identity(slot as usize, txn.txid);
            if txn.ddl()[..position].iter().any(|earlier| {
                let DdlUndo::ColumnAclChanged {
                    slot: earlier_slot, ..
                } = *earlier
                else {
                    return false;
                };
                if earlier_slot == slot {
                    return false;
                }
                let earlier_entry = self.storage.column_acl_entry(earlier_slot as usize);
                earlier_entry.target == entry.target
                    && self
                        .storage
                        .column_acl_identity(earlier_slot as usize, txn.txid)
                        == (grantee, grantor)
            }) {
                continue;
            }
            let (privileges, grant_options) =
                self.storage
                    .column_acl_from(entry.target, grantee, grantor, txn.txid);
            let (schema, name) = self.storage.access_object_name_to(relation, txn.txid);
            let column = entry.target.column();
            let grantee_name = (grantee != crate::storage::PUBLIC_ROLE)
                .then(|| self.storage.role_name(grantee as usize, txn.txid));
            let grantor_name = self.storage.role_name(grantor as usize, txn.txid);
            let lsn = self.storage.lsn() + 1;
            if let Err(error) = self.wal.stage(
                txn.txid,
                lsn,
                &WalOp::SetColumnAcl {
                    class: relation.class as u8,
                    schema: schema.as_str(),
                    name: name.as_str(),
                    column,
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
            let DdlUndo::ParameterAclChanged { slot, .. } = *undo else {
                continue;
            };
            if txn.ddl()[position + 1..].iter().any(
                |later| matches!(later, DdlUndo::ParameterAclChanged { slot: later, .. } if *later == slot),
            ) {
                continue;
            }
            let entry = *self.storage.parameter_acl_entry(slot as usize);
            let (grantee, grantor) = self.storage.parameter_acl_identity(slot as usize, txn.txid);
            if txn.ddl()[..position].iter().any(|earlier| {
                let DdlUndo::ParameterAclChanged {
                    slot: earlier_slot, ..
                } = *earlier
                else {
                    return false;
                };
                if earlier_slot == slot {
                    return false;
                }
                let earlier_entry = self.storage.parameter_acl_entry(earlier_slot as usize);
                earlier_entry.parameter == entry.parameter
                    && self
                        .storage
                        .parameter_acl_identity(earlier_slot as usize, txn.txid)
                        == (grantee, grantor)
            }) {
                continue;
            }
            let (privileges, grant_options) =
                self.storage.parameter_acl_state(slot as usize, txn.txid);
            let grantee_name = (grantee != crate::storage::PUBLIC_ROLE)
                .then(|| self.storage.role_name(grantee as usize, txn.txid));
            let grantor_name = self.storage.role_name(grantor as usize, txn.txid);
            let lsn = self.storage.lsn() + 1;
            if let Err(error) = self.wal.stage(
                txn.txid,
                lsn,
                &WalOp::SetParameterAcl {
                    parameter: entry.parameter.as_str(),
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
                    created_at: advance.stream().created_at(),
                    definition_generation: advance.stream().definition_generation(),
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
        let boundary_bytes =
            prepared_slot.map_or_else(crate::wal::Wal::commit_boundary_bytes, |slot| {
                crate::wal::Wal::prepare_boundary_bytes(
                    self.prepared_transactions.slot(slot).metadata(),
                )
            });
        let next_batch_bytes = self
            .wal
            .pending_batch_bytes()
            .saturating_add(staged_bytes)
            .saturating_add(boundary_bytes as u64);
        if next_batch_bytes > self.wal_seg_buf.capacity() as u64
            && let Err(error) = self.commit_wal()
        {
            self.rollback_txn(txn, guc);
            return Err(error);
        }
        let finish_result = match prepared_slot {
            Some(slot) => {
                let metadata = self.prepared_transactions.slot(slot).metadata();
                let records = &mut self.prepared_transactions.slot_mut(slot).records;
                self.wal
                    .prepare_stage(metadata, self.storage.lsn(), records)
            }
            None => self.wal.commit_stage(txn.txid, self.storage.lsn()),
        };
        let commit_lsn = match finish_result {
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
        if prepared_slot.is_some() {
            guc.commit_transaction();
            return Ok(());
        }
        self.promote_transaction_state(txn, commit_lsn, Some(guc))
    }

    /// Promotes an already-durable transaction's live overlays. Ordinary
    /// COMMIT and COMMIT PREPARED share this transition rather than rebuilding
    /// in-memory state from the journal.
    fn promote_transaction_state(
        &mut self,
        txn: &mut TxnState,
        commit_lsn: u64,
        guc: Option<&GucState>,
    ) -> Result<(), SqlError> {
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
                DdlUndo::ViewSchemaChanged { slot, .. } => {
                    self.storage.commit_view_schema(*slot as usize, txn.txid)
                }
                DdlUndo::ViewRenamed { slot, .. } => {
                    self.storage.commit_view_rename(*slot as usize, txn.txid)
                }
                DdlUndo::ViewOptionsChanged { slot, .. } => {
                    self.storage.commit_view_options(*slot as usize, txn.txid)
                }
                DdlUndo::ViewColumnsChanged { slot, .. } => {
                    self.storage.commit_view_columns(*slot as usize, txn.txid)
                }
                DdlUndo::RuleCreated { slot, .. } => {
                    self.storage.commit_rule_create(*slot as usize)
                }
                DdlUndo::RuleAltered { slot, .. } => {
                    self.storage.commit_rule_alter(*slot as usize, txn.txid)
                }
                DdlUndo::RuleDropped(slot) => self.storage.commit_rule_drop(*slot as usize),
                DdlUndo::RoutineCreated(slot) => {
                    self.storage.commit_routine_create(*slot as usize, txn.txid)
                }
                DdlUndo::RoutineDropped(slot) => self.storage.commit_routine_drop(*slot as usize),
                DdlUndo::RoutineReplaced { slot, .. } => self
                    .storage
                    .commit_routine_replace(*slot as usize, txn.txid),
                DdlUndo::CastCreated(slot) => self.storage.commit_cast_create(*slot as usize),
                DdlUndo::CastDropped(slot) => self.storage.commit_cast_drop(*slot as usize),
                DdlUndo::OperatorCreated(slot) => {
                    self.storage.commit_operator_create(*slot as usize)
                }
                DdlUndo::CollationCreated(slot) => {
                    self.storage.commit_collation_create(*slot as usize)
                }
                DdlUndo::CollationAltered { slot, .. } => self
                    .storage
                    .commit_collation_alter(*slot as usize, txn.txid),
                DdlUndo::CollationDropped(slot) => {
                    self.storage.commit_collation_drop(*slot as usize)
                }
                DdlUndo::ConversionCreated(slot) => {
                    self.storage.commit_conversion_create(*slot as usize)
                }
                DdlUndo::ConversionAltered { slot, .. } => self
                    .storage
                    .commit_conversion_alter(*slot as usize, txn.txid),
                DdlUndo::ConversionDropped(slot) => {
                    self.storage.commit_conversion_drop(*slot as usize)
                }
                DdlUndo::TextSearchCreated(slot) => {
                    self.storage.commit_text_search_create(*slot as usize)
                }
                DdlUndo::TextSearchAltered { slot, .. } => self
                    .storage
                    .commit_text_search_alter(*slot as usize, txn.txid),
                DdlUndo::TextSearchDropped(slot) => {
                    self.storage.commit_text_search_drop(*slot as usize)
                }
                DdlUndo::EventTriggerCreated(slot) => {
                    self.storage.commit_event_trigger_create(*slot as usize)
                }
                DdlUndo::EventTriggerAltered { slot, .. } => self
                    .storage
                    .commit_event_trigger_alter(*slot as usize, txn.txid),
                DdlUndo::EventTriggerDropped(slot) => {
                    self.storage.commit_event_trigger_drop(*slot as usize)
                }
                DdlUndo::OperatorAltered { slot, .. } => {
                    self.storage.commit_operator_alter(*slot as usize, txn.txid)
                }
                DdlUndo::OperatorDropped(slot) => self.storage.commit_operator_drop(*slot as usize),
                DdlUndo::OperatorFamilyCreated(slot) => {
                    self.storage.commit_operator_family_create(*slot as usize)
                }
                DdlUndo::OperatorFamilyAltered { slot, .. } => self
                    .storage
                    .commit_operator_family_alter(*slot as usize, txn.txid),
                DdlUndo::OperatorFamilyDropped(slot) => {
                    self.storage.commit_operator_family_drop(*slot as usize)
                }
                DdlUndo::OperatorClassCreated(slot) => {
                    self.storage.commit_operator_class_create(*slot as usize)
                }
                DdlUndo::OperatorClassAltered { slot, .. } => self
                    .storage
                    .commit_operator_class_alter(*slot as usize, txn.txid),
                DdlUndo::OperatorClassDropped(slot) => {
                    self.storage.commit_operator_class_drop(*slot as usize)
                }
                DdlUndo::TriggerCreated(slot) => self.storage.commit_trigger_create(*slot as usize),
                DdlUndo::TriggerDropped(slot) => self.storage.commit_trigger_drop(*slot as usize),
                DdlUndo::TriggerAltered { slot, .. } => {
                    self.storage.commit_trigger_alter(*slot as usize, txn.txid)
                }
                DdlUndo::PartitionTriggerAltered { slot, .. } => self
                    .storage
                    .commit_partition_trigger_state(*slot as usize, txn.txid),
                DdlUndo::PolicyCreated(slot) => self.storage.commit_policy_create(*slot as usize),
                DdlUndo::PolicyDropped(slot) => self.storage.commit_policy_drop(*slot as usize),
                DdlUndo::PolicyAltered { slot, .. } => {
                    self.storage.commit_policy_alter(*slot as usize, txn.txid)
                }
                DdlUndo::StatisticsCreated(slot) => self
                    .storage
                    .commit_extended_statistics_create(*slot as usize),
                DdlUndo::StatisticsDropped(slot) => {
                    self.storage.commit_extended_statistics_drop(*slot as usize)
                }
                DdlUndo::StatisticsAltered { slot, .. } => self
                    .storage
                    .commit_extended_statistics_alter(*slot as usize, txn.txid),
                DdlUndo::StatisticsKeysAltered { slot, .. } => self
                    .storage
                    .commit_extended_statistics_alter(*slot as usize, txn.txid),
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
                DdlUndo::ForeignDataWrapperCreated(slot) => {
                    self.storage.foreign_catalog_commit_create(
                        crate::storage::foreign::ForeignObjectClass::Wrapper,
                        *slot as usize,
                    )
                }
                DdlUndo::ForeignDataWrapperAltered { slot, .. } => {
                    self.storage.foreign_catalog_commit_alter(
                        crate::storage::foreign::ForeignObjectClass::Wrapper,
                        *slot as usize,
                        txn.txid,
                    )
                }
                DdlUndo::ForeignDataWrapperDropped(slot) => {
                    self.storage.foreign_catalog_commit_drop(
                        crate::storage::foreign::ForeignObjectClass::Wrapper,
                        *slot as usize,
                    )
                }
                DdlUndo::ForeignServerCreated(slot) => self.storage.foreign_catalog_commit_create(
                    crate::storage::foreign::ForeignObjectClass::Server,
                    *slot as usize,
                ),
                DdlUndo::ForeignServerAltered { slot, .. } => {
                    self.storage.foreign_catalog_commit_alter(
                        crate::storage::foreign::ForeignObjectClass::Server,
                        *slot as usize,
                        txn.txid,
                    )
                }
                DdlUndo::ForeignServerDropped(slot) => self.storage.foreign_catalog_commit_drop(
                    crate::storage::foreign::ForeignObjectClass::Server,
                    *slot as usize,
                ),
                DdlUndo::UserMappingCreated(slot) => self.storage.foreign_catalog_commit_create(
                    crate::storage::foreign::ForeignObjectClass::Mapping,
                    *slot as usize,
                ),
                DdlUndo::UserMappingAltered { slot, .. } => {
                    self.storage.foreign_catalog_commit_alter(
                        crate::storage::foreign::ForeignObjectClass::Mapping,
                        *slot as usize,
                        txn.txid,
                    )
                }
                DdlUndo::UserMappingDropped(slot) => self.storage.foreign_catalog_commit_drop(
                    crate::storage::foreign::ForeignObjectClass::Mapping,
                    *slot as usize,
                ),
                DdlUndo::ForeignTableCreated(slot) => self.storage.foreign_catalog_commit_create(
                    crate::storage::foreign::ForeignObjectClass::Table,
                    *slot as usize,
                ),
                DdlUndo::ForeignTableAltered { slot, .. } => {
                    self.storage.foreign_catalog_commit_alter(
                        crate::storage::foreign::ForeignObjectClass::Table,
                        *slot as usize,
                        txn.txid,
                    )
                }
                DdlUndo::ForeignTableDropped(slot) => self.storage.foreign_catalog_commit_drop(
                    crate::storage::foreign::ForeignObjectClass::Table,
                    *slot as usize,
                ),
                DdlUndo::ForeignOwnerChanged { class, slot, .. } => self
                    .storage
                    .commit_foreign_catalog_owner(*class, *slot as usize, txn.txid),
                DdlUndo::SubscriptionCreated(slot) => {
                    self.storage.commit_subscription_create(*slot as usize)
                }
                DdlUndo::SubscriptionDropped(slot) => {
                    self.storage.commit_subscription_drop(*slot as usize)
                }
                DdlUndo::SubscriptionEnabled { slot, .. } => self
                    .storage
                    .commit_subscription_enabled(*slot as usize, txn.txid),
                DdlUndo::SubscriptionBootstrapChanged { slot, .. } => self
                    .storage
                    .commit_subscription_bootstrap(*slot as usize, txn.txid),
                DdlUndo::SubscriptionRelationsChanged => {
                    self.storage.commit_subscription_relation_refresh(txn.txid)
                }
                DdlUndo::SubscriptionDefinitionChanged { slot, .. } => self
                    .storage
                    .commit_subscription_definition(*slot as usize, txn.txid),
                DdlUndo::SubscriptionOwnerChanged { slot, .. } => self
                    .storage
                    .commit_subscription_owner(*slot as usize, txn.txid),
                DdlUndo::SubscriptionRenamed { slot, .. } => self
                    .storage
                    .commit_subscription_rename(*slot as usize, txn.txid),
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
                DdlUndo::LargeObjectCreated(slot) => {
                    self.storage.commit_large_object_create(*slot as usize);
                    self.storage.commit_object_owner(
                        crate::storage::AccessObject {
                            class: crate::storage::AccessClass::LargeObject,
                            slot: *slot as u16,
                        },
                        txn.txid,
                    );
                }
                DdlUndo::LargeObjectDropped(slot) => {
                    self.storage.commit_large_object_drop(*slot as usize)
                }
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
                DdlUndo::CompositeCreated(slot) => {
                    self.storage.commit_composite_create(*slot as usize);
                    self.storage.commit_object_owner(
                        crate::storage::AccessObject {
                            class: crate::storage::AccessClass::Composite,
                            slot: *slot as u16,
                        },
                        txn.txid,
                    );
                }
                DdlUndo::CompositeAltered { slot, .. } => self
                    .storage
                    .commit_composite_alter(*slot as usize, txn.txid),
                DdlUndo::CompositeDropped(slot) => {
                    self.storage.commit_composite_drop(*slot as usize)
                }
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
                DdlUndo::IndexAltered { slot, .. } => self
                    .storage
                    .commit_index_definition(*slot as usize, txn.txid),
                DdlUndo::TablespaceCreated(slot) => {
                    self.storage.commit_tablespace_create(*slot as usize)
                }
                DdlUndo::TablespaceAltered { slot, .. } => self
                    .storage
                    .commit_tablespace_alter(*slot as usize, txn.txid),
                DdlUndo::TablespaceDropped(slot) => {
                    self.storage.commit_tablespace_drop(*slot as usize)
                }
                DdlUndo::AccessMethodCreated(slot) => {
                    self.storage.commit_access_method_create(*slot as usize)
                }
                DdlUndo::AccessMethodDropped(slot) => {
                    self.storage.commit_access_method_drop(*slot as usize)
                }
                DdlUndo::DatabaseCreated(slot) => {
                    self.storage.commit_database_create(*slot as usize)
                }
                DdlUndo::DatabaseAltered { slot, .. } => {
                    self.storage.commit_database_alter(*slot as usize, txn.txid)
                }
                DdlUndo::DatabaseDropped(slot) => self.storage.commit_database_drop(*slot as usize),
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
                DdlUndo::SchemaRenamed { .. } => {}
                DdlUndo::ExtensionCreated(slot) => self
                    .storage
                    .commit_extension_create(*slot as usize, txn.txid),
                DdlUndo::ExtensionDropped(slot) => {
                    self.storage.commit_extension_drop(*slot as usize)
                }
                DdlUndo::ExtensionAltered { slot, .. } => self
                    .storage
                    .commit_extension_alter(*slot as usize, txn.txid),
                DdlUndo::ExtensionDependencyChanged { slot, .. } => self
                    .storage
                    .commit_extension_dependency(*slot as usize, txn.txid),
                DdlUndo::ExtensionConfigChanged { slot, .. } => self
                    .storage
                    .commit_extension_config(*slot as usize, txn.txid),
                DdlUndo::RoleChanged { slot, .. } => {
                    self.storage.commit_role_change(*slot as usize);
                }
                DdlUndo::RoleMembershipChanged { slot, .. } => {
                    self.storage.commit_role_membership_change(*slot as usize);
                }
                DdlUndo::RoleSettingChanged { slot, .. } => {
                    self.storage.commit_role_setting(*slot as usize);
                }
                DdlUndo::SystemSettingChanged { slot, .. } => {
                    self.storage.commit_system_setting(*slot as usize);
                }
                DdlUndo::ObjectOwnerChanged { object, .. } => {
                    self.storage.commit_object_owner(*object, txn.txid);
                }
                DdlUndo::ObjectAclChanged { slot, .. } => {
                    self.storage.commit_acl(*slot as usize, txn.txid);
                }
                DdlUndo::ColumnAclChanged { slot, .. } => {
                    self.storage.commit_column_acl(*slot as usize, txn.txid);
                }
                DdlUndo::DefaultAclChanged { slot, .. } => {
                    self.storage.commit_default_acl(*slot as usize, txn.txid);
                }
                DdlUndo::ParameterAclChanged { slot, .. } => {
                    self.storage.commit_parameter_acl(*slot as usize, txn.txid);
                }
                // Promote the uncommitted comment overlay to committed; its WAL
                // record was journaled at exec time (like other DDL).
                DdlUndo::CommentSet { slot, .. } => {
                    self.storage.commit_comment(*slot as usize, txn.txid);
                }
                DdlUndo::ConstraintCommentRenamed { .. } => {}
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
        for slot in 0..self.storage.extended_statistics_count() {
            self.storage.commit_extended_statistics_data(slot, txn.txid);
        }
        // Past the durability point, so these fire iff the transaction really
        // committed: apply its LISTEN/UNLISTEN to the shared registry and move
        // its notifications into the delivery outbox. A pool-exhaustion here is
        // a loud error reported to the client — like a post-commit upload
        // failure, the data is committed regardless — never a silent drop.
        let notify_result = self.flush_committed_notifications(txn);
        if let Some(guc) = guc {
            guc.commit_transaction();
        }
        txn.clear();
        notify_result.and(index_result)
    }

    pub(crate) fn commit_txn_with_triggers(
        &mut self,
        txn: &mut TxnState,
        guc: &GucState,
        arena: &Arena,
        responder: &mut Responder,
    ) -> Result<(), SqlError> {
        self.fire_constraint_trigger_boundary(
            txn,
            guc,
            arena,
            responder,
            exec::TriggerQueueBoundary::Transaction,
        )?;
        self.commit_txn(txn, guc)
    }

    fn refresh_prepared_transaction_catalog(&mut self) {
        self.storage.replace_prepared_transaction_catalog(
            self.prepared_transactions.entries().map(|(_, metadata)| {
                crate::storage::PreparedTransactionCatalogEntry {
                    transaction_id: metadata.transaction_id,
                    gid: crate::util::StackStr::from_str(metadata.gid.as_str()),
                    prepared_at: metadata.prepared_at,
                    owner: metadata.owner,
                    database: metadata.database,
                    first_lsn: metadata.first_lsn,
                    prepared_lsn: metadata.prepared_lsn,
                }
            }),
        );
    }

    fn prepare_transaction(
        &mut self,
        gid: ast::PreparedTransactionId,
        txn: &mut TxnState,
        guc: &GucState,
        cursors: &mut cursor::CursorPool,
        arena: &Arena,
        responder: &mut Responder,
    ) -> Result<(), SqlError> {
        let fail =
            |engine: &mut Self, txn: &mut TxnState, cursors: &mut cursor::CursorPool, error| {
                if txn.is_explicit() {
                    engine.rollback_txn(txn, guc);
                    cursors.on_rollback();
                }
                error
            };
        if !txn.is_explicit() {
            return Err(sql_err!(
                sqlstate::NO_ACTIVE_SQL_TRANSACTION,
                "PREPARE TRANSACTION can only be used in transaction blocks"
            ));
        }
        if txn.failed {
            return Err(fail(
                self,
                txn,
                cursors,
                sql_err!(
                    sqlstate::IN_FAILED_SQL_TRANSACTION,
                    "current transaction is aborted, commands ignored until end of transaction block"
                ),
            ));
        }
        if self.prepared_transactions.find(gid).is_some() {
            return Err(fail(
                self,
                txn,
                cursors,
                sql_err!(
                    sqlstate::DUPLICATE_OBJECT,
                    "transaction identifier \"{}\" is already in use",
                    gid.as_str()
                ),
            ));
        }
        if txn.has_session_notification_actions() {
            return Err(fail(
                self,
                txn,
                cursors,
                sql_err!(
                    sqlstate::FEATURE_NOT_SUPPORTED,
                    "cannot PREPARE a transaction that has executed LISTEN, UNLISTEN, or NOTIFY"
                ),
            ));
        }
        if cursors.has_uncommitted_hold_cursor() {
            return Err(fail(
                self,
                txn,
                cursors,
                sql_err!(
                    sqlstate::FEATURE_NOT_SUPPORTED,
                    "cannot PREPARE a transaction that has created a cursor WITH HOLD"
                ),
            ));
        }
        let current = eval::funcs::system::current_user_owned();
        let owner = self
            .storage
            .find_role_visible(current.as_str(), txn.txid)
            .expect("the current role is transaction-visible") as u16;
        let metadata = two_phase::PreparedTransactionMetadata {
            gid,
            transaction_id: txn.txid,
            owner,
            database: self.storage.current_database_oid(),
            prepared_at: datetime::now_micros(),
            first_lsn: 0,
            prepared_lsn: 0,
        };
        let Some(slot) = self.prepared_transactions.reserve(metadata) else {
            return Err(fail(
                self,
                txn,
                cursors,
                sql_err!(
                    sqlstate::OUT_OF_MEMORY,
                    "maximum number of prepared transactions reached"
                ),
            ));
        };
        if let Err(error) = self.fire_constraint_trigger_boundary(
            txn,
            guc,
            arena,
            responder,
            exec::TriggerQueueBoundary::Transaction,
        ) {
            self.prepared_transactions.release(slot);
            return Err(fail(self, txn, cursors, error));
        }
        if let Err(error) = self.storage.encode_transaction_locks(
            txn.txid,
            &mut self.prepared_transactions.slot_mut(slot).locks,
        ) {
            self.prepared_transactions.release(slot);
            return Err(fail(self, txn, cursors, error));
        }
        let lock_lsn = self.storage.lsn() + 1;
        let lock_record = WalOp::PreparedLocks {
            transaction_id: txn.txid,
            encoded: self.prepared_transactions.slot(slot).locks.readable(),
        };
        if let Err(error) = self.wal.stage(txn.txid, lock_lsn, &lock_record) {
            self.prepared_transactions.release(slot);
            return Err(fail(self, txn, cursors, error));
        }
        self.storage.set_lsn(lock_lsn);
        if let Err(error) = self.finish_txn(txn, guc, Some(slot)) {
            self.prepared_transactions.release(slot);
            return Err(error);
        }
        let prepared_lsn = self.storage.lsn();
        let first_lsn = self
            .prepared_transactions
            .slot(slot)
            .first_lsn()
            .expect("a prepared transaction has a database-scope WAL record");
        self.prepared_transactions
            .set_lsn_range(slot, first_lsn, prepared_lsn);
        core::mem::swap(
            txn,
            &mut self.prepared_transactions.slot_mut(slot).transaction,
        );
        txn.clear();
        cursors.on_rollback();
        Ok(())
    }

    fn resolve_prepared_transaction(
        &mut self,
        gid: ast::PreparedTransactionId,
        commit: bool,
        txn: &mut TxnState,
        guc: &GucState,
    ) -> Result<(), SqlError> {
        let Some(slot) = self.prepared_transactions.find(gid) else {
            return Err(sql_err!(
                sqlstate::UNDEFINED_OBJECT,
                "prepared transaction with identifier \"{}\" does not exist",
                gid.as_str()
            ));
        };
        let metadata = self.prepared_transactions.slot(slot).metadata();
        let recovered = self.prepared_transactions.slot(slot).recovered;
        if metadata.database != self.storage.current_database_oid() {
            return Err(sql_err!(
                sqlstate::FEATURE_NOT_SUPPORTED,
                "prepared transaction belongs to another database"
            ));
        }
        let current = eval::funcs::system::current_user_owned();
        let current_role = self
            .storage
            .find_role_visible(current.as_str(), txn.txid)
            .expect("the current role is transaction-visible");
        if current_role != usize::from(metadata.owner)
            && !self
                .storage
                .role(current_role)
                .attributes_to(txn.txid)
                .superuser
        {
            return Err(sql_err!(
                sqlstate::INSUFFICIENT_PRIVILEGE,
                "permission denied to finish prepared transaction"
            ));
        }

        let lsn = self.storage.lsn() + 1;
        let operation = if commit {
            WalOp::CommitPrepared { gid: gid.as_str() }
        } else {
            WalOp::RollbackPrepared { gid: gid.as_str() }
        };
        self.wal.stage(txn.txid, lsn, &operation)?;
        self.storage.set_lsn(lsn);
        self.commit_txn(txn, guc)?;
        let resolution_lsn = self.storage.lsn();

        // The resolution record is durable before the detached transaction is
        // released. Recovery therefore reaches the same outcome if the process
        // stops anywhere in the in-memory promotion below.
        core::mem::swap(
            txn,
            &mut self.prepared_transactions.slot_mut(slot).transaction,
        );
        let result = if commit && recovered {
            self.rollback_transaction_state(txn);
            self.prepared_transactions
                .slot(slot)
                .visit_records(|_, raw| {
                    let operation = crate::wal::decode_record(raw).ok_or_else(|| {
                        sql_err!(
                            sqlstate::DATA_EXCEPTION,
                            "prepared transaction WAL is corrupt"
                        )
                    })?;
                    apply_wal_op(&mut self.storage, resolution_lsn, operation)
                })
        } else if commit {
            self.storage.release_snapshot(txn.txid);
            self.storage.release_serializable(txn.txid);
            self.storage.release_table_locks(txn.txid);
            self.storage.release_row_locks(txn.txid);
            self.promote_transaction_state(txn, resolution_lsn, None)
        } else {
            self.rollback_transaction_state(txn);
            Ok(())
        };
        self.prepared_transactions.release(slot);
        result
    }

    fn fire_constraint_trigger_boundary(
        &mut self,
        txn: &mut TxnState,
        guc: &GucState,
        arena: &Arena,
        responder: &mut Responder,
        boundary: exec::TriggerQueueBoundary,
    ) -> Result<(), SqlError> {
        exec::fire_constraint_triggers(
            &mut self.storage,
            txn,
            arena,
            guc.seq_session(),
            responder,
            &mut self.dml_scratch,
            boundary,
        )
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
            DdlUndo::RuleCreated {
                slot,
                prior_table_rule_txid,
            } => self
                .storage
                .rollback_rule_create(slot as usize, prior_table_rule_txid),
            DdlUndo::RuleAltered { slot, prior } => {
                self.storage.rollback_rule_alter(slot as usize, prior)
            }
            DdlUndo::RuleDropped(slot) => self.storage.rollback_rule_drop(slot as usize, txid),
            DdlUndo::RoutineCreated(slot) => self.storage.rollback_routine_create(slot as usize),
            DdlUndo::RoutineDropped(slot) => {
                self.storage.rollback_routine_drop(slot as usize, txid)
            }
            DdlUndo::RoutineReplaced { slot, prior } => {
                self.storage.rollback_routine_replace(slot as usize, prior)
            }
            DdlUndo::CastCreated(slot) => self.storage.rollback_cast_create(slot as usize),
            DdlUndo::CastDropped(slot) => self.storage.rollback_cast_drop(slot as usize, txid),
            DdlUndo::OperatorCreated(slot) => self.storage.rollback_operator_create(slot as usize),
            DdlUndo::CollationCreated(slot) => {
                self.storage.rollback_collation_create(slot as usize)
            }
            DdlUndo::CollationAltered { slot, prior } => {
                self.storage.rollback_collation_alter(slot as usize, prior)
            }
            DdlUndo::CollationDropped(slot) => {
                self.storage.rollback_collation_drop(slot as usize, txid)
            }
            DdlUndo::ConversionCreated(slot) => {
                self.storage.rollback_conversion_create(slot as usize)
            }
            DdlUndo::ConversionAltered { slot, prior } => {
                self.storage.rollback_conversion_alter(slot as usize, prior)
            }
            DdlUndo::ConversionDropped(slot) => {
                self.storage.rollback_conversion_drop(slot as usize, txid)
            }
            DdlUndo::TextSearchCreated(slot) => {
                self.storage.rollback_text_search_create(slot as usize)
            }
            DdlUndo::TextSearchAltered { slot, prior } => self
                .storage
                .rollback_text_search_alter(slot as usize, prior),
            DdlUndo::TextSearchDropped(slot) => {
                self.storage.rollback_text_search_drop(slot as usize, txid)
            }
            DdlUndo::EventTriggerCreated(slot) => {
                self.storage.rollback_event_trigger_create(slot as usize)
            }
            DdlUndo::EventTriggerAltered { slot, prior } => self
                .storage
                .rollback_event_trigger_alter(slot as usize, prior),
            DdlUndo::EventTriggerDropped(slot) => self
                .storage
                .rollback_event_trigger_drop(slot as usize, txid),
            DdlUndo::OperatorAltered { slot, prior } => {
                self.storage.rollback_operator_alter(slot as usize, prior)
            }
            DdlUndo::OperatorDropped(slot) => {
                self.storage.rollback_operator_drop(slot as usize, txid)
            }
            DdlUndo::OperatorFamilyCreated(slot) => {
                self.storage.rollback_operator_family_create(slot as usize)
            }
            DdlUndo::OperatorFamilyAltered { slot, prior } => self
                .storage
                .rollback_operator_family_alter(slot as usize, prior),
            DdlUndo::OperatorFamilyDropped(slot) => self
                .storage
                .rollback_operator_family_drop(slot as usize, txid),
            DdlUndo::OperatorClassCreated(slot) => {
                self.storage.rollback_operator_class_create(slot as usize)
            }
            DdlUndo::OperatorClassAltered { slot, prior } => self
                .storage
                .rollback_operator_class_alter(slot as usize, prior),
            DdlUndo::OperatorClassDropped(slot) => self
                .storage
                .rollback_operator_class_drop(slot as usize, txid),
            DdlUndo::TriggerCreated(slot) => self.storage.rollback_trigger_create(slot as usize),
            DdlUndo::TriggerDropped(slot) => {
                self.storage.rollback_trigger_drop(slot as usize, txid)
            }
            DdlUndo::TriggerAltered { slot, prior } => {
                self.storage.rollback_trigger_alter(slot as usize, prior)
            }
            DdlUndo::PartitionTriggerAltered { slot, prior } => self
                .storage
                .rollback_partition_trigger_state(slot as usize, prior),
            DdlUndo::PolicyCreated(slot) => self.storage.rollback_policy_create(slot as usize),
            DdlUndo::PolicyDropped(slot) => self.storage.rollback_policy_drop(slot as usize, txid),
            DdlUndo::PolicyAltered { slot, prior } => {
                self.storage.rollback_policy_alter(slot as usize, prior)
            }
            DdlUndo::StatisticsCreated(slot) => self
                .storage
                .rollback_extended_statistics_create(slot as usize),
            DdlUndo::StatisticsDropped(slot) => self
                .storage
                .rollback_extended_statistics_drop(slot as usize, txid),
            DdlUndo::StatisticsAltered { slot, prior } => self
                .storage
                .rollback_extended_statistics_alter(slot as usize, prior),
            DdlUndo::StatisticsKeysAltered { slot, prior } => self
                .storage
                .rollback_extended_statistics_keys(slot as usize, prior),
            DdlUndo::RoutineIdentityAltered { slot, prior } => {
                self.storage.restore_routine_identity(slot as usize, prior)
            }
            DdlUndo::ViewDropped(slot) => {
                self.storage.rollback_view_drop(slot as usize, txid);
            }
            DdlUndo::ViewSchemaChanged { slot, prior } => {
                self.storage.rollback_view_schema(slot as usize, prior)
            }
            DdlUndo::ViewRenamed { slot, prior } => {
                self.storage.rollback_view_rename(slot as usize, prior)
            }
            DdlUndo::ViewOptionsChanged { slot, prior } => {
                self.storage.rollback_view_options(slot as usize, prior)
            }
            DdlUndo::ViewColumnsChanged { slot, prior } => {
                self.storage.rollback_view_columns(slot as usize, prior)
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
            DdlUndo::ForeignDataWrapperCreated(slot) => {
                self.storage.foreign_catalog_rollback_create(
                    crate::storage::foreign::ForeignObjectClass::Wrapper,
                    slot as usize,
                )
            }
            DdlUndo::ForeignDataWrapperAltered { slot, prior } => self
                .storage
                .rollback_foreign_wrapper_alter(slot as usize, prior),
            DdlUndo::ForeignDataWrapperDropped(slot) => self.storage.foreign_catalog_rollback_drop(
                crate::storage::foreign::ForeignObjectClass::Wrapper,
                slot as usize,
                txid,
            ),
            DdlUndo::ForeignServerCreated(slot) => self.storage.foreign_catalog_rollback_create(
                crate::storage::foreign::ForeignObjectClass::Server,
                slot as usize,
            ),
            DdlUndo::ForeignServerAltered { slot, prior } => self
                .storage
                .rollback_foreign_server_alter(slot as usize, prior),
            DdlUndo::ForeignServerDropped(slot) => self.storage.foreign_catalog_rollback_drop(
                crate::storage::foreign::ForeignObjectClass::Server,
                slot as usize,
                txid,
            ),
            DdlUndo::UserMappingCreated(slot) => self.storage.foreign_catalog_rollback_create(
                crate::storage::foreign::ForeignObjectClass::Mapping,
                slot as usize,
            ),
            DdlUndo::UserMappingAltered { slot, prior } => self
                .storage
                .rollback_foreign_mapping_alter(slot as usize, prior),
            DdlUndo::UserMappingDropped(slot) => self.storage.foreign_catalog_rollback_drop(
                crate::storage::foreign::ForeignObjectClass::Mapping,
                slot as usize,
                txid,
            ),
            DdlUndo::ForeignTableCreated(slot) => self.storage.foreign_catalog_rollback_create(
                crate::storage::foreign::ForeignObjectClass::Table,
                slot as usize,
            ),
            DdlUndo::ForeignTableAltered { slot, prior } => self
                .storage
                .rollback_foreign_table_alter(slot as usize, prior),
            DdlUndo::ForeignTableDropped(slot) => self.storage.foreign_catalog_rollback_drop(
                crate::storage::foreign::ForeignObjectClass::Table,
                slot as usize,
                txid,
            ),
            DdlUndo::ForeignOwnerChanged { class, slot, prior } => self
                .storage
                .rollback_foreign_catalog_owner(class, slot as usize, prior),
            DdlUndo::SubscriptionCreated(slot) => {
                self.storage.rollback_subscription_create(slot as usize)
            }
            DdlUndo::SubscriptionDropped(slot) => {
                self.storage.rollback_subscription_drop(slot as usize, txid)
            }
            DdlUndo::SubscriptionEnabled { slot, prior } => self
                .storage
                .restore_subscription_enabled(slot as usize, prior),
            DdlUndo::SubscriptionBootstrapChanged { slot, prior } => self
                .storage
                .restore_subscription_bootstrap(slot as usize, prior),
            DdlUndo::SubscriptionRelationsChanged => {
                self.storage.rollback_subscription_relation_refresh(txid)
            }
            DdlUndo::SubscriptionDefinitionChanged { slot, prior } => self
                .storage
                .restore_subscription_definition(slot as usize, prior),
            DdlUndo::SubscriptionOwnerChanged { slot, prior } => self
                .storage
                .restore_subscription_owner_pending(slot as usize, prior),
            DdlUndo::SubscriptionRenamed { slot, prior } => self
                .storage
                .rollback_subscription_rename(slot as usize, prior),
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
            DdlUndo::LargeObjectCreated(slot) => {
                self.storage.rollback_large_object_create(slot as usize)
            }
            DdlUndo::LargeObjectDropped(slot) => {
                self.storage.rollback_large_object_drop(slot as usize, txid)
            }
            DdlUndo::DomainCreated(slot) => self.storage.rollback_domain_create(slot as usize),
            DdlUndo::DomainDropped(slot) => {
                self.storage.rollback_domain_drop(slot as usize, txid);
            }
            DdlUndo::DomainAltered { slot, prior } => {
                self.storage.rollback_domain_alter(slot as usize, prior)
            }
            DdlUndo::EnumCreated(slot) => self.storage.rollback_enum_create(slot as usize),
            DdlUndo::CompositeCreated(slot) => {
                self.storage.rollback_composite_create(slot as usize)
            }
            DdlUndo::CompositeAltered { slot, prior } => {
                self.storage.rollback_composite_alter(slot as usize, prior)
            }
            DdlUndo::CompositeDropped(slot) => {
                self.storage.rollback_composite_drop(slot as usize, txid)
            }
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
            DdlUndo::IndexAltered { slot, prior } => {
                self.storage.rollback_index_definition(slot as usize, prior)
            }
            DdlUndo::TablespaceCreated(slot) => {
                self.storage.rollback_tablespace_create(slot as usize)
            }
            DdlUndo::TablespaceAltered {
                slot,
                prior_definition,
                prior_owner,
            } => {
                let object = crate::storage::AccessObject {
                    class: crate::storage::AccessClass::Tablespace,
                    slot: slot as u16,
                };
                self.storage
                    .rollback_tablespace_alter(slot as usize, prior_definition);
                self.storage.restore_object_owner(object, prior_owner);
            }
            DdlUndo::TablespaceDropped(slot) => {
                self.storage.rollback_tablespace_drop(slot as usize, txid)
            }
            DdlUndo::AccessMethodCreated(slot) => {
                self.storage.rollback_access_method_create(slot as usize)
            }
            DdlUndo::AccessMethodDropped(slot) => self
                .storage
                .rollback_access_method_drop(slot as usize, txid),
            DdlUndo::DatabaseCreated(slot) => self.storage.rollback_database_create(slot as usize),
            DdlUndo::DatabaseAltered {
                slot,
                prior_definition,
                prior_owner,
            } => {
                let object = crate::storage::AccessObject {
                    class: crate::storage::AccessClass::Database,
                    slot: slot as u16,
                };
                self.storage
                    .rollback_database_alter(slot as usize, prior_definition);
                self.storage.restore_object_owner(object, prior_owner);
            }
            DdlUndo::DatabaseDropped(slot) => {
                self.storage.rollback_database_drop(slot as usize, txid)
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
            DdlUndo::SchemaRenamed { slot, prior } => {
                let _ = self.storage.rename_schema(slot as usize, prior);
            }
            DdlUndo::ExtensionCreated(slot) => {
                self.storage.rollback_extension_create(slot as usize)
            }
            DdlUndo::ExtensionDropped(slot) => {
                self.storage.rollback_extension_drop(slot as usize, txid)
            }
            DdlUndo::ExtensionAltered { slot, prior } => {
                self.storage.rollback_extension_alter(slot as usize, prior)
            }
            DdlUndo::ExtensionDependencyChanged { slot, prior } => self
                .storage
                .rollback_extension_dependency(slot as usize, prior),
            DdlUndo::ExtensionConfigChanged { slot, prior } => {
                self.storage.rollback_extension_config(slot as usize, prior)
            }
            DdlUndo::RoleChanged { slot, prior } => {
                self.storage.rollback_role_change(slot as usize, prior);
            }
            DdlUndo::RoleMembershipChanged { slot, prior } => {
                self.storage
                    .rollback_role_membership_change(slot as usize, prior);
            }
            DdlUndo::RoleSettingChanged { slot, prior } => {
                self.storage.rollback_role_setting(slot as usize, prior);
            }
            DdlUndo::SystemSettingChanged { slot, prior } => {
                self.storage.rollback_system_setting(slot as usize, prior);
            }
            DdlUndo::ObjectOwnerChanged { object, prior } => {
                self.storage.restore_object_owner(object, prior);
            }
            DdlUndo::ObjectAclChanged { slot, prior } => {
                self.storage.restore_acl_pending(slot as usize, prior);
            }
            DdlUndo::ColumnAclChanged { slot, prior } => {
                self.storage
                    .restore_column_acl_pending(slot as usize, prior);
            }
            DdlUndo::DefaultAclChanged { slot, prior } => {
                self.storage
                    .restore_default_acl_pending(slot as usize, prior);
            }
            DdlUndo::ParameterAclChanged { slot, prior } => {
                self.storage
                    .restore_parameter_acl_pending(slot as usize, prior);
            }
            DdlUndo::CommentSet { slot, prior } => {
                self.storage.restore_comment_pending(slot as usize, prior);
            }
            DdlUndo::ConstraintCommentRenamed { slot, prior } => {
                self.storage
                    .restore_comment_identity_pending(slot as usize, prior);
            }
        }
    }

    /// Discards every uncommitted change and journal byte of the
    /// transaction.
    fn rollback_transaction_state(&mut self, txn: &mut TxnState) {
        if txn.txid == 0 {
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
            match *undo {
                StatisticsUndo::Table(table) => self
                    .storage
                    .rollback_table_statistics(table as usize, txn.txid),
                StatisticsUndo::Extended(statistics) => self
                    .storage
                    .rollback_extended_statistics_data(statistics as usize, txn.txid),
            }
        }
        self.wal.discard_stage(txn.txid);
        txn.clear();
    }

    pub fn rollback_txn(&mut self, txn: &mut TxnState, guc: &GucState) {
        // The next statement starts a fresh transaction clock.
        datetime::end_transaction();
        if txn.txid == 0 {
            guc.rollback_transaction();
            txn.clear();
            return;
        }
        self.rollback_transaction_state(txn);
        guc.rollback_transaction();
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
        assert!(
            txn.owns_statement_mark(mark),
            "a statement rewind cannot cross a transaction boundary"
        );
        for index in (mark.touched..txn.touched().len()).rev() {
            let (table, rowid, prior) = txn.touched()[index];
            self.storage
                .restore_pending(table as usize, rowid, txn.txid, prior);
        }
        for index in (mark.ddl..txn.ddl().len()).rev() {
            self.rollback_ddl(txn.ddl()[index], txn.txid);
        }
        for index in (mark.statistics..txn.statistics_undo().len()).rev() {
            match txn.statistics_undo()[index] {
                StatisticsUndo::Table(table) => self
                    .storage
                    .rollback_table_statistics(table as usize, txn.txid),
                StatisticsUndo::Extended(statistics) => self
                    .storage
                    .rollback_extended_statistics_data(statistics as usize, txn.txid),
            }
        }
        txn.rewind_touched(mark.touched);
        txn.rewind_truncates(mark.truncates);
        txn.rewind_ddl(mark.ddl);
        txn.rewind_statistics(mark.statistics);
        txn.rewind_subscription_advances(mark.subscription_advances);
        txn.rewind_constraints(
            mark.constraint_obligations,
            mark.constraint_completions,
            mark.constraint_modes,
            mark.constraint_renames,
            mark.deferred_triggers,
            mark.deferred_trigger_completions,
            mark.deferred_trigger_bytes,
        );
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
            match txn.statistics_undo()[i] {
                StatisticsUndo::Table(table) => self
                    .storage
                    .rollback_table_statistics(table as usize, txn.txid),
                StatisticsUndo::Extended(statistics) => self
                    .storage
                    .rollback_extended_statistics_data(statistics as usize, txn.txid),
            }
        }
        txn.rewind_touched(sp.touched_mark);
        txn.rewind_truncates(sp.truncate_mark);
        txn.rewind_ddl(sp.ddl_mark);
        txn.rewind_statistics(sp.statistics_mark);
        txn.rewind_subscription_advances(sp.subscription_advance_mark);
        txn.rewind_constraints(
            sp.constraint_obligation_mark,
            sp.constraint_completion_mark,
            sp.constraint_mode_mark,
            sp.constraint_rename_mark,
            sp.deferred_trigger_mark,
            sp.deferred_trigger_completion_mark,
            sp.deferred_trigger_bytes_mark,
        );
        txn.rewind_notifications(sp.notify_mark, sp.notify_payload_mark, sp.listen_mark);
        self.storage.rollback_locks_to(txn.txid, sp.lock_mark);
        txn.rollback_savepoints_after(index);
        self.wal.truncate_stage(txn.txid, sp.wal_mark);
        guc.rollback_to_savepoint(index);
        txn.read_only = sp.read_only;
        txn.restore_read_only_source(sp.read_only_source);
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
                sqlstate: SqlState::known(sqlstate::IO_ERROR),
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
        mode: ast::TableLockMode,
    ) -> Result<(), SqlError> {
        if targets.is_empty() {
            for slot in 0..self.storage.table_count() {
                if self.storage.table(slot).visible_to(txid) {
                    self.storage.lock_table(txid, slot, mode, false)?;
                }
            }
            return Ok(());
        }
        for target in targets {
            let slot = exec::resolve_dml_table(&self.storage, &target.table, txid)?;
            self.storage.lock_table(txid, slot, mode, false)?;
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
                    exec::analyze_extended_statistics(
                        &mut self.storage,
                        txn,
                        slot,
                        &[],
                        &mut self.work,
                    )?;
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
            exec::analyze_extended_statistics(
                &mut self.storage,
                txn,
                slot,
                &selected[..selected_count],
                &mut self.work,
            )?;
        }
        Ok(total_rows)
    }

    pub(crate) fn execute_checkpoint_statement(
        &mut self,
        responder: &mut Responder,
    ) -> Result<Result<(), SqlError>, WireFull> {
        match self.checkpoint() {
            Ok(_) => {
                responder.command_complete("CHECKPOINT")?;
                Ok(Ok(()))
            }
            Err(error) => Ok(Err(error)),
        }
    }

    pub(crate) fn execute_vacuum_statement(
        &mut self,
        targets: &[ast::MaintenanceTarget<'_>],
        options: ast::VacuumOptions,
        txn: &mut TxnState,
        responder: &mut Responder,
    ) -> Result<Result<(), SqlError>, WireFull> {
        let mode = if options.full {
            ast::TableLockMode::AccessExclusive
        } else {
            ast::TableLockMode::ShareUpdateExclusive
        };
        if let Err(error) = self.lock_maintenance_targets(targets, txn.txid, mode) {
            return Ok(Err(error));
        }
        let validation = if options.analyze {
            self.analyze_targets(targets, txn).map(|_| ())
        } else {
            self.validate_maintenance_targets(targets, txn.txid)
        };
        if let Err(error) = validation {
            return Ok(Err(error));
        }
        if self.ckpt.is_some()
            && let Err(error) = self.checkpoint()
        {
            return Ok(Err(error));
        }
        responder.command_complete("VACUUM")?;
        Ok(Ok(()))
    }

    pub(crate) fn execute_analyze_statement(
        &mut self,
        targets: &[ast::MaintenanceTarget<'_>],
        txn: &mut TxnState,
        responder: &mut Responder,
    ) -> Result<Result<(), SqlError>, WireFull> {
        if let Err(error) = self.lock_maintenance_targets(
            targets,
            txn.txid,
            ast::TableLockMode::ShareUpdateExclusive,
        ) {
            return Ok(Err(error));
        }
        if let Err(error) = self.analyze_targets(targets, txn) {
            return Ok(Err(error));
        }
        responder.command_complete("ANALYZE")?;
        Ok(Ok(()))
    }

    pub(crate) fn execute_listen_statement(
        &mut self,
        channel: &str,
        txn: &mut TxnState,
        responder: &mut Responder,
    ) -> Result<Result<(), SqlError>, WireFull> {
        let operation = notify::ListenOp::Listen {
            conn_id: self.current_conn_id,
            channel: notify::channel(channel),
        };
        if let Err(error) = txn.buffer_listen_op(operation) {
            return Ok(Err(error));
        }
        responder.command_complete("LISTEN")?;
        Ok(Ok(()))
    }

    pub(crate) fn execute_unlisten_statement(
        &mut self,
        channel: Option<&str>,
        txn: &mut TxnState,
        responder: &mut Responder,
    ) -> Result<Result<(), SqlError>, WireFull> {
        let operation = match channel {
            Some(name) => notify::ListenOp::Unlisten {
                conn_id: self.current_conn_id,
                channel: notify::channel(name),
            },
            None => notify::ListenOp::UnlistenAll {
                conn_id: self.current_conn_id,
            },
        };
        if let Err(error) = txn.buffer_listen_op(operation) {
            return Ok(Err(error));
        }
        responder.command_complete("UNLISTEN")?;
        Ok(Ok(()))
    }

    pub(crate) fn execute_notify_statement(
        &mut self,
        channel: &str,
        text: Option<&str>,
        txn: &mut TxnState,
        responder: &mut Responder,
    ) -> Result<Result<(), SqlError>, WireFull> {
        let payload = match text {
            Some(text) => match notify::payload(text) {
                Ok(payload) => payload,
                Err(error) => return Ok(Err(error)),
            },
            None => notify::Payload::new(),
        };
        if let Err(error) = txn.buffer_notify(
            self.current_conn_id,
            notify::channel(channel),
            payload.as_str(),
        ) {
            return Ok(Err(error));
        }
        responder.command_complete("NOTIFY")?;
        Ok(Ok(()))
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn execute_set_statement(
        &mut self,
        name: &str,
        value: &str,
        local: bool,
        syntax: ast::SettingSyntax,
        txn: &mut TxnState,
        guc: &GucState,
        arena: &Arena,
        responder: &mut Responder,
    ) -> Result<Result<(), SqlError>, WireFull> {
        if crate::sql::guc::requires_set_privilege(name)
            && self
                .storage
                .current_role_slot(txn.txid)
                .is_some_and(|role| {
                    !self.storage.role(role).attributes_to(txn.txid).superuser
                        && crate::sql::ast::ParameterName::parse(name).is_none_or(|parameter| {
                            !self.storage.has_parameter_privilege(
                                parameter,
                                role,
                                crate::sql::ast::ParameterPrivileges::SET,
                                txn.txid,
                            )
                        })
                })
        {
            return Ok(Err(sql_err!(
                sqlstate::INSUFFICIENT_PRIVILEGE,
                "permission denied to set parameter \"{}\"",
                name
            )));
        }
        if local && !txn.is_explicit() {
            responder.warning(
                sqlstate::NO_ACTIVE_SQL_TRANSACTION,
                "SET LOCAL can only be used in transaction blocks",
            )?;
        }
        if syntax == ast::SettingSyntax::FromCurrent {
            if let Some(characteristics) = guc.current_transaction_setting_from_current(
                name,
                txn.isolation,
                txn.read_only,
                txn.deferrable,
            ) {
                let characteristics = match characteristics {
                    Ok(characteristics) => characteristics,
                    Err(error) => return Ok(Err(error)),
                };
                if let Err(error) = apply_current_transaction_setting(txn, characteristics) {
                    return Ok(Err(error));
                }
            } else if let Err(error) = guc.set_from_current(name, local) {
                return Ok(Err(error));
            }
            guc::publish_active_setting(guc, name);
            responder.command_complete("SET")?;
            return Ok(Ok(()));
        }
        if let Some(characteristics) = guc.current_transaction_setting(name, value) {
            let characteristics = match characteristics {
                Ok(characteristics) => characteristics,
                Err(error) => return Ok(Err(error)),
            };
            if let Err(error) = apply_current_transaction_setting(txn, characteristics) {
                return Ok(Err(error));
            }
            responder.command_complete("SET")?;
            return Ok(Ok(()));
        }
        let changed = match syntax {
            ast::SettingSyntax::Generic => guc.set(name, value, local),
            ast::SettingSyntax::FromCurrent => {
                unreachable!("FROM CURRENT is handled before value application")
            }
            ast::SettingSyntax::TimeZone => guc.set_time_zone_sql(value, local),
            ast::SettingSyntax::TimeZoneInterval(type_mod) => {
                let interval = match datetime::parse_interval(value) {
                    Ok(interval) => interval,
                    Err(error) => return Ok(Err(error)),
                };
                let interval = match exec::apply_typmod(
                    Datum::Interval(interval),
                    types::ColType::Interval,
                    type_mod,
                    arena,
                ) {
                    Ok(Datum::Interval(interval)) => interval,
                    Ok(_) => unreachable!("interval typmod preserves its type"),
                    Err(error) => return Ok(Err(error)),
                };
                guc.set_time_zone_interval(interval, local)
            }
        };
        match changed {
            Ok(()) => {
                if name.eq_ignore_ascii_case("default_tablespace") {
                    let tablespace = guc.default_tablespace();
                    let tablespace_name = tablespace.as_str();
                    if !tablespace_name.is_empty()
                        && !tablespace_name.eq_ignore_ascii_case("pg_default")
                        && !tablespace_name.eq_ignore_ascii_case("pg_global")
                        && self
                            .storage
                            .tablespace_slot(tablespace_name, txn.txid)
                            .is_none()
                    {
                        return Ok(Err(sql_err!(
                            sqlstate::INVALID_PARAMETER_VALUE,
                            "invalid value for parameter \"default_tablespace\": \"{}\": tablespace does not exist",
                            tablespace_name
                        )));
                    }
                }
                guc::publish_active_setting(guc, name);
                responder.set_render(guc.render());
                responder.command_complete("SET")?;
                Ok(Ok(()))
            }
            Err(error) => Ok(Err(error)),
        }
    }

    pub(crate) fn execute_reset_statement(
        &mut self,
        name: Option<&str>,
        txn: &TxnState,
        guc: &GucState,
        responder: &mut Responder,
    ) -> Result<Result<(), SqlError>, WireFull> {
        if name.is_some_and(crate::sql::guc::requires_set_privilege)
            && self
                .storage
                .current_role_slot(txn.txid)
                .is_some_and(|role| {
                    !self.storage.role(role).attributes_to(txn.txid).superuser
                        && name
                            .and_then(crate::sql::ast::ParameterName::parse)
                            .is_none_or(|parameter| {
                                !self.storage.has_parameter_privilege(
                                    parameter,
                                    role,
                                    crate::sql::ast::ParameterPrivileges::SET,
                                    txn.txid,
                                )
                            })
                })
        {
            return Ok(Err(sql_err!(
                sqlstate::INSUFFICIENT_PRIVILEGE,
                "permission denied to set parameter \"{}\"",
                name.unwrap_or("")
            )));
        }
        if let Some(name) = name
            && guc.transaction_reset_owned(name).is_some()
        {
            return Ok(Err(sql_err!(
                sqlstate::FEATURE_NOT_SUPPORTED,
                "parameter \"{}\" cannot be reset",
                name
            )));
        }
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
            Err(error) => Ok(Err(error)),
        }
    }

    pub fn checkpoint(&mut self) -> Result<bool, SqlError> {
        self.retry_pending_wal_upload()?;
        if self.post_publish_cleanup.is_some() {
            self.finish_post_publish_cleanup()?;
        }
        self.refresh_prepared_transaction_catalog();
        let Some(ckpt) = self.ckpt.as_mut() else {
            // The explicit non-durable test mode has no publication target.
            // PostgreSQL still treats CHECKPOINT as a successful maintenance
            // command, so it is a no-op rather than a client-visible error.
            return Ok(false);
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
                    .min(lsn)
                    .min(
                        self.prepared_transactions
                            .entries()
                            .map(|(_, metadata)| metadata.first_lsn.saturating_sub(1))
                            .min()
                            .unwrap_or(lsn),
                    );
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
        self.invalidate_replication_snapshot(conn_id);
    }

    /// One complete COPY data line (no trailing newline).
    pub fn copy_row_line(
        &mut self,
        setup: &exec::CopySetup,
        txn: &mut TxnState,
        seq_session: &guc::SeqSession,
        arena: &Arena,
        responder: &mut Responder,
        line: &[u8],
    ) -> Result<exec::CopyRowOutcome, SqlError> {
        exec::copy_row(
            &mut self.storage,
            txn,
            seq_session,
            setup,
            line,
            arena,
            responder,
            &mut self.dml_scratch,
            &mut self.copy_transition_scratch,
        )
    }

    /// Checks a `COPY ... HEADER MATCH` line through the same decoded field
    /// grammar as the following data rows.
    pub fn copy_match_header(
        &self,
        setup: &exec::CopySetup,
        line: &[u8],
        arena: &Arena,
    ) -> Result<(), SqlError> {
        exec::copy_match_header(&self.storage, setup, line, arena)
    }

    /// One complete COPY FROM binary row (int16 field count + fields).
    pub fn copy_row_binary(
        &mut self,
        setup: &exec::CopySetup,
        txn: &mut TxnState,
        seq_session: &guc::SeqSession,
        arena: &Arena,
        responder: &mut Responder,
        row: &[u8],
    ) -> Result<exec::CopyRowOutcome, SqlError> {
        exec::copy_row_binary(
            &mut self.storage,
            txn,
            seq_session,
            setup,
            row,
            arena,
            responder,
            &mut self.dml_scratch,
            &mut self.copy_transition_scratch,
        )
    }

    pub fn copy_start(
        &mut self,
        setup: &exec::CopySetup,
        txn: &mut TxnState,
        seq_session: &guc::SeqSession,
        arena: &Arena,
        responder: &mut Responder,
    ) -> Result<(), SqlError> {
        self.copy_transition_scratch.clear();
        exec::copy_statement_begin(
            &mut self.storage,
            txn,
            setup,
            seq_session,
            arena,
            responder,
            &mut self.dml_scratch,
        )
    }

    pub(crate) fn subscription_copy_setup(
        &self,
        schema: SqlName,
        table: SqlName,
        columns: &[SqlName],
        txid: u32,
    ) -> Result<exec::CopySetup, SqlError> {
        exec::subscription_copy_setup(&self.storage, schema, table, columns, txid)
    }

    /// Ends a successful COPY FROM: an implicit transaction commits here
    /// (this was the statement's end); an explicit one stays open, exactly
    /// as INSERT inside BEGIN would.
    pub fn copy_finish(
        &mut self,
        setup: &exec::CopySetup,
        txn: &mut TxnState,
        guc: &GucState,
        responder: &mut Responder,
    ) -> Result<(), SqlError> {
        self.work.reset();
        exec::copy_statement_end(
            &mut self.storage,
            txn,
            setup,
            guc.seq_session(),
            &self.work,
            responder,
            &mut self.dml_scratch,
            &self.copy_transition_scratch,
        )?;
        exec::constraints::validate_deferred_constraints(&self.storage, txn, true, &self.work)?;
        exec::fire_constraint_triggers(
            &mut self.storage,
            txn,
            &self.work,
            guc.seq_session(),
            responder,
            &mut self.dml_scratch,
            exec::TriggerQueueBoundary::Statement,
        )?;
        txn.compact_completed_constraints();
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
            let message = stack_format!(
                512,
                "pos3ql: auto-checkpoint failed ({}): {}\n",
                error.sqlstate,
                error.message.as_str()
            );
            crate::util::stderr_line(message.as_str());
            return false;
        }
        if self.post_publish_cleanup.is_some() {
            return match self.finish_post_publish_cleanup() {
                Ok(()) => true,
                Err(e) => {
                    let message = stack_format!(
                        512,
                        "pos3ql: post-checkpoint bookkeeping failed ({}): {}\n",
                        e.sqlstate,
                        e.message.as_str()
                    );
                    crate::util::stderr_line(message.as_str());
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
                    let message = stack_format!(
                        512,
                        "pos3ql: post-checkpoint bookkeeping failed ({}): {}\n",
                        e.sqlstate,
                        e.message.as_str()
                    );
                    crate::util::stderr_line(message.as_str());
                    return false;
                }
                true
            }
            Ok(_) => true,
            Err(e) => {
                let message = stack_format!(
                    512,
                    "pos3ql: auto-checkpoint failed ({}): {}\n",
                    e.sqlstate,
                    e.message.as_str()
                );
                crate::util::stderr_line(message.as_str());
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
        // One Query message owns an implicit transaction until an explicit
        // BEGIN boundary. PostgreSQL commits preceding simple-query commands
        // before opening that block, so ROLLBACK cannot erase them.
        // Freeze this statement's clock before anything anchors a transaction
        // to it, so `now()` and `statement_timestamp()` agree on a lone
        // statement as they do in PostgreSQL.
        datetime::begin_statement();
        self.ensure_txn(txn, TxnMode::Implicit, guc);
        let mut statements = [None; parser::MAX_LIST];
        let mut statement_count = 0usize;
        loop {
            match parser.next_stmt() {
                Ok(Some(statement)) => {
                    if statement_count == statements.len() {
                        let error = sql_err!(
                            sqlstate::PROGRAM_LIMIT_EXCEEDED,
                            "query string has too many statements"
                        );
                        if txn.is_explicit() {
                            txn.failed = true;
                        } else {
                            self.rollback_txn(txn, guc);
                        }
                        responder.error(error.sqlstate, error.message.as_str())?;
                        return Ok(ExecutionStatus::Complete);
                    }
                    statements[statement_count] = Some(statement);
                    statement_count += 1;
                }
                Ok(None) => break,
                Err(error) => {
                    if txn.is_explicit() {
                        txn.failed = true;
                    } else {
                        self.rollback_txn(txn, guc);
                    }
                    report_parse_error(responder, &error)?;
                    return Ok(ExecutionStatus::Complete);
                }
            }
        }
        emit_parse_warnings(&mut parser, responder)?;
        let routine_transaction_context = if !txn.is_explicit() && statement_count == 1 {
            exec::PlpgsqlTransactionContext::NonAtomic
        } else {
            exec::PlpgsqlTransactionContext::Atomic
        };
        let mut executed_any = resume_statement > 0;
        for (statement_index, statement) in statements[..statement_count]
            .iter()
            .enumerate()
            .skip(resume_statement)
        {
            let statement = statement.as_ref().expect("parsed query statement");
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
            if matches!(statement, Stmt::Begin(_))
                && txn.mode == TxnMode::Implicit
                && let Err(error) = self.commit_txn(txn, guc)
            {
                responder.error(error.sqlstate, error.message.as_str())?;
                return Ok(ExecutionStatus::Complete);
            }
            executed_any = true;
            let output_mark = responder.buffer.mark();
            let statement_mark =
                txn.statement_mark(self.wal.stage_mark(txn.txid), self.storage.lock_mark());
            let outcome = self.execute_stmt(
                statement,
                arena,
                NO_PARAMS,
                txn,
                sqlprep,
                cursors,
                guc,
                routine_transaction_context,
                responder,
            )?;
            let outcome = outcome
                .and_then(|()| {
                    exec::constraints::validate_deferred_constraints(
                        &self.storage,
                        txn,
                        true,
                        arena,
                    )
                })
                .and_then(|()| {
                    self.fire_constraint_trigger_boundary(
                        txn,
                        guc,
                        arena,
                        responder,
                        exec::TriggerQueueBoundary::Statement,
                    )
                })
                .and_then(|()| query::check_timeout());
            if let Err(mut e) = outcome {
                if e.sqlstate == sqlstate::INTERNAL_LOCK_WAIT
                    || e.sqlstate == sqlstate::INTERNAL_IO_WAIT
                {
                    if txn.owns_statement_mark(statement_mark) {
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
                    } else {
                        e = sql_err!(
                            sqlstate::FEATURE_NOT_SUPPORTED,
                            "a non-atomic routine cannot suspend after transaction control"
                        );
                    }
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
            if txn.take_concurrent_partition_detach() {
                if let Err(error) = self.commit_txn(txn, guc) {
                    responder.error(error.sqlstate, error.message.as_str())?;
                    return Ok(ExecutionStatus::Complete);
                }
                return Ok(ExecutionStatus::Blocked {
                    completed_statements: statement_index,
                    output_mark,
                    io_wait: false,
                });
            }
            txn.compact_completed_constraints();
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
        parameter_type_oids: &[i32],
        txn: &mut TxnState,
        sqlprep: &mut SqlPreparedPool,
        cursors: &mut cursor::CursorPool,
        guc: &mut GucState,
        responder: &mut Responder,
        conn_id: i32,
        lock_timeout_expired: bool,
    ) -> Result<ExtendedExecutionStatus, WireFull> {
        let _parameter_types = exec::enter_bound_parameter_types(parameter_type_oids);
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
        let routine_transaction_context = if txn.is_active() {
            exec::PlpgsqlTransactionContext::Atomic
        } else {
            exec::PlpgsqlTransactionContext::NonAtomic
        };
        self.ensure_txn(txn, TxnMode::Implicit, guc);
        let statement_mark =
            txn.statement_mark(self.wal.stage_mark(txn.txid), self.storage.lock_mark());
        emit_parse_warnings(&mut parser, responder)?;
        let output_mark = responder.buffer.mark();
        let outcome = match self.execute_stmt(
            &statement,
            arena,
            params,
            txn,
            sqlprep,
            cursors,
            guc,
            routine_transaction_context,
            responder,
        ) {
            Ok(outcome) => outcome,
            Err(WireFull) => {
                if txn.is_explicit() {
                    txn.failed = true;
                } else {
                    self.rollback_txn(txn, guc);
                }
                responder.replace_with_overflow_error(output_mark)?;
                return Ok(ExtendedExecutionStatus::Complete(false));
            }
        }
        .and_then(|()| {
            exec::constraints::validate_deferred_constraints(&self.storage, txn, true, arena)
        })
        .and_then(|()| {
            self.fire_constraint_trigger_boundary(
                txn,
                guc,
                arena,
                responder,
                exec::TriggerQueueBoundary::Statement,
            )
        })
        .and_then(|()| query::check_timeout());
        match outcome {
            Ok(()) => {
                if txn.take_concurrent_partition_detach() {
                    if let Err(error) = self.commit_txn(txn, guc) {
                        responder.error(error.sqlstate, error.message.as_str())?;
                        return Ok(ExtendedExecutionStatus::Complete(false));
                    }
                    return Ok(ExtendedExecutionStatus::Blocked { io_wait: false });
                }
                txn.compact_completed_constraints();
                Ok(ExtendedExecutionStatus::Complete(true))
            }
            Err(mut e) => {
                if e.sqlstate == sqlstate::INTERNAL_LOCK_WAIT
                    || e.sqlstate == sqlstate::INTERNAL_IO_WAIT
                {
                    if txn.owns_statement_mark(statement_mark) {
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
                    } else {
                        e = sql_err!(
                            sqlstate::FEATURE_NOT_SUPPORTED,
                            "a non-atomic routine cannot suspend after transaction control"
                        );
                    }
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

    pub(crate) fn large_object_fast_path_signature(
        function_oid: i32,
    ) -> Option<(&'static [i32], i32)> {
        large_object::fast_path_signature(function_oid)
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn execute_large_object_fast_path(
        &mut self,
        function_oid: i32,
        arguments: &[Datum],
        binary_result: bool,
        arena: &Arena,
        txn: &mut TxnState,
        guc: &GucState,
        responder: &mut Responder,
        conn_id: i32,
    ) -> Result<bool, WireFull> {
        self.current_conn_id = conn_id;
        datetime::begin_statement();
        self.ensure_txn(txn, TxnMode::Implicit, guc);
        if txn.failed {
            responder.error(
                sqlstate::IN_FAILED_SQL_TRANSACTION,
                "current transaction is aborted, commands ignored until end of transaction block",
            )?;
            responder.ready_for_query(txn.status_byte())?;
            return Ok(false);
        }
        if let Err(error) = self.begin_command_snapshot(txn, true) {
            if txn.is_explicit() {
                txn.failed = true;
            } else {
                self.rollback_txn(txn, guc);
            }
            responder.error(error.sqlstate, error.message.as_str())?;
            responder.ready_for_query(txn.status_byte())?;
            return Ok(false);
        }
        let mark = txn.statement_mark(self.wal.stage_mark(txn.txid), self.storage.lock_mark());
        let result = if arguments.iter().any(Datum::is_null) {
            Ok(Datum::Null)
        } else {
            large_object::execute(function_oid, arguments, &mut self.storage, txn, arena)
        };
        let value = match result {
            Ok(value) => value,
            Err(error) => {
                if txn.owns_statement_mark(mark) {
                    self.rollback_waiting_statement(txn, mark);
                }
                if txn.is_explicit() {
                    txn.failed = true;
                } else {
                    self.rollback_txn(txn, guc);
                }
                responder.error(error.sqlstate, error.message.as_str())?;
                responder.ready_for_query(txn.status_byte())?;
                return Ok(false);
            }
        };
        txn.compact_completed_constraints();
        if txn.mode == TxnMode::Implicit
            && let Err(error) = self.commit_txn(txn, guc)
        {
            responder.error(error.sqlstate, error.message.as_str())?;
            responder.ready_for_query(txn.status_byte())?;
            return Ok(false);
        }
        responder.function_call_response(&value, binary_result)?;
        responder.ready_for_query(txn.status_byte())?;
        Ok(true)
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
            self.infer_resolved_stmt_params(&statement, txn.txid, arena, &mut oids);
        }
        // A client's explicit (non-zero) parameter type overrides inference.
        for (i, &c) in client_oids.iter().enumerate().take(MAX_BIND_PARAMS) {
            if c != 0 {
                oids[i] = c;
            }
        }
        oids
    }

    fn infer_resolved_stmt_params<'a>(
        &'a self,
        statement: &'a Stmt<'a>,
        txid: u32,
        arena: &'a Arena,
        oids: &mut [i32; MAX_BIND_PARAMS],
    ) {
        match statement {
            Stmt::Select(select) => self.infer_resolved_select_params(select, txid, arena, oids),
            Stmt::SetQuery(query) => {
                self.infer_resolved_set_tree_params(query.body, txid, arena, oids);
                for cte in query.with {
                    match cte.dml {
                        Some(dml) => self.infer_resolved_stmt_params(dml, txid, arena, oids),
                        None => self.infer_resolved_select_params(cte.query, txid, arena, oids),
                    }
                }
            }
            Stmt::With { ctes, statement } => {
                for cte in *ctes {
                    match cte.dml {
                        Some(dml) => self.infer_resolved_stmt_params(dml, txid, arena, oids),
                        None => self.infer_resolved_select_params(cte.query, txid, arena, oids),
                    }
                }
                self.infer_resolved_stmt_params(statement, txid, arena, oids);
            }
            Stmt::Insert(insert) => {
                if let Some(select) = insert.select {
                    self.infer_resolved_select_params(select, txid, arena, oids);
                }
            }
            Stmt::Call {
                name,
                arguments,
                argument_names,
                variadic,
            } => {
                let mut actual = [types::oid::UNKNOWN; crate::storage::MAX_ROUTINE_ARGUMENTS];
                if arguments.len() > actual.len() {
                    return;
                }
                for (index, argument) in arguments.iter().copied().enumerate() {
                    actual[index] = exec::infer_type_catalog(argument, None, &self.storage, txid)
                        .map_or(types::oid::UNKNOWN, |inferred| inferred.0);
                }
                let qualified = match name.schema {
                    Some(schema) => stack_format!(260, "{}.{}", schema, name.name),
                    None => stack_format!(260, "{}", name.name),
                };
                if let Some(expected) = self.storage.procedure_call_parameter_oids(
                    qualified.as_str(),
                    argument_names,
                    *variadic,
                    &actual[..arguments.len()],
                    txid,
                ) {
                    for (argument, expected_oid) in arguments.iter().copied().zip(expected) {
                        if let Expr::Param(index) = argument
                            && expected_oid != types::oid::UNKNOWN
                            && *index >= 1
                            && (*index as usize) <= MAX_BIND_PARAMS
                        {
                            oids[*index as usize - 1] = expected_oid;
                        }
                    }
                }
            }
            Stmt::Explain { statement, .. } => {
                self.infer_resolved_stmt_params(statement, txid, arena, oids);
            }
            _ => {}
        }
    }

    fn infer_resolved_set_tree_params<'a>(
        &'a self,
        tree: &'a ast::SetTree<'a>,
        txid: u32,
        arena: &'a Arena,
        oids: &mut [i32; MAX_BIND_PARAMS],
    ) {
        match tree {
            ast::SetTree::Select(select) => {
                self.infer_resolved_select_params(select, txid, arena, oids)
            }
            ast::SetTree::Op { left, right, .. } => {
                self.infer_resolved_set_tree_params(left, txid, arena, oids);
                self.infer_resolved_set_tree_params(right, txid, arena, oids);
            }
        }
    }

    fn infer_resolved_select_params<'a>(
        &'a self,
        select: &'a ast::Select<'a>,
        txid: u32,
        arena: &'a Arena,
        oids: &mut [i32; MAX_BIND_PARAMS],
    ) {
        if let Some(tree) = select.set_body {
            self.infer_resolved_set_tree_params(tree, txid, arena, oids);
        }
        for cte in select.with {
            match cte.dml {
                Some(dml) => self.infer_resolved_stmt_params(dml, txid, arena, oids),
                None => self.infer_resolved_select_params(cte.query, txid, arena, oids),
            }
        }

        let scope = match select.from.as_ref() {
            Some(from) => match query::QueryScope::resolve_schema(&self.storage, from, txid, arena)
            {
                Ok(scope) => Some(scope),
                Err(_) => return,
            },
            None => None,
        };
        let resolver = scope.as_ref().map(|scope| query::CatalogScopeCols {
            scope,
            outer_scope: None,
            storage: &self.storage,
            txid,
        });
        let infer = |expression: &Expr<'a>| {
            let inferred = match &resolver {
                Some(resolver) => exec::infer_type_res(expression, resolver),
                None => exec::infer_type_catalog(expression, None, &self.storage, txid),
            }
            .ok()?
            .0;
            (inferred != types::oid::UNKNOWN).then_some(inferred)
        };

        let mut visit =
            |expression| self.infer_expression_params(expression, txid, arena, oids, &infer);
        for item in select.items {
            match item {
                ast::SelectItem::Expr { expression, .. }
                | ast::SelectItem::RecordStar(expression) => visit(expression),
                ast::SelectItem::Wildcard | ast::SelectItem::TableWildcard(_) => {}
            }
        }
        if let Some(from) = select.from {
            for join in from.joins {
                if let Some(on) = join.on {
                    visit(on);
                }
            }
        }
        for expression in select
            .where_clause
            .into_iter()
            .chain(select.group_by.iter().copied())
            .chain(select.having)
            .chain(select.order_by.iter().map(|order| order.expression))
            .chain(select.limit)
            .chain(select.offset)
        {
            visit(expression);
        }
    }

    fn infer_expression_params<'a>(
        &'a self,
        expression: &'a Expr<'a>,
        txid: u32,
        arena: &'a Arena,
        oids: &mut [i32; MAX_BIND_PARAMS],
        infer: &impl Fn(&Expr<'a>) -> Option<i32>,
    ) {
        let assign = |expression: &Expr, type_oid: i32, oids: &mut [i32; MAX_BIND_PARAMS]| {
            if let Expr::Param(index) = expression
                && *index >= 1
                && (*index as usize) <= MAX_BIND_PARAMS
            {
                oids[*index as usize - 1] = type_oid;
            }
        };
        match expression {
            Expr::Call {
                name,
                args,
                argument_names,
                variadic,
                ..
            } => {
                let mut actual = [types::oid::UNKNOWN; crate::storage::MAX_ROUTINE_ARGUMENTS];
                if args.len() <= actual.len() {
                    for (index, argument) in args.iter().copied().enumerate() {
                        actual[index] = infer(argument).unwrap_or(types::oid::UNKNOWN);
                    }
                    if let Some((schema, operator)) = crate::sql::ast::catalog_operator_call(name) {
                        if args.len() == 1 {
                            if let Some(expected) = self
                                .storage
                                .operator_prefix_parameter_oid(schema, operator, actual[0], txid)
                            {
                                assign(args[0], expected, oids);
                            }
                        } else if args.len() == 2
                            && let Some(expected) = self.storage.operator_call_parameter_oids(
                                schema,
                                operator,
                                [actual[0], actual[1]],
                                txid,
                            )
                        {
                            for (argument, expected_oid) in args.iter().copied().zip(expected) {
                                if expected_oid != types::oid::UNKNOWN {
                                    assign(argument, expected_oid, oids);
                                }
                            }
                        }
                    } else if let Some(expected) = self.storage.function_call_parameter_oids(
                        name,
                        argument_names,
                        *variadic,
                        &actual[..args.len()],
                        txid,
                    ) {
                        for (argument, expected_oid) in args.iter().copied().zip(expected) {
                            if expected_oid != types::oid::UNKNOWN {
                                assign(argument, expected_oid, oids);
                            }
                        }
                    }
                }
            }
            Expr::Cast {
                operand, type_name, ..
            } => {
                let type_oid = types::ColType::from_sql_name(type_name)
                    .map(types::ColType::oid)
                    .or_else(|| catalog::user_type_oid(&self.storage, txid, type_name));
                if let Some(type_oid) = type_oid {
                    assign(operand, type_oid, oids);
                }
            }
            Expr::Binary { left, right, .. } => {
                if let Some(type_oid) = infer(right) {
                    assign(left, type_oid, oids);
                }
                if let Some(type_oid) = infer(left) {
                    assign(right, type_oid, oids);
                }
            }
            Expr::Between {
                operand, low, high, ..
            } => {
                if let Some(type_oid) = infer(operand) {
                    assign(low, type_oid, oids);
                    assign(high, type_oid, oids);
                } else if let Some(type_oid) = infer(low).or_else(|| infer(high)) {
                    assign(operand, type_oid, oids);
                }
            }
            Expr::InList { operand, list, .. } => {
                if let Some(type_oid) = infer(operand) {
                    for item in *list {
                        assign(item, type_oid, oids);
                    }
                } else if let Some(type_oid) = list.iter().find_map(|item| infer(item)) {
                    assign(operand, type_oid, oids);
                }
            }
            Expr::Like {
                operand, pattern, ..
            }
            | Expr::Match {
                operand, pattern, ..
            } => {
                assign(operand, types::oid::TEXT, oids);
                assign(pattern, types::oid::TEXT, oids);
                if let Expr::Like { escape, .. } = expression
                    && let Some(escape) = escape
                {
                    assign(escape, types::oid::TEXT, oids);
                }
            }
            Expr::Unary {
                operator: ast::UnaryOp::Not,
                operand,
            } => assign(operand, types::oid::BOOL, oids),
            Expr::Case { whens, .. } => {
                for (condition, _) in *whens {
                    assign(condition, types::oid::BOOL, oids);
                }
            }
            Expr::Subquery(select) | Expr::Exists(select) | Expr::ArraySubquery(select) => {
                self.infer_resolved_select_params(select, txid, arena, oids);
            }
            Expr::InSubquery { select, .. } | Expr::QuantifiedSubquery { select, .. } => {
                self.infer_resolved_select_params(select, txid, arena, oids);
            }
            _ => {}
        }
        query::walk_children(expression, &mut |child| {
            self.infer_expression_params(child, txid, arena, oids, infer);
            Ok(())
        })
        .expect("parameter expression walk cannot fail");
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
                if let Some(select) = ins.select {
                    self.infer_select_source_params(select, txid, oids);
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
                if let Some(from) = u.from {
                    Self::infer_from_source_params(from, oids);
                }
            }
            Stmt::Delete(d) => {
                if let Some(w) = d.where_clause {
                    self.infer_where_params(&d.table, w, txid, oids);
                }
                if let Some(using) = d.using {
                    Self::infer_from_source_params(using, oids);
                }
            }
            Stmt::Merge(merge) => {
                Self::infer_table_sample_params(&merge.source, oids);
            }
            Stmt::Select(s) => {
                self.infer_select_source_params(s, txid, oids);
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
            Stmt::SetQuery(query) => {
                self.infer_set_tree_source_params(query.body, txid, oids);
                for cte in query.with {
                    match cte.dml {
                        Some(dml) => self.infer_stmt_params(dml, txid, oids),
                        None => self.infer_select_source_params(cte.query, txid, oids),
                    }
                }
            }
            _ => {}
        }
    }

    fn infer_select_source_params(
        &self,
        select: &ast::Select<'_>,
        txid: u32,
        oids: &mut [i32; MAX_BIND_PARAMS],
    ) {
        if let Some(from) = select.from {
            Self::infer_from_source_params(&from, oids);
        }
        if let Some(tree) = select.set_body {
            self.infer_set_tree_source_params(tree, txid, oids);
        }
        for cte in select.with {
            match cte.dml {
                Some(dml) => self.infer_stmt_params(dml, txid, oids),
                None => self.infer_select_source_params(cte.query, txid, oids),
            }
        }
    }

    fn infer_from_source_params(from: &ast::FromClause<'_>, oids: &mut [i32; MAX_BIND_PARAMS]) {
        Self::infer_table_sample_params(&from.base, oids);
        for join in from.joins {
            Self::infer_table_sample_params(&join.table, oids);
        }
    }

    fn infer_table_sample_params(table: &ast::TableRef<'_>, oids: &mut [i32; MAX_BIND_PARAMS]) {
        let Some(sample) = table.sample else { return };
        let assign = |expression: &Expr, oid: i32, oids: &mut [i32; MAX_BIND_PARAMS]| {
            if let Expr::Param(parameter) = expression
                && *parameter >= 1
                && (*parameter as usize) <= MAX_BIND_PARAMS
            {
                oids[*parameter as usize - 1] = oid;
            }
        };
        assign(sample.percentage, types::ColType::Float4.oid(), oids);
        if let Some(repeatable) = sample.repeatable {
            assign(repeatable, types::ColType::Float8.oid(), oids);
        }
    }

    fn infer_set_tree_source_params(
        &self,
        tree: &ast::SetTree<'_>,
        txid: u32,
        oids: &mut [i32; MAX_BIND_PARAMS],
    ) {
        match tree {
            ast::SetTree::Select(select) => self.infer_select_source_params(select, txid, oids),
            ast::SetTree::Op { left, right, .. } => {
                self.infer_set_tree_source_params(left, txid, oids);
                self.infer_set_tree_source_params(right, txid, oids);
            }
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
        if let Stmt::Merge(merge) = statement {
            if merge.returning.is_empty() {
                responder.no_data()?;
                return Ok(true);
            }
            let mut columns = [ColDesc::new("", 0, 0); MAX_PROJ];
            return match exec::describe_merge_returning(
                &self.storage,
                txn.txid,
                merge,
                arena,
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
            };
        }
        let (target, returning, target_alias) = match statement {
            Stmt::Insert(insert) => (insert.table, insert.returning, None),
            Stmt::Update(update) => (update.table, update.returning, update.alias),
            Stmt::Delete(delete) => (delete.table, delete.returning, delete.alias),
            _ => {
                responder.no_data()?;
                return Ok(true);
            }
        };
        if returning.is_empty() {
            responder.no_data()?;
            return Ok(true);
        }
        let (target, returning, target_alias) =
            match query::resolve_view_for_dml(&self.storage, target, txn.txid, arena) {
                Ok(Some(view)) => {
                    let rewritten = match query::rewrite_view_dml(
                        statement,
                        target.name,
                        view.base.name,
                        view.base.schema.expect("view base is qualified"),
                        view.columns,
                        view.base_columns,
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
                    let (returning, target_alias) = match rewritten {
                        Stmt::Insert(insert) => (insert.returning, None),
                        Stmt::Update(update) => (update.returning, update.alias),
                        Stmt::Delete(delete) => (delete.returning, delete.alias),
                        _ => unreachable!("view rewrite keeps its statement kind"),
                    };
                    (view.base, returning, target_alias)
                }
                Ok(None) => (target, returning, target_alias),
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
        match exec::describe_returning_items(
            returning,
            Some(&definition),
            target_alias,
            Some(&self.storage),
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
        cursors: Option<&cursor::CursorPool>,
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
            Stmt::Insert(_) | Stmt::Update(_) | Stmt::Delete(_) | Stmt::Merge(_) => {
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
            Stmt::Call {
                name,
                arguments,
                argument_names,
                variadic: _,
            } => {
                let qualified = match name.schema {
                    Some(schema) => stack_format!(260, "{}.{}", schema, name.name),
                    None => stack_format!(260, "{}", name.name),
                };
                let Some(slot) = self.storage.procedure_slot_for_call_shape(
                    qualified.as_str(),
                    argument_names,
                    arguments.len(),
                    txn.txid,
                ) else {
                    responder.error(
                        sqlstate::UNDEFINED_FUNCTION,
                        stack_format!(192, "procedure \"{}\" does not exist", qualified.as_str())
                            .as_str(),
                    )?;
                    return Ok(false);
                };
                let routine = self.storage.routine_for(slot, txn.txid);
                let mut columns =
                    [ColDesc::new("", types::oid::TEXT, -1); crate::storage::MAX_ROUTINE_ARGUMENTS];
                let mut count = 0usize;
                for parameter in routine.parameters() {
                    if !parameter.mode.is_output() {
                        continue;
                    }
                    let Some(type_oid) = self.storage.routine_type_oid(
                        parameter.ctype,
                        parameter.user_type,
                        txn.txid,
                    ) else {
                        responder.error(
                            sqlstate::INTERNAL_ERROR,
                            "procedure output type identity is unavailable",
                        )?;
                        return Ok(false);
                    };
                    let name = match arena.alloc_str(parameter.name.as_str()) {
                        Ok(name) => name,
                        Err(_) => {
                            responder.error(
                                sqlstate::PROGRAM_LIMIT_EXCEEDED,
                                "procedure description exceeds the statement arena",
                            )?;
                            return Ok(false);
                        }
                    };
                    columns[count] = ColDesc::new(name, type_oid, parameter.ctype.typlen());
                    count += 1;
                }
                if count == 0 {
                    responder.no_data()?;
                } else {
                    responder.row_description(&columns[..count])?;
                }
                Ok(true)
            }
            Stmt::Show(name) => {
                responder.row_description(&[ColDesc::new(name, types::oid::TEXT, -1)])?;
                Ok(true)
            }
            Stmt::FetchCursor {
                name, move_only, ..
            } => {
                if *move_only {
                    responder.no_data()?;
                    return Ok(true);
                }
                let Some((description, formats)) = cursors.and_then(|cursors| {
                    cursors.fetch_description(name, responder.result_formats())
                }) else {
                    responder.error(
                        eval::sqlstate::UNDEFINED_CURSOR,
                        stack_format!(96, "cursor \"{name}\" does not exist").as_str(),
                    )?;
                    return Ok(false);
                };
                responder.cursor_row_description(description, formats)?;
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
                Some(&sequence::SeqEval::new(
                    &self.storage,
                    guc.seq_session(),
                    txn.txid,
                )),
            )?;
            let returning = match dml {
                Stmt::Insert(i) => (&i.table, i.returning),
                Stmt::Update(u) => (&u.table, u.returning),
                Stmt::Delete(d) => (&d.table, d.returning),
                Stmt::Merge(m) => (&m.target, m.returning),
                _ => {
                    return Err(sql_err!(
                        sqlstate::FEATURE_NOT_SUPPORTED,
                        "a data-modifying WITH sub-statement must be INSERT, UPDATE, DELETE or MERGE"
                    ));
                }
            };
            // Describe the RETURNING columns against the target table, applying
            // the CTE's optional rename list.
            let mut descs = [ColDesc::new("", 0, 0); MAX_PROJ];
            let ncols = match dml {
                Stmt::Merge(merge) => exec::describe_merge_returning(
                    &self.storage,
                    txn.txid,
                    merge,
                    arena,
                    &mut descs,
                )?,
                _ => {
                    let target = returning.0;
                    let described_target =
                        match query::resolve_view_for_dml(&self.storage, *target, txn.txid, arena)?
                        {
                            Some(view) => view.base,
                            None => *target,
                        };
                    let idx = crate::sql::exec::resolve_dml_table(
                        &self.storage,
                        &described_target,
                        txn.txid,
                    )?;
                    let def = arena
                        .alloc(*self.storage.table_def(idx, txn.txid))
                        .map_err(|_| query::arena_full_pub())?;
                    let mut local = [ColDesc::new("", 0, 0); MAX_PROJ];
                    let count = exec::describe_returning_items(
                        returning.1,
                        Some(&*def),
                        match dml {
                            Stmt::Insert(_) => None,
                            Stmt::Update(update) => update.alias,
                            Stmt::Delete(delete) => delete.alias,
                            _ => unreachable!(),
                        },
                        Some(&self.storage),
                        txn.txid,
                        &mut local,
                    )?;
                    for index in 0..count {
                        descs[index] = local[index];
                        descs[index].name = arena
                            .alloc_str(local[index].name)
                            .map_err(|_| query::arena_full_pub())?;
                    }
                    count
                }
            };
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
            let outcome = match dml {
                Stmt::Merge(_) => Self::execute_merge(
                    &mut self.storage,
                    &mut self.dml_scratch,
                    &self.work,
                    dml,
                    txn,
                    params,
                    guc,
                    responder,
                    Some(&mut sink),
                ),
                _ => Self::execute_data_modification(
                    &mut self.storage,
                    &mut self.dml_scratch,
                    &self.work,
                    dml,
                    exec::DmlAuthorization::Invoker,
                    txn,
                    params,
                    guc,
                    responder,
                    Some(&mut sink),
                    self.current_conn_id,
                ),
            };
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
                    source: ast::MaterializedCteSource::Inline(rows),
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

    #[allow(clippy::too_many_arguments, clippy::type_complexity)]
    fn execute_data_modification<'a, 'capture>(
        storage: &mut Storage,
        scratch: &mut exec::DmlScratch,
        arena: &'a Arena,
        statement: &'a Stmt<'a>,
        authorization: exec::DmlAuthorization,
        txn: &mut TxnState,
        params: &[Datum<'a>],
        guc: &mut GucState,
        responder: &mut Responder,
        capture: Option<&'capture mut ReturningCapture<'capture>>,
        connection_id: i32,
    ) -> Result<Result<(), SqlError>, WireFull> {
        let sequence = sequence::SeqEval::new(storage, guc.seq_session(), txn.txid);
        let statement = match query::expand_dml_ctes(
            statement,
            &[],
            storage,
            txn.txid,
            arena,
            params,
            &[],
            Some(&sequence),
        ) {
            Ok(statement) => statement,
            Err(error) => return Ok(Err(error)),
        };
        let (relation, event) = match statement {
            Stmt::Insert(insert) => (insert.table, crate::storage::RewriteEvent::Insert),
            Stmt::Update(update) => (update.table, crate::storage::RewriteEvent::Update),
            Stmt::Delete(delete) => (delete.table, crate::storage::RewriteEvent::Delete),
            _ => {
                return Ok(Err(sql_err!(
                    sqlstate::INTERNAL_ERROR,
                    "rewrite input is not data-modifying"
                )));
            }
        };
        let outer_returns_rows = query::statement_returns_rows(statement);
        let resolved = match storage.resolve_relation(relation.schema, relation.name, txn.txid) {
            Some(resolved) => resolved,
            None => {
                return Self::execute_data_modification_unrewritten(
                    storage,
                    scratch,
                    arena,
                    statement,
                    authorization,
                    txn,
                    params,
                    guc,
                    responder,
                    capture,
                );
            }
        };
        let target = match resolved {
            crate::storage::ResolvedRelation::Table(slot) => {
                crate::storage::RuleTarget::Table(slot as u16)
            }
            crate::storage::ResolvedRelation::View(slot) => {
                crate::storage::RuleTarget::View(slot as u16)
            }
            crate::storage::ResolvedRelation::Catalog => {
                return Self::execute_data_modification_unrewritten(
                    storage,
                    scratch,
                    arena,
                    statement,
                    authorization,
                    txn,
                    params,
                    guc,
                    responder,
                    capture,
                );
            }
        };
        if matches!(
            statement,
            Stmt::Insert(Insert {
                on_conflict: Some(_),
                ..
            })
        ) {
            let has_insert_or_update_rule = storage
                .firing_rules_for(
                    target,
                    crate::storage::RewriteEvent::Insert,
                    txn.replication_apply,
                    txn.txid,
                )
                .next()
                .is_some()
                || storage
                    .firing_rules_for(
                        target,
                        crate::storage::RewriteEvent::Update,
                        txn.replication_apply,
                        txn.txid,
                    )
                    .next()
                    .is_some();
            if has_insert_or_update_rule {
                return Ok(Err(sql_err!(
                    sqlstate::FEATURE_NOT_SUPPORTED,
                    "INSERT with ON CONFLICT clause cannot be used with table that has INSERT or UPDATE rules"
                )));
            }
        }
        if storage
            .firing_rules_for(target, event, txn.replication_apply, txn.txid)
            .next()
            .is_none()
        {
            return Self::execute_data_modification_unrewritten(
                storage,
                scratch,
                arena,
                statement,
                authorization,
                txn,
                params,
                guc,
                responder,
                capture,
            );
        }
        let rule_owner = match u16::try_from(storage.object_owner(target.access_object(), txn.txid))
        {
            Ok(owner) => owner,
            Err(_) => {
                return Ok(Err(sql_err!(
                    sqlstate::PROGRAM_LIMIT_EXCEEDED,
                    "rewrite-rule owner is out of range"
                )));
            }
        };
        let rule_authorization = exec::DmlAuthorization::RuleOwner(rule_owner);
        if let Err(error) = exec::require_rewrite_input_privileges(
            storage,
            statement,
            authorization,
            txn.txid,
            arena,
        ) {
            return Ok(Err(error));
        }

        let mut rule_defaults: exec::constraints::ParsedDefaults<'a> =
            [None; crate::storage::MAX_COLUMNS];
        let mut force_defaults = [false; crate::storage::MAX_COLUMNS];
        let transition_collation = |collation| -> Result<_, SqlError> {
            match collation {
                ast::Collation::None | ast::Collation::Default => Ok(None),
                ast::Collation::C | ast::Collation::Posix | ast::Collation::UcsBasic => {
                    Ok(Some(ast::ParsedCollation::Builtin(collation)))
                }
                ast::Collation::Catalog(slot) => {
                    let definition = storage
                        .collation(usize::from(slot))
                        .definition_for(txn.txid);
                    let schema = arena
                        .alloc_str(definition.schema.as_str())
                        .map_err(|_| query::arena_full_pub())?;
                    let name = arena
                        .alloc_str(definition.name.as_str())
                        .map_err(|_| query::arena_full_pub())?;
                    let name = arena
                        .alloc(ast::QualName {
                            schema: Some(schema),
                            name,
                        })
                        .map_err(|_| query::arena_full_pub())?;
                    Ok(Some(ast::ParsedCollation::Named(&*name)))
                }
            }
        };
        let mut transition_types = [query::RuleTransitionType {
            type_name: "text",
            type_mod: -1,
            collation: None,
        }; crate::storage::MAX_COLUMNS];
        let (columns, transition_types) = match target {
            crate::storage::RuleTarget::Table(slot) => {
                let definition = storage.table_def(usize::from(slot), txn.txid);
                rule_defaults = match exec::parse_defaults(definition, arena) {
                    Ok(defaults) => defaults,
                    Err(error) => return Ok(Err(error)),
                };
                if let Stmt::Insert(insert) = statement
                    && insert.overriding == ast::Overriding::User
                {
                    for (index, column) in definition.columns().iter().enumerate() {
                        force_defaults[index] = column.is_identity;
                    }
                }
                let mut names = [""; crate::storage::MAX_COLUMNS];
                for (index, column) in definition.columns().iter().enumerate() {
                    names[index] = match arena.alloc_str(column.name.as_str()) {
                        Ok(name) => name,
                        Err(_) => return Ok(Err(query::arena_full_pub())),
                    };
                    let type_oid =
                        match storage.routine_type_oid(column.ctype, column.user_type, txn.txid) {
                            Some(type_oid) => type_oid,
                            None => {
                                return Ok(Err(sql_err!(
                                    sqlstate::UNDEFINED_OBJECT,
                                    "rule target column has an unknown type"
                                )));
                            }
                        };
                    let type_name =
                        match catalog::user_type_name_text(storage, txn.txid, type_oid, arena) {
                            Ok(Some(name)) => name,
                            Ok(None) => column.ctype.name(),
                            Err(error) => return Ok(Err(error)),
                        };
                    transition_types[index] = query::RuleTransitionType {
                        type_name,
                        type_mod: column.type_mod,
                        collation: match transition_collation(column.collation) {
                            Ok(collation) => collation,
                            Err(error) => return Ok(Err(error)),
                        },
                    };
                }
                let columns = match arena.alloc_slice_copy(&names[..definition.n_columns]) {
                    Ok(columns) => &*columns,
                    Err(_) => return Ok(Err(query::arena_full_pub())),
                };
                let types = match arena.alloc_slice_copy(&transition_types[..definition.n_columns])
                {
                    Ok(types) => &*types,
                    Err(_) => return Ok(Err(query::arena_full_pub())),
                };
                (columns, types)
            }
            crate::storage::RuleTarget::View(slot) => {
                let mut descriptions = [ColDesc::new("", 0, 0); MAX_PROJ];
                let count = match catalog::describe_view(
                    storage,
                    txn.txid,
                    storage.view(usize::from(slot)),
                    arena,
                    &mut descriptions,
                ) {
                    Ok(count) => count,
                    Err(error) => return Ok(Err(error)),
                };
                let mut names = [""; crate::storage::MAX_COLUMNS];
                for index in 0..count {
                    names[index] = match arena.alloc_str(descriptions[index].name) {
                        Ok(name) => name,
                        Err(_) => return Ok(Err(query::arena_full_pub())),
                    };
                    let ctype = match exec::catalog_column_type(
                        storage,
                        txn.txid,
                        descriptions[index].type_oid,
                    ) {
                        Some((ctype, _)) => ctype,
                        None => {
                            return Ok(Err(sql_err!(
                                sqlstate::UNDEFINED_OBJECT,
                                "rule target column has an unknown type"
                            )));
                        }
                    };
                    let type_name = match catalog::user_type_name_text(
                        storage,
                        txn.txid,
                        descriptions[index].type_oid,
                        arena,
                    ) {
                        Ok(Some(name)) => name,
                        Ok(None) => ctype.name(),
                        Err(error) => return Ok(Err(error)),
                    };
                    transition_types[index] = query::RuleTransitionType {
                        type_name,
                        type_mod: descriptions[index].type_mod,
                        collation: match transition_collation(descriptions[index].collation) {
                            Ok(collation) => collation,
                            Err(error) => return Ok(Err(error)),
                        },
                    };
                }
                let columns = match arena.alloc_slice_copy(&names[..count]) {
                    Ok(columns) => &*columns,
                    Err(_) => return Ok(Err(query::arena_full_pub())),
                };
                let types = match arena.alloc_slice_copy(&transition_types[..count]) {
                    Ok(types) => &*types,
                    Err(_) => return Ok(Err(query::arena_full_pub())),
                };
                (columns, types)
            }
        };
        let transition = match query::rule_transition_source(
            statement,
            columns,
            transition_types,
            &rule_defaults,
            &force_defaults,
            arena,
        ) {
            Ok(transition) => transition,
            Err(error) => return Ok(Err(error)),
        };
        let original_transition =
            match query::rule_original_transition(statement, columns, transition_types, arena) {
                Ok(transition) => transition,
                Err(error) => return Ok(Err(error)),
            };
        let suppress_original = storage
            .firing_rules_for(target, event, txn.replication_apply, txn.txid)
            .any(|(_, rule)| {
                let definition = rule.definition_for(txn.txid);
                definition.mode == crate::storage::RewriteMode::Instead
                    && definition.condition.is_none()
            });
        if outer_returns_rows
            && suppress_original
            && !storage
                .firing_rules_for(target, event, txn.replication_apply, txn.txid)
                .any(|(_, rule)| rule.definition_for(txn.txid).returning_action.is_some())
        {
            return Ok(Err(sql_err!(
                sqlstate::FEATURE_NOT_SUPPORTED,
                "cannot perform a RETURNING query on relation \"{}\"",
                relation.name
            )));
        }
        let insert_event = event == crate::storage::RewriteEvent::Insert;
        let mut original_filter: Option<&Expr<'a>> = None;
        for (_, rule) in storage.firing_rules_for(target, event, txn.replication_apply, txn.txid) {
            let definition = rule.definition_for(txn.txid);
            if definition.mode != crate::storage::RewriteMode::Instead
                || definition.condition.is_none()
            {
                continue;
            }
            let user = eval::funcs::system::session_user_owned();
            let path =
                storage.compute_path(definition.creation_path.as_str(), user.as_str(), txn.txid);
            let sql = definition.condition_sql().expect("qualified rule");
            let sql = match arena.alloc_str(sql) {
                Ok(sql) => sql,
                Err(_) => return Ok(Err(query::arena_full_pub())),
            };
            let parsed = match parser::parse_expr(sql, arena) {
                Ok(parsed) => parsed,
                Err(error) => return Ok(Err(error)),
            };
            let binding = if insert_event {
                (transition.names, transition.old, transition.new)
            } else {
                (
                    original_transition.names,
                    original_transition.old,
                    original_transition.new,
                )
            };
            let condition = match query::expand_stored_rule_expression_exec(
                parsed,
                storage,
                txn.txid,
                path,
                &definition.dependencies,
                arena,
                params,
                Some(&sequence::SeqEval::new(
                    storage,
                    guc.seq_session(),
                    txn.txid,
                )),
                rule_owner,
                binding.0,
                binding.1,
                binding.2,
                event != crate::storage::RewriteEvent::Insert,
                event != crate::storage::RewriteEvent::Delete,
            ) {
                Ok(condition) => condition,
                Err(error) => return Ok(Err(error)),
            };
            let exclusion = match arena.alloc(Expr::Unary {
                operator: ast::UnaryOp::Not,
                operand: condition,
            }) {
                Ok(exclusion) => &*exclusion,
                Err(_) => return Ok(Err(query::arena_full_pub())),
            };
            original_filter = match query::and_where(original_filter, Some(exclusion), arena) {
                Ok(filter) => filter,
                Err(error) => return Ok(Err(error)),
            };
        }
        let original_statement = if let Some(filter) = original_filter {
            match query::restrict_rule_original(statement, transition, columns, filter, arena) {
                Ok(statement) => statement,
                Err(error) => return Ok(Err(error)),
            }
        } else {
            statement
        };
        let mut capture = capture;
        let mut original_rows = 0u64;
        if insert_event && !suppress_original {
            let outcome = Self::execute_data_modification_unrewritten(
                storage,
                scratch,
                arena,
                original_statement,
                authorization,
                txn,
                params,
                guc,
                responder,
                capture.take(),
            )?;
            if outcome.is_err() {
                return Ok(outcome);
            }
            original_rows = responder.take_affected_rows().unwrap_or(0);
        }

        let mut last_name: Option<SqlName> = None;
        let mut rewritten_rows = 0u64;
        loop {
            let selected = storage
                .firing_rules_for(target, event, txn.replication_apply, txn.txid)
                .filter(|(_, rule)| {
                    last_name.is_none_or(|last| {
                        rule.definition_for(txn.txid).name.as_str() > last.as_str()
                    })
                })
                .min_by(|(_, left), (_, right)| {
                    left.definition_for(txn.txid)
                        .name
                        .as_str()
                        .cmp(right.definition_for(txn.txid).name.as_str())
                });
            let Some((_, rule)) = selected else { break };
            let definition = rule.definition_for(txn.txid);
            last_name = Some(definition.name);
            if let Err(error) = txn.enter_rule(rule.oid(), relation.name) {
                return Ok(Err(error));
            }
            let action_result = (|| -> Result<Result<u64, SqlError>, WireFull> {
                let user = eval::funcs::system::session_user_owned();
                let path = storage.compute_path(
                    definition.creation_path.as_str(),
                    user.as_str(),
                    txn.txid,
                );
                let condition = match definition.condition_sql() {
                    Some(sql) => {
                        let sql = match arena.alloc_str(sql) {
                            Ok(sql) => sql,
                            Err(_) => return Ok(Err(query::arena_full_pub())),
                        };
                        let parsed = match parser::parse_expr(sql, arena) {
                            Ok(parsed) => parsed,
                            Err(error) => return Ok(Err(error)),
                        };
                        match query::expand_stored_rule_expression_exec(
                            parsed,
                            storage,
                            txn.txid,
                            path,
                            &definition.dependencies,
                            arena,
                            params,
                            Some(&sequence::SeqEval::new(
                                storage,
                                guc.seq_session(),
                                txn.txid,
                            )),
                            rule_owner,
                            transition.names,
                            transition.old,
                            transition.new,
                            event != crate::storage::RewriteEvent::Insert,
                            event != crate::storage::RewriteEvent::Delete,
                        ) {
                            Ok(condition) => Some(condition),
                            Err(error) => return Ok(Err(error)),
                        }
                    }
                    None => None,
                };
                let mut affected = 0u64;
                for (action_index, action_sql) in definition.action_sql().enumerate() {
                    let action_sql = match arena.alloc_str(action_sql) {
                        Ok(sql) => sql,
                        Err(_) => return Ok(Err(query::arena_full_pub())),
                    };
                    let parsed = match parser::parse_stored_statement(action_sql, arena) {
                        Ok(parsed) => parsed,
                        Err(error) => return Ok(Err(error)),
                    };
                    let action = match query::expand_stored_rule_action_exec(
                        parsed,
                        storage,
                        txn.txid,
                        path,
                        &definition.dependencies,
                        arena,
                        params,
                        Some(&sequence::SeqEval::new(
                            storage,
                            guc.seq_session(),
                            txn.txid,
                        )),
                        rule_owner,
                        transition.names,
                        transition.old,
                        transition.new,
                        event != crate::storage::RewriteEvent::Insert,
                        event != crate::storage::RewriteEvent::Delete,
                    ) {
                        Ok(action) => action,
                        Err(error) => return Ok(Err(error)),
                    };
                    let action = match query::attach_rule_source(
                        action,
                        transition.source,
                        condition,
                        arena,
                    ) {
                        Ok(action) => action,
                        Err(error) => return Ok(Err(error)),
                    };
                    let use_returning = outer_returns_rows
                        && definition.returning_action == Some(action_index as u8);
                    let outcome = Self::execute_rule_action(
                        storage,
                        scratch,
                        arena,
                        action,
                        rule_authorization,
                        txn,
                        params,
                        guc,
                        responder,
                        if use_returning { capture.take() } else { None },
                        use_returning,
                        connection_id,
                    )?;
                    if let Err(error) = outcome {
                        return Ok(Err(error));
                    }
                    affected = affected.saturating_add(responder.take_affected_rows().unwrap_or(0));
                }
                Ok(Ok(affected))
            })();
            txn.leave_rule();
            match action_result? {
                Ok(affected) => rewritten_rows = rewritten_rows.saturating_add(affected),
                Err(error) => return Ok(Err(error)),
            }
        }

        if !insert_event && !suppress_original {
            let outcome = Self::execute_data_modification_unrewritten(
                storage,
                scratch,
                arena,
                original_statement,
                authorization,
                txn,
                params,
                guc,
                responder,
                capture.take(),
            )?;
            if outcome.is_err() {
                return Ok(outcome);
            }
            original_rows = responder.take_affected_rows().unwrap_or(0);
        }
        let affected = if suppress_original {
            rewritten_rows
        } else {
            original_rows
        };
        responder.set_affected_rows(affected);
        let tag = match statement {
            Stmt::Insert(_) => stack_format!(48, "INSERT 0 {}", affected),
            Stmt::Update(_) => stack_format!(48, "UPDATE {}", affected),
            Stmt::Delete(_) => stack_format!(48, "DELETE {}", affected),
            _ => unreachable!(),
        };
        responder.command_complete(tag.as_str())?;
        Ok(Ok(()))
    }

    #[allow(clippy::too_many_arguments, clippy::type_complexity)]
    fn execute_rule_action<'a, 'capture>(
        storage: &mut Storage,
        scratch: &mut exec::DmlScratch,
        arena: &'a Arena,
        action: &'a Stmt<'a>,
        authorization: exec::DmlAuthorization,
        txn: &mut TxnState,
        params: &[Datum<'a>],
        guc: &mut GucState,
        responder: &mut Responder,
        capture: Option<&'capture mut ReturningCapture<'capture>>,
        returning: bool,
        connection_id: i32,
    ) -> Result<Result<(), SqlError>, WireFull> {
        match action {
            Stmt::Insert(_) | Stmt::Update(_) | Stmt::Delete(_) if returning => responder
                .without_command_complete(|responder| {
                    Self::execute_data_modification(
                        storage,
                        scratch,
                        arena,
                        action,
                        authorization,
                        txn,
                        params,
                        guc,
                        responder,
                        capture,
                        connection_id,
                    )
                }),
            Stmt::Insert(_) | Stmt::Update(_) | Stmt::Delete(_) => {
                responder.without_query_output(|responder| {
                    Self::execute_data_modification(
                        storage,
                        scratch,
                        arena,
                        action,
                        authorization,
                        txn,
                        params,
                        guc,
                        responder,
                        None,
                        connection_id,
                    )
                })
            }
            Stmt::Select(select) => responder.without_query_output(|responder| {
                let sequence = sequence::SeqEval::new(storage, guc.seq_session(), txn.txid);
                if select.from.is_some() {
                    query::select_query(
                        storage,
                        txn.txid,
                        select,
                        arena,
                        params,
                        Some(&sequence),
                        responder,
                    )
                } else {
                    query::constant_select(
                        storage,
                        txn.txid,
                        select,
                        arena,
                        params,
                        Some(&sequence),
                        responder,
                    )
                }
            }),
            Stmt::SetQuery(query) => responder.without_query_output(|responder| {
                let sequence = sequence::SeqEval::new(storage, guc.seq_session(), txn.txid);
                query::set_query(
                    storage,
                    txn.txid,
                    query,
                    arena,
                    params,
                    Some(&sequence),
                    responder,
                )
            }),
            Stmt::Notify { channel, payload } => {
                let payload = match payload {
                    Some(payload) => match notify::payload(payload) {
                        Ok(payload) => payload,
                        Err(error) => return Ok(Err(error)),
                    },
                    None => notify::Payload::new(),
                };
                Ok(txn.buffer_notify(connection_id, notify::channel(channel), payload.as_str()))
            }
            _ => Ok(Err(sql_err!(
                sqlstate::FEATURE_NOT_SUPPORTED,
                "rewrite-rule action is not executable"
            ))),
        }
    }

    /// Executes one INSERT/UPDATE/DELETE after any enclosing WITH clause has
    /// been expanded. View rewriting lives here as well, so a data-modifying
    /// CTE and a main DML statement have exactly the same target semantics.
    #[allow(clippy::too_many_arguments, clippy::type_complexity)]
    fn execute_data_modification_unrewritten<'a, 'capture>(
        storage: &mut Storage,
        scratch: &mut exec::DmlScratch,
        arena: &'a Arena,
        statement: &'a Stmt<'a>,
        authorization: exec::DmlAuthorization,
        txn: &mut TxnState,
        params: &[Datum<'a>],
        guc: &mut GucState,
        responder: &mut Responder,
        capture: Option<&'capture mut ReturningCapture<'capture>>,
    ) -> Result<Result<(), SqlError>, WireFull> {
        // Expand every DML source before the mutable storage borrow so views
        // behave consistently across INSERT, UPDATE FROM, DELETE USING and MERGE.
        let sequence = sequence::SeqEval::new(storage, guc.seq_session(), txn.txid);
        let statement = match query::expand_dml_ctes(
            statement,
            &[],
            storage,
            txn.txid,
            arena,
            params,
            &[],
            Some(&sequence),
        ) {
            Ok(expanded) => expanded,
            Err(error) => return Ok(Err(error)),
        };
        let relation = match statement {
            Stmt::Insert(insert) => Some(insert.table),
            Stmt::Update(update) => Some(update.table),
            Stmt::Delete(delete) => Some(delete.table),
            _ => None,
        };
        let view = relation.and_then(|relation| {
            match storage.resolve_relation(relation.schema, relation.name, txn.txid) {
                Some(crate::storage::ResolvedRelation::View(view)) => Some(view),
                _ => None,
            }
        });
        let Some(view) = view else {
            return Self::execute_data_modification_inner(
                storage,
                scratch,
                arena,
                statement,
                authorization,
                txn,
                params,
                guc,
                responder,
                capture,
            );
        };
        let event = match statement {
            Stmt::Insert(_) => ast::TriggerEvents::INSERT,
            Stmt::Update(_) => ast::TriggerEvents::UPDATE,
            Stmt::Delete(_) => ast::TriggerEvents::DELETE,
            _ => unreachable!(),
        };
        if !storage
            .triggers_for_view(view, txn.txid)
            .any(|(_, trigger)| {
                matches!(trigger.level, ast::TriggerLevel::Statement)
                    && trigger.events.contains(event)
            })
        {
            return Self::execute_data_modification_inner(
                storage,
                scratch,
                arena,
                statement,
                authorization,
                txn,
                params,
                guc,
                responder,
                capture,
            );
        }
        if let Err(error) = exec::fire_view_statement_triggers(
            storage,
            txn,
            scratch,
            arena,
            guc.seq_session(),
            responder,
            view,
            statement,
            true,
        ) {
            return Ok(Err(error));
        }
        let capturing = capture.is_some();
        let outcome = responder.without_command_complete(|responder| {
            Self::execute_data_modification_inner(
                storage,
                scratch,
                arena,
                statement,
                authorization,
                txn,
                params,
                guc,
                responder,
                capture,
            )
        })?;
        let affected = responder.take_affected_rows().unwrap_or(0);
        if outcome.is_err() {
            return Ok(outcome);
        }
        if let Err(error) = exec::fire_view_statement_triggers(
            storage,
            txn,
            scratch,
            arena,
            guc.seq_session(),
            responder,
            view,
            statement,
            false,
        ) {
            return Ok(Err(error));
        }
        if capturing {
            responder.set_affected_rows(affected);
        } else {
            let tag = match statement {
                Stmt::Insert(_) => crate::stack_format!(48, "INSERT 0 {}", affected),
                Stmt::Update(_) => crate::stack_format!(48, "UPDATE {}", affected),
                Stmt::Delete(_) => crate::stack_format!(48, "DELETE {}", affected),
                _ => unreachable!(),
            };
            responder.command_complete_rows(tag.as_str(), affected)?;
        }
        Ok(outcome)
    }

    #[allow(clippy::too_many_arguments)]
    fn execute_merge<'a, 'capture>(
        storage: &mut Storage,
        scratch: &mut exec::DmlScratch,
        arena: &'a Arena,
        statement: &'a Stmt<'a>,
        txn: &mut TxnState,
        params: &[Datum<'a>],
        guc: &mut GucState,
        responder: &mut Responder,
        capture: Option<&'capture mut ReturningCapture<'capture>>,
    ) -> Result<Result<(), SqlError>, WireFull> {
        let sequence = sequence::SeqEval::new(storage, guc.seq_session(), txn.txid);
        let expanded = match query::expand_dml_ctes(
            statement,
            &[],
            storage,
            txn.txid,
            arena,
            params,
            &[],
            Some(&sequence),
        ) {
            Ok(Stmt::Merge(merge)) => merge,
            Ok(_) => unreachable!("MERGE source expansion keeps its statement kind"),
            Err(error) => return Ok(Err(error)),
        };
        exec::merge(
            storage,
            txn,
            scratch,
            expanded,
            arena,
            params,
            guc.seq_session(),
            responder,
            capture,
        )
    }

    #[allow(clippy::too_many_arguments, clippy::type_complexity)]
    fn execute_data_modification_inner<'a, 'capture>(
        storage: &mut Storage,
        scratch: &mut exec::DmlScratch,
        arena: &Arena,
        statement: &'a Stmt<'a>,
        authorization: exec::DmlAuthorization,
        txn: &mut TxnState,
        params: &[Datum<'a>],
        guc: &mut GucState,
        responder: &mut Responder,
        capture: Option<&'capture mut ReturningCapture<'capture>>,
    ) -> Result<Result<(), SqlError>, WireFull> {
        let authorization = match match statement {
            Stmt::Insert(insert) => storage
                .resolve_relation(insert.table.schema, insert.table.name, txn.txid)
                .and_then(|relation| match relation {
                    crate::storage::ResolvedRelation::View(view) => Some(view),
                    _ => None,
                })
                .map_or(Ok(authorization), |view| {
                    exec::require_view_dml_privileges(
                        storage,
                        statement,
                        authorization,
                        view,
                        txn.txid,
                        arena,
                    )
                }),
            Stmt::Update(update) => storage
                .resolve_relation(update.table.schema, update.table.name, txn.txid)
                .and_then(|relation| match relation {
                    crate::storage::ResolvedRelation::View(view) => Some(view),
                    _ => None,
                })
                .map_or(Ok(authorization), |view| {
                    exec::require_view_dml_privileges(
                        storage,
                        statement,
                        authorization,
                        view,
                        txn.txid,
                        arena,
                    )
                }),
            Stmt::Delete(delete) => storage
                .resolve_relation(delete.table.schema, delete.table.name, txn.txid)
                .and_then(|relation| match relation {
                    crate::storage::ResolvedRelation::View(view) => Some(view),
                    _ => None,
                })
                .map_or(Ok(authorization), |view| {
                    exec::require_view_dml_privileges(
                        storage,
                        statement,
                        authorization,
                        view,
                        txn.txid,
                        arena,
                    )
                }),
            _ => Ok(authorization),
        } {
            Ok(authorization) => authorization,
            Err(error) => return Ok(Err(error)),
        };
        match statement {
            Stmt::Insert(insert) => {
                if let Some(crate::storage::ResolvedRelation::View(view)) =
                    storage.resolve_relation(insert.table.schema, insert.table.name, txn.txid)
                    && storage
                        .triggers_for_view(view, txn.txid)
                        .any(|(_, trigger)| {
                            matches!(trigger.level, ast::TriggerLevel::Row)
                                && matches!(trigger.timing, ast::TriggerTiming::InsteadOf)
                        })
                {
                    return exec::instead_of_view_dml(
                        storage,
                        txn,
                        scratch,
                        statement,
                        authorization,
                        view,
                        arena,
                        params,
                        guc.seq_session(),
                        responder,
                        capture,
                    );
                }
                let (insert, view_check) =
                    match query::resolve_view_for_dml(storage, insert.table, txn.txid, arena) {
                        Ok(Some(view)) => {
                            let view_check = Some(exec::ViewCheck {
                                predicate: view.check_option.and(view.where_clause),
                                view_name: insert.table.name,
                                defaults: exec::ViewInsertDefaults {
                                    base_columns: view.base_columns,
                                    columns: view.defaults,
                                },
                            });
                            let rewritten = match query::rewrite_view_dml(
                                statement,
                                insert.table.name,
                                view.base.name,
                                view.base.schema.expect("view base is qualified"),
                                view.columns,
                                view.base_columns,
                                storage,
                                txn.txid,
                                arena,
                            ) {
                                Ok(Stmt::Insert(rewritten)) => rewritten,
                                Ok(_) => unreachable!("insert rewrite keeps its statement kind"),
                                Err(error) => return Ok(Err(error)),
                            };
                            let columns = if rewritten.columns.is_empty() {
                                view.base_columns
                            } else {
                                rewritten.columns
                            };
                            let rewritten = match arena.alloc(Insert {
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
                            };
                            (rewritten, view_check)
                        }
                        Ok(None) => (insert, None),
                        Err(error) => return Ok(Err(error)),
                    };
                exec::insert(
                    storage,
                    txn,
                    scratch,
                    insert,
                    authorization,
                    view_check,
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
                if let Some(crate::storage::ResolvedRelation::View(view)) =
                    storage.resolve_relation(update.table.schema, update.table.name, txn.txid)
                    && storage
                        .triggers_for_view(view, txn.txid)
                        .any(|(_, trigger)| {
                            matches!(trigger.level, ast::TriggerLevel::Row)
                                && matches!(trigger.timing, ast::TriggerTiming::InsteadOf)
                        })
                {
                    return exec::instead_of_view_dml(
                        storage,
                        txn,
                        scratch,
                        statement,
                        authorization,
                        view,
                        arena,
                        params,
                        guc.seq_session(),
                        responder,
                        capture,
                    );
                }
                let (update, view_check) =
                    match query::resolve_view_for_dml(storage, update.table, txn.txid, arena) {
                        Ok(Some(view)) => {
                            let view_check = view.check_option.map(|_| exec::ViewCheck {
                                predicate: view.where_clause,
                                view_name: update.table.name,
                                defaults: exec::ViewInsertDefaults {
                                    base_columns: view.base_columns,
                                    columns: view.defaults,
                                },
                            });
                            let rewritten = match query::rewrite_view_dml(
                                statement,
                                update.table.name,
                                view.base.name,
                                view.base.schema.expect("view base is qualified"),
                                view.columns,
                                view.base_columns,
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
                            let rewritten = match arena.alloc(Update {
                                table: view.base,
                                alias: update.alias,
                                assignments: rewritten.assignments,
                                from: rewritten.from,
                                where_clause,
                                returning: rewritten.returning,
                            }) {
                                Ok(rewritten) => &*rewritten,
                                Err(_) => return Ok(Err(query::arena_full_pub())),
                            };
                            (rewritten, view_check)
                        }
                        Ok(None) => (update, None),
                        Err(error) => return Ok(Err(error)),
                    };
                exec::update(
                    storage,
                    txn,
                    scratch,
                    update,
                    authorization,
                    view_check,
                    arena,
                    params,
                    guc.seq_session(),
                    responder,
                    capture,
                    None,
                )
            }
            Stmt::Delete(delete) => {
                if let Some(crate::storage::ResolvedRelation::View(view)) =
                    storage.resolve_relation(delete.table.schema, delete.table.name, txn.txid)
                    && storage
                        .triggers_for_view(view, txn.txid)
                        .any(|(_, trigger)| {
                            matches!(trigger.level, ast::TriggerLevel::Row)
                                && matches!(trigger.timing, ast::TriggerTiming::InsteadOf)
                        })
                {
                    return exec::instead_of_view_dml(
                        storage,
                        txn,
                        scratch,
                        statement,
                        authorization,
                        view,
                        arena,
                        params,
                        guc.seq_session(),
                        responder,
                        capture,
                    );
                }
                let delete =
                    match query::resolve_view_for_dml(storage, delete.table, txn.txid, arena) {
                        Ok(Some(view)) => {
                            let rewritten = match query::rewrite_view_dml(
                                statement,
                                delete.table.name,
                                view.base.name,
                                view.base.schema.expect("view base is qualified"),
                                view.columns,
                                view.base_columns,
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
                    authorization,
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

    #[allow(clippy::too_many_arguments)]
    fn execute_modification_resumable(
        &mut self,
        statement: &Stmt<'_>,
        arena: &Arena,
        params: &[Datum],
        txn: &mut TxnState,
        sqlprep: &mut SqlPreparedPool,
        cursors: &mut cursor::CursorPool,
        guc: &mut GucState,
        responder: &mut Responder,
    ) -> Result<Result<(), SqlError>, WireFull> {
        let invocations = query::RoutineInvocationState::new();
        loop {
            self.work.reset();
            invocations.begin_attempt();
            let attempt_arena_mark = arena.mark();
            let output_mark = responder.buffer.mark();
            let statement_mark =
                txn.statement_mark(self.wal.stage_mark(txn.txid), self.storage.lock_mark());
            let outcome = {
                let _scope = query::enter_routine_invocation_scope(Some(
                    query::RoutineInvocationContext::new(&invocations, arena),
                ));
                match statement {
                    Stmt::Insert(_) | Stmt::Update(_) | Stmt::Delete(_) => {
                        Self::execute_data_modification(
                            &mut self.storage,
                            &mut self.dml_scratch,
                            &self.work,
                            statement,
                            exec::DmlAuthorization::Invoker,
                            txn,
                            params,
                            guc,
                            responder,
                            None,
                            self.current_conn_id,
                        )?
                    }
                    Stmt::Merge(_) => Self::execute_merge(
                        &mut self.storage,
                        &mut self.dml_scratch,
                        arena,
                        statement,
                        txn,
                        params,
                        guc,
                        responder,
                        None,
                    )?,
                    Stmt::With { ctes, statement } => self.execute_with_data_modification(
                        ctes, statement, arena, params, txn, guc, responder, None,
                    )?,
                    _ => unreachable!("resumable modification has a mutation statement"),
                }
            };
            let Err(error) = outcome else {
                return Ok(outcome);
            };
            if error.sqlstate != sqlstate::INTERNAL_ROUTINE_INVOCATION {
                return Ok(Err(error));
            }
            self.rollback_waiting_statement(txn, statement_mark);
            responder.buffer.truncate_to(output_mark);
            // The failed attempt has emitted or encoded every value it owns.
            // Keep prior invocation results below the mark and reclaim only
            // the materialization that will be rebuilt on the next attempt.
            unsafe { arena.rewind_to(attempt_arena_mark) };
            let Some(pending) = invocations.take_pending() else {
                return Ok(Err(sql_err!(
                    sqlstate::INTERNAL_ERROR,
                    "data modification yielded without a pending routine invocation"
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
        if let Some(oid) = pending.intrinsic_oid {
            let mut arguments = [Datum::Null; crate::sql::parser::MAX_LIST];
            for (index, argument) in arguments[..pending.argument_count].iter_mut().enumerate() {
                *argument = match exec::decode_projected_col_record(pending.arguments, index, arena)
                {
                    Ok(value) => value,
                    Err(error) => return Ok(Err(error)),
                };
            }
            let value = match large_object::execute(
                oid,
                &arguments[..pending.argument_count],
                &mut self.storage,
                txn,
                arena,
            ) {
                Ok(value) => value,
                Err(error) => return Ok(Err(error)),
            };
            return Ok(invocations.complete(value));
        }
        if self
            .storage
            .routine_for(pending.slot, txn.txid)
            .kind
            .is_set_returning()
        {
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
        let base_sequence = sequence::SeqEval::new(&self.storage, guc.seq_session(), txn.txid);
        let replay_sequence =
            sequence_state.map(|state| sequence::ReplaySeqEval::new(base_sequence, state));
        let sequence: &dyn SequenceAccess = replay_sequence
            .as_ref()
            .map(|sequence| sequence as &dyn SequenceAccess)
            .unwrap_or(&base_sequence);
        let statement = match query::expand_ctes_exec(
            statement,
            &self.storage,
            txn.txid,
            &self.work,
            params,
            dml_mats,
            Some(sequence),
        ) {
            Ok(expanded) => expanded,
            Err(error) => return Ok(Err(error)),
        };
        if let Err(error) = query::validate_locking(statement) {
            return Ok(Err(error));
        }
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
        let owner = self
            .storage
            .role_name(routine.ownership.owner_to(txn.txid).into(), txn.txid);
        let _security = routine
            .attributes
            .security_definer
            .then(|| eval::funcs::system::enter_current_user(owner.as_str()));
        let _config = match guc.enter_routine_configs(routine.configs()) {
            Ok(scope) => scope,
            Err(error) => return Ok(Err(error)),
        };
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
            let statement = match query::bind_stored_routine_statement(
                statement,
                &self.storage,
                slot,
                routine,
                txn.txid,
                arena,
                arguments,
            ) {
                Ok(statement) => statement,
                Err(error) => return Ok(Err(error)),
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
                let outcome = query::execute_bound_routine_query(
                    result_query,
                    &self.storage,
                    slot,
                    routine,
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
            query::RoutineFunctionResult::DataModification(statement) => {
                let statement = match query::bind_stored_routine_statement(
                    statement,
                    &self.storage,
                    slot,
                    routine,
                    txn.txid,
                    arena,
                    arguments,
                ) {
                    Ok(statement) => statement,
                    Err(error) => return Ok(Err(error)),
                };
                loop {
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
                }
            }
            query::RoutineFunctionResult::Void(statement) => {
                let statement = match query::bind_stored_routine_statement(
                    statement,
                    &self.storage,
                    slot,
                    routine,
                    txn.txid,
                    arena,
                    arguments,
                ) {
                    Ok(statement) => statement,
                    Err(error) => return Ok(Err(error)),
                };
                loop {
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
                }
            }
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
        let routine = self.storage.routine_for(pending.slot, txn.txid);
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
        if routine.language == crate::storage::RoutineLanguage::PlPgSql {
            if let Err(error) = self.storage.require_routine_execute(pending.slot, txn.txid) {
                return Ok(Err(error));
            }
            let owner = self
                .storage
                .role_name(routine.ownership.owner_to(txn.txid).into(), txn.txid);
            let _security = routine
                .attributes
                .security_definer
                .then(|| eval::funcs::system::enter_current_user(owner.as_str()));
            let _config = match guc.enter_routine_configs(routine.configs()) {
                Ok(scope) => scope,
                Err(error) => return Ok(Err(error)),
            };
            let value = match exec::execute_plpgsql_function(
                self,
                txn,
                cursors,
                guc,
                &routine,
                &arguments[..pending.argument_count],
                arena,
                responder,
            ) {
                Ok(value) => value,
                Err(error) => return Ok(Err(error)),
            };
            let value = match exec::detach_routine_datum(value, arena) {
                Ok(value) => value,
                Err(error) => return Ok(Err(error)),
            };
            return Ok(eval::cast_to(value, result_type, arena));
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
        let routine_name = match arena.alloc_str(routine.name.as_str()) {
            Ok(name) => name,
            Err(_) => {
                return Ok(Err(sql_err!(
                    sqlstate::PROGRAM_LIMIT_EXCEEDED,
                    "statement arena exhausted while invoking SQL function"
                )));
            }
        };
        let program = match query::parse_stored_routine_function_program(
            routine.body_kind,
            body,
            arena,
            result_type == ColType::Void,
            routine_name,
            routine.arguments(),
        ) {
            Ok(program) => program,
            Err(error) => return Ok(Err(error)),
        };
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
        let routine = self.storage.routine_for(pending.slot, txn.txid);
        if !routine.kind.is_set_returning() {
            return Ok(Err(sql_err!(
                sqlstate::INTERNAL_ERROR,
                "table routine invocation resolved to a scalar function"
            )));
        }
        if let Err(error) = self.storage.require_routine_execute(pending.slot, txn.txid) {
            return Ok(Err(error));
        }
        let owner = self
            .storage
            .role_name(routine.ownership.owner_to(txn.txid).into(), txn.txid);
        let _security = routine
            .attributes
            .security_definer
            .then(|| eval::funcs::system::enter_current_user(owner.as_str()));
        let _config = match guc.enter_routine_configs(routine.configs()) {
            Ok(scope) => scope,
            Err(error) => return Ok(Err(error)),
        };
        if routine.language == crate::storage::RoutineLanguage::PlPgSql {
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
            let inputs = match arena.alloc_slice_copy(&arguments[..pending.argument_count]) {
                Ok(inputs) => inputs,
                Err(_) => {
                    return Ok(Err(sql_err!(
                        sqlstate::PROGRAM_LIMIT_EXCEEDED,
                        "statement arena exhausted while invoking PL/pgSQL table function"
                    )));
                }
            };
            let _formal_scope = exec::enter_routine_parameter_types(routine.arguments());
            return Ok(exec::execute_plpgsql_table_function(
                self, txn, cursors, guc, &routine, &*inputs, arena, responder,
            ));
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
        let routine_name = match arena.alloc_str(routine.name.as_str()) {
            Ok(name) => name,
            Err(_) => {
                return Ok(Err(sql_err!(
                    sqlstate::PROGRAM_LIMIT_EXCEEDED,
                    "statement arena exhausted while invoking SQL table function"
                )));
            }
        };
        let result_type = routine.kind.function_result().expect("set routine result");
        let program = match query::parse_stored_routine_function_program(
            routine.body_kind,
            body,
            arena,
            result_type == ColType::Void,
            routine_name,
            routine.arguments(),
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
            let statement = match query::bind_stored_routine_statement(
                statement,
                &self.storage,
                pending.slot,
                routine,
                txn.txid,
                arena,
                &arguments[..pending.argument_count],
            ) {
                Ok(statement) => statement,
                Err(error) => return Ok(Err(error)),
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
                let outcome = query::execute_bound_routine_query(
                    result,
                    &self.storage,
                    pending.slot,
                    routine,
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
            query::RoutineFunctionResult::DataModification(statement) => {
                let statement = match query::bind_stored_routine_statement(
                    statement,
                    &self.storage,
                    pending.slot,
                    routine,
                    txn.txid,
                    arena,
                    &arguments[..pending.argument_count],
                ) {
                    Ok(statement) => statement,
                    Err(error) => return Ok(Err(error)),
                };
                loop {
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
                }
            }
            query::RoutineFunctionResult::Void(statement) => {
                let statement = match query::bind_stored_routine_statement(
                    statement,
                    &self.storage,
                    pending.slot,
                    routine,
                    txn.txid,
                    arena,
                    &arguments[..pending.argument_count],
                ) {
                    Ok(statement) => statement,
                    Err(error) => return Ok(Err(error)),
                };
                loop {
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
                }
            }
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
                &mut self.dml_scratch,
                &self.work,
                statement,
                exec::DmlAuthorization::Invoker,
                txn,
                params,
                guc,
                responder,
                None,
                self.current_conn_id,
            ),
            Stmt::Merge(_) => Self::execute_merge(
                &mut self.storage,
                &mut self.dml_scratch,
                &self.work,
                statement,
                txn,
                params,
                guc,
                responder,
                None,
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
        let sequence = sequence::SeqEval::new(&self.storage, guc.seq_session(), txn.txid);
        let statement = match query::expand_dml_ctes(
            statement,
            ctes,
            &self.storage,
            txn.txid,
            &self.work,
            params,
            dml_mats,
            Some(&sequence),
        ) {
            Ok(expanded) => expanded,
            Err(error) => return Ok(Err(error)),
        };
        match statement {
            Stmt::Insert(_) | Stmt::Update(_) | Stmt::Delete(_) => Self::execute_data_modification(
                &mut self.storage,
                &mut self.dml_scratch,
                &self.work,
                statement,
                exec::DmlAuthorization::Invoker,
                txn,
                params,
                guc,
                responder,
                capture,
                self.current_conn_id,
            ),
            Stmt::Merge(_) => Self::execute_merge(
                &mut self.storage,
                &mut self.dml_scratch,
                &self.work,
                statement,
                txn,
                params,
                guc,
                responder,
                capture,
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
    fn execute_do(
        &mut self,
        body: &str,
        arena: &Arena,
        txn: &mut TxnState,
        cursors: &mut cursor::CursorPool,
        guc: &mut GucState,
        transaction_context: exec::PlpgsqlTransactionContext,
        responder: &mut Responder,
    ) -> Result<Result<(), SqlError>, WireFull> {
        if let Err(error) = self.storage.require_language_usage("plpgsql", txn.txid) {
            return Ok(Err(error));
        }
        match exec::execute_anonymous_plpgsql(
            self,
            txn,
            cursors,
            guc,
            transaction_context,
            body,
            arena,
            responder,
        ) {
            Ok(()) => responder.command_complete("DO").map(|_| Ok(())),
            Err(error) => Ok(Err(error)),
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn execute_call(
        &mut self,
        name: ast::QualName<'_>,
        arguments: &[&Expr<'_>],
        argument_names: &[Option<&str>],
        variadic: bool,
        arena: &Arena,
        params: &[Datum],
        txn: &mut TxnState,
        sqlprep: &mut SqlPreparedPool,
        cursors: &mut cursor::CursorPool,
        guc: &mut GucState,
        transaction_context: exec::PlpgsqlTransactionContext,
        responder: &mut Responder,
    ) -> Result<Result<(), SqlError>, WireFull> {
        let mut values = [Datum::Null; crate::storage::MAX_ROUTINE_ARGUMENTS];
        if arguments.len() > values.len() {
            return Ok(Err(sql_err!(
                sqlstate::PROGRAM_LIMIT_EXCEEDED,
                "too many procedure arguments"
            )));
        }
        let qualified = match name.schema {
            Some(schema) => stack_format!(260, "{}.{}", schema, name.name),
            None => stack_format!(260, "{}", name.name),
        };
        let shape_slot = self.storage.procedure_slot_for_call_shape(
            qualified.as_str(),
            argument_names,
            arguments.len(),
            txn.txid,
        );
        let shape_mapping = shape_slot.and_then(|slot| {
            let routine = self.storage.routine_for(slot, txn.txid);
            routine
                .parameters()
                .iter()
                .any(|parameter| parameter.mode.is_output())
                .then(|| routine.procedure_call_mapping(argument_names, arguments.len()))
                .flatten()
        });
        let catalog = query::storage_catalog(&self.storage, &self.work, txn.txid);
        let hooks = EvalHooks {
            catalog: Some(&catalog),
            ..NO_HOOKS
        };
        for (slot, argument) in arguments.iter().enumerate() {
            if shape_mapping.is_some_and(|mapping| mapping[slot] == u8::MAX) {
                continue;
            }
            values[slot] = match eval::eval_full(argument, arena, params, &NoColumns, &hooks) {
                Ok(value) => value,
                Err(error) => return Ok(Err(error)),
            };
        }
        let mut type_oids = [types::oid::UNKNOWN; crate::storage::MAX_ROUTINE_ARGUMENTS];
        for (slot, argument) in arguments.iter().enumerate() {
            if shape_mapping.is_some_and(|mapping| mapping[slot] == u8::MAX) {
                continue;
            }
            type_oids[slot] = match argument {
                ast::Expr::Cast { type_name, .. } => {
                    crate::sql::catalog::user_type_oid(&self.storage, txn.txid, type_name)
                        .unwrap_or_else(|| values[slot].type_oid())
                }
                _ => values[slot].type_oid(),
            };
        }
        let slot = if shape_mapping.is_some() {
            shape_slot
        } else if argument_names.is_empty() {
            self.storage.procedure_slot_for_call_syntax_oids(
                qualified.as_str(),
                &type_oids[..arguments.len()],
                variadic,
                txn.txid,
            )
        } else {
            self.storage.procedure_slot_for_named_call_oids(
                qualified.as_str(),
                argument_names,
                &type_oids[..arguments.len()],
                txn.txid,
            )
        };
        let Some(slot) = slot else {
            return Ok(Err(sql_err!(
                sqlstate::UNDEFINED_FUNCTION,
                "procedure \"{}\" does not exist",
                qualified.as_str()
            )));
        };
        if let Err(error) = self.storage.require_routine_execute(slot, txn.txid) {
            return Ok(Err(error));
        }
        let declared = self.storage.routine_for(slot, txn.txid);
        let mapping = shape_mapping.unwrap_or_else(|| {
            declared
                .call_input_mapping(argument_names, arguments.len(), variadic)
                .expect("resolved procedure call has a valid argument mapping")
        });
        let mut completed = [Datum::Null; crate::storage::MAX_ROUTINE_ARGUMENTS];
        let mut completed_type_oids = [types::oid::UNKNOWN; crate::storage::MAX_ROUTINE_ARGUMENTS];
        let mut provided = [false; crate::storage::MAX_ROUTINE_ARGUMENTS];
        let mut variadic_values = [Datum::Null; crate::storage::MAX_ROUTINE_ARGUMENTS];
        let mut variadic_count = 0usize;
        for call_index in 0..arguments.len() {
            if mapping[call_index] == u8::MAX {
                continue;
            }
            let input_index = usize::from(mapping[call_index]);
            if !variadic
                && matches!(
                    declared
                        .parameter_for_input(input_index)
                        .expect("mapped procedure input has a declared parameter")
                        .mode,
                    crate::storage::RoutineParameterMode::Variadic { .. }
                )
            {
                variadic_values[variadic_count] = values[call_index];
                variadic_count += 1;
                provided[input_index] = true;
                continue;
            }
            completed[input_index] = values[call_index];
            completed_type_oids[input_index] = type_oids[call_index];
            provided[input_index] = true;
        }
        if variadic_count != 0 {
            let input_index = declared.argument_count - 1;
            let parameter = declared
                .parameter_for_input(input_index)
                .expect("variadic procedure input has a declared parameter");
            let types::ColType::Array(element) = parameter.ctype else {
                return Ok(Err(sql_err!(
                    sqlstate::INTERNAL_ERROR,
                    "variadic procedure parameter is not an array"
                )));
            };
            completed[input_index] = Datum::Array {
                element,
                raw: match array::build(&variadic_values[..variadic_count], arena) {
                    Ok(raw) => raw,
                    Err(error) => return Ok(Err(error)),
                },
            };
            completed_type_oids[input_index] =
                match self
                    .storage
                    .routine_type_oid(parameter.ctype, parameter.user_type, txn.txid)
                {
                    Some(oid) => oid,
                    None => {
                        return Ok(Err(sql_err!(
                            sqlstate::INTERNAL_ERROR,
                            "variadic procedure parameter type identity is unavailable"
                        )));
                    }
                };
        }
        for input_index in 0..declared.argument_count {
            if provided[input_index] {
                continue;
            }
            let parameter = declared
                .parameter_for_input(input_index)
                .expect("procedure input signature has a declared parameter");
            completed[input_index] = if let Some(default) = parameter.mode.default() {
                let source = match arena.alloc_str(default.as_str()) {
                    Ok(source) => source,
                    Err(_) => return Ok(Err(eval::arena_full())),
                };
                let expression = match parser::parse_expression(source, arena) {
                    Ok(expression) => expression,
                    Err(error) => return Ok(Err(error)),
                };
                match eval::eval_full(expression, arena, params, &NoColumns, &hooks) {
                    Ok(value) => match eval::cast_to(value, parameter.ctype, arena) {
                        Ok(value) => value,
                        Err(error) => return Ok(Err(error)),
                    },
                    Err(error) => return Ok(Err(error)),
                }
            } else {
                return Ok(Err(sql_err!(
                    sqlstate::INTERNAL_ERROR,
                    "resolved procedure is missing a required argument"
                )));
            };
            completed_type_oids[input_index] =
                match self
                    .storage
                    .routine_type_oid(parameter.ctype, parameter.user_type, txn.txid)
                {
                    Some(oid) => oid,
                    None => {
                        return Ok(Err(sql_err!(
                            sqlstate::INTERNAL_ERROR,
                            "procedure parameter type identity is unavailable"
                        )));
                    }
                };
        }
        let routine = self
            .storage
            .routine_for_bound_call(
                slot,
                &completed_type_oids[..declared.argument_count],
                txn.txid,
            )
            .expect("resolved procedure call has a valid polymorphic binding");
        for (index, argument) in routine.arguments().iter().copied().enumerate() {
            completed[index] = match exec::coerce_routine_argument(
                completed[index],
                argument,
                &self.storage,
                txn.txid,
                arena,
            ) {
                Ok(value) => value,
                Err(error) => return Ok(Err(error)),
            };
        }
        let owner = self
            .storage
            .role_name(routine.ownership.owner_to(txn.txid).into(), txn.txid);
        let _security = routine
            .attributes
            .security_definer
            .then(|| eval::funcs::system::enter_current_user(owner.as_str()));
        let _config = match guc.enter_routine_configs(routine.configs()) {
            Ok(scope) => scope,
            Err(error) => return Ok(Err(error)),
        };
        if routine.language == crate::storage::RoutineLanguage::PlPgSql {
            let mut output_values = [Datum::Null; crate::storage::MAX_ROUTINE_ARGUMENTS];
            let output_count = match exec::execute_plpgsql_procedure(
                self,
                txn,
                cursors,
                guc,
                if transaction_context == exec::PlpgsqlTransactionContext::NonAtomic
                    && !routine.attributes.security_definer
                    && routine.configs().is_empty()
                {
                    exec::PlpgsqlTransactionContext::NonAtomic
                } else {
                    exec::PlpgsqlTransactionContext::Atomic
                },
                &routine,
                &completed[..declared.argument_count],
                arena,
                responder,
                &mut output_values,
            ) {
                Ok(count) => count,
                Err(error) => return Ok(Err(error)),
            };
            if output_count != 0 {
                let mut description =
                    [ColDesc::new("", types::oid::TEXT, -1); crate::storage::MAX_ROUTINE_ARGUMENTS];
                let mut output_index = 0usize;
                for parameter in routine.parameters() {
                    if !parameter.mode.is_output() {
                        continue;
                    }
                    let type_oid = match self.storage.routine_type_oid(
                        parameter.ctype,
                        parameter.user_type,
                        txn.txid,
                    ) {
                        Some(oid) => oid,
                        None => {
                            return Ok(Err(sql_err!(
                                sqlstate::INTERNAL_ERROR,
                                "procedure output type identity is unavailable"
                            )));
                        }
                    };
                    description[output_index] = ColDesc::new(
                        match arena.alloc_str(parameter.name.as_str()) {
                            Ok(name) => name,
                            Err(_) => return Ok(Err(eval::arena_full())),
                        },
                        type_oid,
                        parameter.ctype.typlen(),
                    );
                    output_values[output_index] =
                        match eval::cast_to(output_values[output_index], parameter.ctype, arena) {
                            Ok(value) => value,
                            Err(error) => return Ok(Err(error)),
                        };
                    output_index += 1;
                }
                responder.row_description(&description[..output_count])?;
                responder.data_row(&output_values[..output_count])?;
            }
            return responder.command_complete("CALL").map(|_| Ok(()));
        }
        if routine.language != crate::storage::RoutineLanguage::Sql {
            return Ok(Err(sql_err!(
                sqlstate::FEATURE_NOT_SUPPORTED,
                "procedure language is not executable"
            )));
        }
        let body = routine.body;
        let _formal_scope = exec::enter_routine_parameter_types(routine.arguments());
        let mut parser = match Parser::new(body.as_str(), arena).and_then(|parser| {
            parser.with_routine_parameters(routine.name.as_str(), routine.arguments())
        }) {
            Ok(parser) => parser,
            Err(error) => return Ok(Err(parse_error_to_sql(&error))),
        };
        let output_mark = responder.buffer.mark();
        let mut statements = [None; parser::MAX_LIST];
        let mut statement_count = 0usize;
        loop {
            let statement = match parser.next_stmt() {
                Ok(Some(statement)) => statement,
                Ok(None) => break,
                Err(error) => {
                    responder.buffer.truncate_to(output_mark);
                    return Ok(Err(parse_error_to_sql(&error)));
                }
            };
            if statement_count == statements.len() {
                return Ok(Err(sql_err!(
                    sqlstate::PROGRAM_LIMIT_EXCEEDED,
                    "procedure body has too many statements"
                )));
            }
            statements[statement_count] = Some(statement);
            statement_count += 1;
        }
        if statement_count == 0 {
            return Ok(Err(sql_err!(
                sqlstate::SYNTAX_ERROR,
                "procedure body is empty"
            )));
        }
        let mut output_parameters =
            [crate::storage::RoutineParameterDef::EMPTY; crate::storage::MAX_ROUTINE_ARGUMENTS];
        let mut output_count = 0usize;
        for parameter in routine.parameters() {
            if parameter.mode.is_output() {
                output_parameters[output_count] = *parameter;
                output_count += 1;
            }
        }
        let mut output_values = [Datum::Null; crate::storage::MAX_ROUTINE_ARGUMENTS];
        let mut output_rows = 0usize;
        for (index, statement) in statements[..statement_count]
            .iter()
            .map(|statement| statement.as_ref().expect("parsed procedure statement"))
            .enumerate()
        {
            let statement = match query::bind_stored_routine_statement(
                statement,
                &self.storage,
                slot,
                routine,
                txn.txid,
                arena,
                &completed[..declared.argument_count],
            ) {
                Ok(statement) => statement,
                Err(error) => return Ok(Err(error)),
            };
            if matches!(statement, Stmt::Commit) {
                return Ok(Err(sql_err!(
                    sqlstate::FEATURE_NOT_SUPPORTED,
                    "COMMIT is not allowed in an SQL function"
                )));
            }
            if matches!(statement, Stmt::Rollback | Stmt::RollbackToSavepoint(_)) {
                return Ok(Err(sql_err!(
                    sqlstate::FEATURE_NOT_SUPPORTED,
                    "ROLLBACK is not allowed in an SQL function"
                )));
            }
            // A top-level CALL has no enclosing query workspace; reclaim each
            // suppressed internal result exactly as the ordinary dispatcher
            // did before routine dispatch gained a non-resetting mode.
            self.work.reset();
            if output_count != 0 && index + 1 == statement_count {
                let output_query = match statement {
                    Stmt::Select(select) => Some(query::RoutineQuery::Select(select)),
                    Stmt::SetQuery(set) => Some(query::RoutineQuery::Set(set)),
                    _ => None,
                };
                let Some(output_query) = output_query else {
                    responder.buffer.truncate_to(output_mark);
                    return Ok(Err(sql_err!(
                        sqlstate::INVALID_FUNCTION_DEFINITION,
                        "SQL procedure with output parameters must end with a query"
                    )));
                };
                if let Err(error) = query::execute_bound_routine_query(
                    &output_query,
                    &self.storage,
                    slot,
                    routine,
                    txn.txid,
                    arena,
                    &completed[..declared.argument_count],
                    false,
                    &mut |values| {
                        if values.len() != output_count {
                            return Err(sql_err!(
                                sqlstate::SYNTAX_ERROR,
                                "SQL procedure query returns {} columns but {} output parameters were declared",
                                values.len(),
                                output_count
                            ));
                        }
                        if output_rows != 0 {
                            return Err(sql_err!(
                                sqlstate::CARDINALITY_VIOLATION,
                                "SQL procedure output query returned more than one row"
                            ));
                        }
                        let encoded = exec::encode_projected_pub(values, arena)?;
                        for (column, output) in output_values[..output_count].iter_mut().enumerate()
                        {
                            *output = exec::decode_projected_col_record(encoded, column, arena)?;
                        }
                        output_rows = 1;
                        Ok(())
                    },
                ) {
                    responder.buffer.truncate_to(output_mark);
                    return Ok(Err(error));
                }
                if output_rows == 0 {
                    output_values[..output_count].fill(Datum::Null);
                }
                continue;
            }
            match self.execute_routine_stmt(
                statement,
                arena,
                &completed[..declared.argument_count],
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
        if output_count != 0 {
            let mut description =
                [ColDesc::new("", types::oid::TEXT, -1); crate::storage::MAX_ROUTINE_ARGUMENTS];
            for index in 0..output_count {
                let parameter = output_parameters[index];
                let type_oid = match self.storage.routine_type_oid(
                    parameter.ctype,
                    parameter.user_type,
                    txn.txid,
                ) {
                    Some(oid) => oid,
                    None => {
                        return Ok(Err(sql_err!(
                            sqlstate::INTERNAL_ERROR,
                            "procedure output type identity is unavailable"
                        )));
                    }
                };
                description[index] = ColDesc::new(
                    match arena.alloc_str(parameter.name.as_str()) {
                        Ok(name) => name,
                        Err(_) => return Ok(Err(eval::arena_full())),
                    },
                    type_oid,
                    parameter.ctype.typlen(),
                );
                output_values[index] =
                    match eval::cast_to(output_values[index], parameter.ctype, arena) {
                        Ok(value) => value,
                        Err(error) => return Ok(Err(error)),
                    };
            }
            responder.row_description(&description[..output_count])?;
            responder.data_row(&output_values[..output_count])?;
        }
        responder.command_complete("CALL").map(|_| Ok(()))
    }

    fn fire_event_triggers(
        &mut self,
        invocation: EventTriggerInvocation<'_>,
        execution: EventTriggerExecution<'_, '_>,
    ) -> Result<(), SqlError> {
        let EventTriggerExecution {
            txn,
            cursors,
            guc,
            arena,
            responder,
        } = execution;
        let tag = invocation.tag();
        if !guc.event_triggers() {
            return Ok(());
        }
        let event = invocation.event();
        let _rewrite_scope = match invocation {
            EventTriggerInvocation::TableRewrite {
                relation_oid,
                reason,
            } => Some(eval::funcs::system::enter_table_rewrite_context(
                eval::funcs::system::TableRewriteContext {
                    relation_oid,
                    reason,
                },
            )),
            _ => None,
        };
        let mut last_name = None;
        while let Some((definition, routine)) = self
            .storage
            .event_triggers_visible_to(txn.txid)
            .filter(|(_, trigger)| {
                trigger.event == event
                    && trigger.tags.matches(tag)
                    && if txn.replication_apply {
                        trigger.enabled.fires_for_replication()
                    } else {
                        trigger.enabled.fires_for_origin()
                    }
                    && last_name.is_none_or(|last: SqlName| trigger.name.as_str() > last.as_str())
            })
            .min_by(|(_, left), (_, right)| left.name.as_str().cmp(right.name.as_str()))
            .map(|(_, trigger)| {
                (
                    trigger,
                    self.storage
                        .routine_for(usize::from(trigger.function), txn.txid),
                )
            })
        {
            last_name = Some(definition.name);
            let owner = self
                .storage
                .role_name(usize::from(routine.ownership.owner_to(txn.txid)), txn.txid);
            let _security = routine
                .attributes
                .security_definer
                .then(|| eval::funcs::system::enter_current_user(owner.as_str()));
            let _configuration = (!routine.configs().is_empty())
                .then(|| guc::enter_active_routine_configs(routine.configs()))
                .transpose()?;
            exec::execute_event_trigger(
                self,
                txn,
                cursors,
                guc,
                &routine,
                event.name(),
                tag,
                arena,
                responder,
            )?;
        }
        Ok(())
    }

    pub(crate) fn execute_login_event_triggers(
        &mut self,
        txn: &mut TxnState,
        cursors: &mut cursor::CursorPool,
        guc: &GucState,
        arena: &Arena,
        responder: &mut Responder,
    ) -> Result<(), SqlError> {
        self.ensure_txn(txn, TxnMode::Implicit, guc);
        let result = (|| {
            let _guc_scope = guc::enter_eval_scope(guc, txn);
            let database = self
                .storage
                .database_slot_by_oid(self.storage.current_database_oid(), txn.txid)
                .ok_or_else(|| {
                    sql_err!(sqlstate::INVALID_CATALOG_NAME, "database does not exist")
                })?;
            let database_name = self.storage.database_definition(database, txn.txid).name;
            eval::funcs::system::set_current_database(database_name.as_str());
            let session_user = guc.session_user();
            eval::funcs::system::set_session_user(session_user.as_str());
            let current_role = guc.current_role();
            eval::funcs::system::set_current_user(current_role.as_str());
            let path = self.storage.compute_path(
                guc.search_path().as_str(),
                current_role.as_str(),
                txn.txid,
            );
            self.storage.swap_path(path);
            self.fire_event_triggers(
                EventTriggerInvocation::Login,
                EventTriggerExecution {
                    txn,
                    cursors,
                    guc,
                    arena,
                    responder,
                },
            )?;
            self.commit_txn(txn, guc)?;
            self.commit_wal()
        })();
        if result.is_err() && txn.is_active() {
            self.rollback_txn(txn, guc);
        }
        result
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
        routine_transaction_context: exec::PlpgsqlTransactionContext,
        responder: &mut Responder,
    ) -> Result<Result<(), SqlError>, WireFull> {
        let prior_origin = txn.enter_ddl_origin();
        let result = self.execute_stmt_with_workspace(
            statement,
            arena,
            params,
            txn,
            sqlprep,
            cursors,
            guc,
            routine_transaction_context,
            responder,
            true,
            None,
        );
        txn.leave_ddl_origin(prior_origin);
        result
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
        let prior_origin = txn.enter_ddl_origin();
        let result = self.execute_stmt_with_workspace(
            statement,
            arena,
            params,
            txn,
            sqlprep,
            cursors,
            guc,
            exec::PlpgsqlTransactionContext::Atomic,
            responder,
            false,
            capture,
        );
        txn.leave_ddl_origin(prior_origin);
        result
    }

    /// Runs a typed dynamic utility through the same command boundary as
    /// static DDL while its executor remains in the procedural module.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn execute_dynamic_utility<F>(
        &mut self,
        statement: &Stmt,
        txn: &mut TxnState,
        cursors: &mut cursor::CursorPool,
        guc: &GucState,
        transaction_context: exec::PlpgsqlTransactionContext,
        arena: &Arena,
        responder: &mut Responder,
        execute: F,
    ) -> Result<Result<(), SqlError>, WireFull>
    where
        F: FnOnce(
            &mut Self,
            &mut TxnState,
            &mut Responder,
        ) -> Result<Result<(), SqlError>, WireFull>,
    {
        if txn.failed {
            return Ok(Err(SqlError {
                sqlstate: SqlState::known(sqlstate::IN_FAILED_SQL_TRANSACTION),
                message: stack_format!(
                    192,
                    "current transaction is aborted, commands ignored until end of transaction block"
                ),
            }));
        }
        if transaction_context == exec::PlpgsqlTransactionContext::Atomic
            && let Some(command) = top_level_only_command(statement)
        {
            return Ok(Err(sql_err!(
                sqlstate::ACTIVE_SQL_TRANSACTION,
                "{} cannot run inside a transaction block",
                command
            )));
        }
        if txn.read_only && statement_writes(statement) {
            return Ok(Err(sql_err!(
                sqlstate::READ_ONLY_SQL_TRANSACTION,
                "cannot execute {} in a read-only transaction block",
                statement_tag(statement)
            )));
        }
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
        if let Err(error) = self.begin_command_snapshot(txn, true) {
            return Ok(Err(error));
        }
        let event_tag = event_trigger_tag(statement);
        if let Some(tag) = event_tag
            && let Err(error) = self.fire_event_triggers(
                EventTriggerInvocation::DdlCommandStart { tag },
                EventTriggerExecution {
                    txn,
                    cursors,
                    guc,
                    arena,
                    responder,
                },
            )
        {
            return Ok(Err(error));
        }
        if let Some((slot, reason)) = table_rewrite_target(statement, &self.storage, txn.txid)
            && let Err(error) = self.fire_event_triggers(
                EventTriggerInvocation::TableRewrite {
                    relation_oid: catalog::user_table_oid(slot),
                    reason,
                },
                EventTriggerExecution {
                    txn,
                    cursors,
                    guc,
                    arena,
                    responder,
                },
            )
        {
            return Ok(Err(error));
        }
        let event_ddl_mark = txn.ddl().len();
        let event_ddl_origin = txn.ddl_origin();
        let event_drop = event_tag.is_some_and(|tag| {
            event_trigger_drop_command(statement)
                && has_event_trigger(
                    &self.storage,
                    txn,
                    guc,
                    ast::EventTriggerEvent::SqlDrop,
                    tag,
                )
        });
        let event_end = event_tag.is_some_and(|tag| {
            has_event_trigger(
                &self.storage,
                txn,
                guc,
                ast::EventTriggerEvent::DdlCommandEnd,
                tag,
            )
        });
        let event_before = if event_drop || event_end {
            match event_trigger::capture_before(&self.storage, txn.txid, statement, arena) {
                Ok(before) => before,
                Err(error) => return Ok(Err(error)),
            }
        } else {
            event_trigger::BeforeDdl::EMPTY
        };
        let outcome = execute(self, txn, responder);
        if matches!(outcome, Ok(Ok(())))
            && (event_drop || event_end)
            && let Some(tag) = event_tag
        {
            let mut commands = [event_trigger::DdlCommand::EMPTY; event_trigger::MAX_EVENT_OBJECTS];
            let mut drops = [event_trigger::DroppedObject::EMPTY; event_trigger::MAX_EVENT_OBJECTS];
            let (command_count, drop_count) = match event_trigger::collect(
                &self.storage,
                txn.txid,
                statement,
                tag,
                event_trigger::CollectChanges {
                    before: event_before,
                    undo: &txn.ddl()[event_ddl_mark..],
                    undo_origins: &txn.ddl_origins()[event_ddl_mark..],
                    origin: event_ddl_origin,
                    in_extension: txn.in_extension_script(),
                },
                event_trigger::EventGraphs {
                    commands: &mut commands,
                    drops: &mut drops,
                },
            ) {
                Ok(counts) => counts,
                Err(error) => return Ok(Err(error)),
            };
            if event_drop {
                let _scope = event_trigger::enter_dropped_objects(&drops[..drop_count]);
                if let Err(error) = self.fire_event_triggers(
                    EventTriggerInvocation::SqlDrop { tag },
                    EventTriggerExecution {
                        txn,
                        cursors,
                        guc,
                        arena,
                        responder,
                    },
                ) {
                    return Ok(Err(error));
                }
            }
            if event_end {
                let _scope = event_trigger::enter_ddl_commands(&commands[..command_count]);
                if let Err(error) = self.fire_event_triggers(
                    EventTriggerInvocation::DdlCommandEnd { tag },
                    EventTriggerExecution {
                        txn,
                        cursors,
                        guc,
                        arena,
                        responder,
                    },
                ) {
                    return Ok(Err(error));
                }
            }
        }
        outcome
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
        routine_transaction_context: exec::PlpgsqlTransactionContext,
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
        let _configuration_reload_scope = ConfigurationReloadScope::new(self, guc);
        let _guc_eval_scope = guc::enter_eval_scope(guc, txn);
        // Reclaim the shared execution arena from the previous top-level
        // statement. A routine entered from an active query keeps that query's
        // materialized state alive until evaluation returns.
        if reset_workspace {
            self.work.reset();
        }
        self.refresh_prepared_transaction_catalog();
        // Drop any diagnostic detail a swallowed error left behind, and
        // install this session's effective search path for the statement:
        // every name resolution below reads it from storage.
        let _ = eval::take_diagnostic();
        exec::reset_record_shapes();
        for (slot, composite) in self.storage.composites_with_slots_visible_to(txn.txid) {
            if let Err(error) = exec::register_named_composite_shape(
                slot as u16,
                composite.name.as_str(),
                composite.fields(),
                &self.storage,
                txn.txid,
            ) {
                return Ok(Err(error));
            }
        }
        let database = self
            .storage
            .database_slot_by_oid(self.storage.current_database_oid(), txn.txid)
            .expect("selected database remains visible");
        let database_name = self.storage.database_definition(database, txn.txid).name;
        eval::funcs::system::set_current_database(database_name.as_str());
        let current_role = if reset_workspace {
            let session_user = guc.session_user();
            eval::funcs::system::set_session_user(session_user.as_str());
            let current_role = guc.current_role();
            eval::funcs::system::set_current_user(current_role.as_str());
            current_role
        } else {
            eval::funcs::system::current_user_owned()
        };
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
            let mut reset_values = [crate::util::StackStr::<256>::new(); SETTING_NAMES.len()];
            let mut sources = ["default"; SETTING_NAMES.len()];
            let mut setting_count = 0;
            for &name in SETTING_NAMES {
                if let Some(value) = self.fixed_setting_for(name, txn) {
                    names[setting_count] = name;
                    values[setting_count] = value;
                    reset_values[setting_count] =
                        guc.transaction_reset_owned(name).unwrap_or(value);
                    sources[setting_count] = txn.setting_source(name).unwrap_or("default");
                    setting_count += 1;
                } else if let Some(value) = guc.get_owned(name) {
                    names[setting_count] = name;
                    values[setting_count] = value;
                    reset_values[setting_count] = guc.reset_owned(name).unwrap_or(value);
                    sources[setting_count] = guc.source(name);
                    setting_count += 1;
                }
            }
            if let Err(e) = eval::funcs::system::set_session_settings(
                &names[..setting_count],
                &values[..setting_count],
                &reset_values[..setting_count],
                &sources[..setting_count],
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
                Stmt::Commit
                    | Stmt::Rollback
                    | Stmt::RollbackToSavepoint(_)
                    | Stmt::PrepareTransaction(_)
            )
        {
            return Ok(Err(SqlError {
                sqlstate: SqlState::known(sqlstate::IN_FAILED_SQL_TRANSACTION),
                message: stack_format!(
                    192,
                    "current transaction is aborted, commands ignored until end of transaction block"
                ),
            }));
        }
        if routine_transaction_context == exec::PlpgsqlTransactionContext::Atomic
            && let Some(command) = top_level_only_command(statement)
        {
            return Ok(Err(sql_err!(
                sqlstate::ACTIVE_SQL_TRANSACTION,
                "{} cannot run inside a transaction block",
                command
            )));
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
        // A data-modifying WITH statement lowers the command snapshot itself;
        // the shared command boundary guarantees it never leaks into the next
        // SQL or fast-path statement.
        let takes_snapshot = !matches!(
            statement,
            Stmt::Begin(_)
                | Stmt::Commit
                | Stmt::Rollback
                | Stmt::PrepareTransaction(_)
                | Stmt::CommitPrepared(_)
                | Stmt::RollbackPrepared(_)
                | Stmt::Savepoint(_)
                | Stmt::ReleaseSavepoint(_)
                | Stmt::RollbackToSavepoint(_)
                | Stmt::LockTable { .. }
                | Stmt::Set { .. }
                | Stmt::SetCatalog(_)
                | Stmt::Reset(_)
                | Stmt::SetTransaction { .. }
                | Stmt::SetTransactionSnapshot(_)
                | Stmt::Show(_)
                | Stmt::ShowAll
        );
        if let Err(error) = self.begin_command_snapshot(txn, takes_snapshot) {
            return Ok(Err(error));
        }
        let event_tag = event_trigger_tag(statement);
        if let Some(tag) = event_tag
            && let Err(error) = self.fire_event_triggers(
                EventTriggerInvocation::DdlCommandStart { tag },
                EventTriggerExecution {
                    txn,
                    cursors,
                    guc,
                    arena,
                    responder,
                },
            )
        {
            return Ok(Err(error));
        }
        if let Some((slot, reason)) = table_rewrite_target(statement, &self.storage, txn.txid)
            && let Err(error) = self.fire_event_triggers(
                EventTriggerInvocation::TableRewrite {
                    relation_oid: catalog::user_table_oid(slot),
                    reason,
                },
                EventTriggerExecution {
                    txn,
                    cursors,
                    guc,
                    arena,
                    responder,
                },
            )
        {
            return Ok(Err(error));
        }
        // Event-trigger introspection is scoped to mutations performed by this
        // command. DDL executed by a start trigger has already completed its
        // own nested invocation and must not leak into the outer command set.
        let event_ddl_mark = txn.ddl().len();
        let event_ddl_origin = txn.ddl_origin();
        let event_drop = event_tag.is_some_and(|tag| {
            event_trigger_drop_command(statement)
                && has_event_trigger(
                    &self.storage,
                    txn,
                    guc,
                    ast::EventTriggerEvent::SqlDrop,
                    tag,
                )
        });
        let event_end = event_tag.is_some_and(|tag| {
            has_event_trigger(
                &self.storage,
                txn,
                guc,
                ast::EventTriggerEvent::DdlCommandEnd,
                tag,
            )
        });
        let event_before = if event_drop || event_end {
            match event_trigger::capture_before(&self.storage, txn.txid, statement, arena) {
                Ok(before) => before,
                Err(error) => return Ok(Err(error)),
            }
        } else {
            event_trigger::BeforeDdl::EMPTY
        };
        let outcome = match statement {
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
            Stmt::With { .. } if reset_workspace => self.execute_modification_resumable(
                statement, arena, params, txn, sqlprep, cursors, guc, responder,
            ),
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
                columns,
                or_replace,
                security,
                security_barrier,
                check_option,
                sql,
            } => exec::create_view(
                &mut self.storage,
                &mut self.wal,
                txn,
                exec::CreateViewCommand {
                    name,
                    columns,
                    or_replace: *or_replace,
                    security: *security,
                    security_barrier: *security_barrier,
                    check_option: *check_option,
                    sql,
                    raw_path: guc.search_path().as_str(),
                },
                arena,
                responder,
            ),
            Stmt::AlterView {
                name,
                if_exists,
                action,
            } => exec::alter_view(
                &mut self.storage,
                &mut self.wal,
                txn,
                exec::AlterViewCommand {
                    name: *name,
                    if_exists: *if_exists,
                    action: *action,
                },
                arena,
                responder,
            ),
            Stmt::AlterMaterializedView {
                name,
                if_exists,
                action,
            } => exec::alter_materialized_view(
                &mut self.storage,
                &mut self.wal,
                txn,
                &mut self.dml_scratch,
                *name,
                *if_exists,
                *action,
                arena,
                guc.seq_session(),
                responder,
            ),
            Stmt::CreateRule(rule) => exec::create_rule(
                &mut self.storage,
                &mut self.wal,
                txn,
                rule,
                guc.search_path().as_str(),
                arena,
                responder,
            ),
            Stmt::AlterRule {
                name,
                table,
                new_name,
            } => exec::alter_rule(
                &mut self.storage,
                &mut self.wal,
                txn,
                name,
                *table,
                new_name,
                responder,
            ),
            Stmt::DropRule(rule) => {
                exec::drop_rule(&mut self.storage, &mut self.wal, txn, *rule, responder)
            }
            Stmt::CreateRoutine(routine) => exec::create_routine(
                &mut self.storage,
                &mut self.wal,
                txn,
                routine,
                guc,
                arena,
                responder,
            ),
            Stmt::CreateAggregate(aggregate) => exec::create_aggregate(
                &mut self.storage,
                &mut self.wal,
                txn,
                aggregate,
                arena,
                responder,
            ),
            Stmt::CreateCast(cast) => {
                exec::create_cast(&mut self.storage, &mut self.wal, txn, cast, responder)
            }
            Stmt::DropCast(cast) => {
                exec::drop_cast(&mut self.storage, &mut self.wal, txn, *cast, responder)
            }
            Stmt::CreateTransform(_) | Stmt::DropTransform(_) => Ok(Err(sql_err!(
                sqlstate::FEATURE_NOT_SUPPORTED,
                "transforms require procedural-language type hooks, which pos3ql does not host"
            ))),
            Stmt::Load(_) => Ok(Err(sql_err!(
                sqlstate::FEATURE_NOT_SUPPORTED,
                "LOAD is not supported; pos3ql does not load native shared libraries"
            ))),
            Stmt::SecurityLabel { .. } => Ok(Err(sql_err!(
                sqlstate::FEATURE_NOT_SUPPORTED,
                "SECURITY LABEL requires a native security label provider, which pos3ql does not host"
            ))),
            Stmt::CreateOperator(operator) => {
                exec::create_operator(&mut self.storage, &mut self.wal, txn, operator, responder)
            }
            Stmt::AlterOperator { identity, action } => exec::alter_operator(
                &mut self.storage,
                &mut self.wal,
                txn,
                *identity,
                *action,
                responder,
            ),
            Stmt::DropOperator {
                identities,
                if_exists,
                cascade,
            } => exec::drop_operators(
                &mut self.storage,
                &mut self.wal,
                txn,
                identities,
                *if_exists,
                *cascade,
                responder,
            ),
            Stmt::CreateOperatorFamily { name, .. } => exec::create_operator_family(
                &mut self.storage,
                &mut self.wal,
                txn,
                *name,
                responder,
            ),
            Stmt::AlterOperatorFamily { name, action, .. } => exec::alter_operator_family(
                &mut self.storage,
                &mut self.wal,
                txn,
                *name,
                *action,
                responder,
            ),
            Stmt::DropOperatorFamily {
                names,
                if_exists,
                cascade,
                ..
            } => exec::drop_operator_families(
                &mut self.storage,
                &mut self.wal,
                txn,
                names,
                *if_exists,
                *cascade,
                responder,
            ),
            Stmt::CreateOperatorClass(class) => {
                exec::create_operator_class(&mut self.storage, &mut self.wal, txn, class, responder)
            }
            Stmt::AlterOperatorClass { name, action, .. } => exec::alter_operator_class(
                &mut self.storage,
                &mut self.wal,
                txn,
                *name,
                *action,
                responder,
            ),
            Stmt::DropOperatorClass {
                names,
                if_exists,
                cascade,
                ..
            } => exec::drop_operator_classes(
                &mut self.storage,
                &mut self.wal,
                txn,
                names,
                *if_exists,
                *cascade,
                responder,
            ),
            Stmt::CreateExtension {
                name,
                if_not_exists,
                schema,
                version,
                cascade,
            } => self.create_extension(
                name,
                *if_not_exists,
                *schema,
                *version,
                *cascade,
                arena,
                params,
                txn,
                sqlprep,
                cursors,
                guc,
                responder,
            ),
            Stmt::AlterExtension { name, action } => match action {
                ast::AlterExtensionAction::Member { add, object } => {
                    match exec::alter_extension_membership(
                        &mut self.storage,
                        txn,
                        name,
                        *add,
                        *object,
                    ) {
                        Ok(()) => {
                            responder.command_complete("ALTER EXTENSION")?;
                            Ok(Ok(()))
                        }
                        Err(error) => Ok(Err(error)),
                    }
                }
                ast::AlterExtensionAction::SetSchema(schema) => self.alter_extension_schema(
                    name, schema, arena, params, txn, sqlprep, cursors, guc, responder,
                ),
                ast::AlterExtensionAction::Update { version } => {
                    let owner = guc.current_role();
                    let plan = match exec::prepare_update_extension(
                        &mut self.storage,
                        txn,
                        name,
                        *version,
                    ) {
                        Ok(plan) => plan,
                        Err(error) => return Ok(Err(error)),
                    };
                    let final_package = match self.execute_extension_plan(
                        plan,
                        owner.as_str(),
                        arena,
                        params,
                        txn,
                        sqlprep,
                        cursors,
                        guc,
                        responder,
                    )? {
                        Ok(package) => package,
                        Err(error) => return Ok(Err(error)),
                    };
                    if !final_package.comment.as_str().is_empty() {
                        let outcome = responder.without_command_complete(|responder| {
                            exec::comment(
                                &mut self.storage,
                                &mut self.wal,
                                txn,
                                &ast::CommentTarget::Extension(name),
                                Some(final_package.comment.as_str()),
                                arena,
                                responder,
                            )
                        })?;
                        if let Err(error) = outcome {
                            return Ok(Err(error));
                        }
                    }
                    responder.command_complete("ALTER EXTENSION")?;
                    Ok(Ok(()))
                }
            },
            Stmt::DropExtension {
                names,
                if_exists,
                cascade,
            } => self.drop_extension(
                names, *if_exists, *cascade, arena, params, txn, sqlprep, cursors, guc, responder,
            ),
            Stmt::AlterMaterializedViewExtensionDependency {
                name,
                extension,
                enabled,
            } => exec::alter_materialized_view_extension_dependency(
                &mut self.storage,
                txn,
                name,
                extension,
                *enabled,
                responder,
            ),
            Stmt::Call {
                name,
                arguments,
                argument_names,
                variadic,
            } => self.execute_call(
                *name,
                arguments,
                argument_names,
                *variadic,
                arena,
                params,
                txn,
                sqlprep,
                cursors,
                guc,
                routine_transaction_context,
                responder,
            ),
            Stmt::Do { body } => self.execute_do(
                body,
                arena,
                txn,
                cursors,
                guc,
                routine_transaction_context,
                responder,
            ),
            Stmt::CreateLanguage(language) => Ok(Err(sql_err!(
                sqlstate::FEATURE_NOT_SUPPORTED,
                "{}",
                if language.handler.is_some() {
                    "native procedural-language handlers are not supported"
                } else {
                    "handlerless CREATE LANGUAGE is not supported"
                }
            ))),
            Stmt::AlterLanguage { .. } | Stmt::DropLanguage { .. } => Ok(Err(sql_err!(
                sqlstate::FEATURE_NOT_SUPPORTED,
                "procedural-language catalog mutation is not supported"
            ))),
            Stmt::AlterRoutine {
                kind,
                routine,
                actions,
            } => exec::alter_routine(
                &mut self.storage,
                &mut self.wal,
                txn,
                exec::AlterRoutineCommand {
                    kind: *kind,
                    identity: routine,
                    actions,
                    guc: Some(guc),
                },
                responder,
            ),
            Stmt::AlterAggregate { aggregate, action } => exec::alter_aggregate(
                &mut self.storage,
                &mut self.wal,
                txn,
                aggregate,
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
                    targets: exec::DropRoutineTargets::Routines(functions),
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
                    targets: exec::DropRoutineTargets::Routines(procedures),
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
                    targets: exec::DropRoutineTargets::Routines(routines),
                    if_exists: *if_exists,
                    cascade: *cascade,
                    kind: crate::sql::ast::RoutineTargetKind::Either,
                },
                responder,
            ),
            Stmt::DropAggregate {
                aggregates,
                if_exists,
                cascade,
            } => exec::drop_aggregate(
                &mut self.storage,
                &mut self.wal,
                txn,
                aggregates,
                *if_exists,
                *cascade,
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
            Stmt::CreateCollation(command) => {
                exec::create_collation(&mut self.storage, &mut self.wal, txn, command, responder)
            }
            Stmt::AlterCollation { name, action } => exec::alter_collation(
                &mut self.storage,
                &mut self.wal,
                txn,
                name,
                *action,
                responder,
            ),
            Stmt::DropCollation {
                name,
                if_exists,
                cascade,
            } => exec::drop_collation(
                &mut self.storage,
                &mut self.wal,
                txn,
                &mut self.dml_scratch,
                name,
                *if_exists,
                *cascade,
                arena,
                guc.seq_session(),
                responder,
            ),
            Stmt::CreateConversion(command) => {
                exec::create_conversion(&mut self.storage, &mut self.wal, txn, command, responder)
            }
            Stmt::AlterConversion { name, action } => exec::alter_conversion(
                &mut self.storage,
                &mut self.wal,
                txn,
                name,
                *action,
                responder,
            ),
            Stmt::DropConversion {
                name,
                if_exists,
                cascade,
            } => exec::drop_conversion(
                &mut self.storage,
                &mut self.wal,
                txn,
                name,
                *if_exists,
                *cascade,
                responder,
            ),
            Stmt::CreateForeignDataWrapper(command) => exec::create_foreign_data_wrapper(
                &mut self.storage,
                &mut self.wal,
                txn,
                command,
                responder,
            ),
            Stmt::CreateForeignServer(command) => exec::create_foreign_server(
                &mut self.storage,
                &mut self.wal,
                txn,
                command,
                responder,
            ),
            Stmt::CreateUserMapping(command) => {
                exec::create_user_mapping(&mut self.storage, &mut self.wal, txn, command, responder)
            }
            Stmt::CreateForeignTable(command) => exec::create_foreign_table(
                &mut self.storage,
                &mut self.wal,
                txn,
                command,
                arena,
                responder,
            ),
            Stmt::AlterForeignDataWrapper { name, action } => exec::alter_foreign_data_wrapper(
                &mut self.storage,
                &mut self.wal,
                txn,
                name,
                *action,
                responder,
            ),
            Stmt::DropForeignDataWrapper {
                names,
                if_exists,
                cascade,
            } => exec::drop_foreign_data_wrapper(
                &mut self.storage,
                &mut self.wal,
                txn,
                names,
                *if_exists,
                *cascade,
                responder,
            ),
            Stmt::AlterForeignServer { name, action } => exec::alter_foreign_server(
                &mut self.storage,
                &mut self.wal,
                txn,
                name,
                *action,
                responder,
            ),
            Stmt::DropForeignServer {
                names,
                if_exists,
                cascade,
            } => exec::drop_foreign_server(
                &mut self.storage,
                &mut self.wal,
                txn,
                names,
                *if_exists,
                *cascade,
                responder,
            ),
            Stmt::AlterUserMapping(command) => {
                exec::alter_user_mapping(&mut self.storage, &mut self.wal, txn, command, responder)
            }
            Stmt::DropUserMapping(command) => {
                exec::drop_user_mapping(&mut self.storage, &mut self.wal, txn, command, responder)
            }
            Stmt::DropForeignTable(command) => {
                exec::drop_foreign_table(&mut self.storage, &mut self.wal, txn, command, responder)
            }
            Stmt::ImportForeignSchema(command) => exec::import_foreign_schema(
                &mut self.storage,
                &mut self.wal,
                txn,
                command,
                arena,
                responder,
            ),
            Stmt::AlterForeignTable(command) => exec::alter_foreign_table(
                &mut self.storage,
                &mut self.wal,
                txn,
                command,
                exec::ForeignTableAlterRuntime {
                    scratch: &mut self.dml_scratch,
                    arena,
                    sequence: guc.seq_session(),
                },
                responder,
            ),
            Stmt::CreateTextSearchParser(command) => exec::create_text_search_parser(
                &mut self.storage,
                &mut self.wal,
                txn,
                command,
                responder,
            ),
            Stmt::CreateTextSearchTemplate(command) => exec::create_text_search_template(
                &mut self.storage,
                &mut self.wal,
                txn,
                command,
                responder,
            ),
            Stmt::CreateTextSearchDictionary(command) => exec::create_text_search_dictionary(
                &mut self.storage,
                &mut self.wal,
                txn,
                command,
                responder,
            ),
            Stmt::CreateTextSearchConfiguration(command) => exec::create_text_search_configuration(
                &mut self.storage,
                &mut self.wal,
                txn,
                command,
                responder,
            ),
            Stmt::AlterTextSearch { kind, name, action } => exec::alter_text_search(
                &mut self.storage,
                &mut self.wal,
                txn,
                *kind,
                name,
                *action,
                responder,
            ),
            Stmt::DropTextSearch {
                kind,
                name,
                if_exists,
                cascade,
            } => exec::drop_text_search(
                &mut self.storage,
                &mut self.wal,
                txn,
                *kind,
                name,
                *if_exists,
                *cascade,
                responder,
            ),
            Stmt::CreateEventTrigger(command) => exec::create_event_trigger(
                &mut self.storage,
                &mut self.wal,
                txn,
                command,
                responder,
            ),
            Stmt::AlterEventTrigger { name, action } => exec::alter_event_trigger(
                &mut self.storage,
                &mut self.wal,
                txn,
                name,
                *action,
                responder,
            ),
            Stmt::DropEventTrigger {
                name,
                if_exists,
                cascade,
            } => exec::drop_event_trigger(
                &mut self.storage,
                &mut self.wal,
                txn,
                name,
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
                publish_via_partition_root,
                publish_generated_columns,
            } => exec::create_publication(
                &mut self.storage,
                &mut self.wal,
                txn,
                name,
                *all_tables,
                tables,
                schemas,
                *publish,
                *publish_via_partition_root,
                *publish_generated_columns,
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
            Stmt::CreateTrigger(trigger) => exec::create_trigger(
                &mut self.storage,
                &mut self.wal,
                txn,
                trigger,
                arena,
                responder,
            ),
            Stmt::DropTrigger {
                trigger,
                if_exists,
                cascade,
            } => exec::drop_trigger(
                &mut self.storage,
                &mut self.wal,
                txn,
                trigger,
                *if_exists,
                *cascade,
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
            Stmt::CreatePolicy(policy) => exec::create_policy(
                &mut self.storage,
                &mut self.wal,
                txn,
                policy,
                arena,
                responder,
            ),
            Stmt::AlterPolicy(policy) => exec::alter_policy(
                &mut self.storage,
                &mut self.wal,
                txn,
                policy,
                arena,
                responder,
            ),
            Stmt::DropPolicy {
                policy,
                if_exists,
                cascade,
            } => exec::drop_policy(
                &mut self.storage,
                &mut self.wal,
                txn,
                policy,
                *if_exists,
                *cascade,
                responder,
            ),
            Stmt::CreateStatistics(statistics) => exec::create_statistics(
                &mut self.storage,
                &mut self.wal,
                txn,
                statistics,
                arena,
                responder,
            ),
            Stmt::AlterStatistics { name, action } => exec::alter_statistics(
                &mut self.storage,
                &mut self.wal,
                txn,
                name,
                *action,
                responder,
            ),
            Stmt::DropStatistics {
                names,
                if_exists,
                cascade,
            } => exec::drop_statistics(
                &mut self.storage,
                &mut self.wal,
                txn,
                names,
                *if_exists,
                *cascade,
                responder,
            ),
            Stmt::CreateTableAs {
                name,
                columns,
                sql,
                with_data,
                if_not_exists,
                kind,
                options,
            } => exec::create_table_as(
                &mut self.storage,
                &mut self.wal,
                txn,
                name,
                columns,
                sql,
                *with_data,
                *if_not_exists,
                *kind == ast::CreateTableAsKind::MaterializedView,
                *options,
                guc.search_path().as_str(),
                guc.seq_session(),
                arena,
                params,
                responder,
            ),
            Stmt::RefreshMaterializedView { name } => exec::refresh_materialized_view(
                &mut self.storage,
                &mut self.wal,
                txn,
                name,
                guc.seq_session(),
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
                action,
            } => exec::alter_sequence(
                &mut self.storage,
                &mut self.wal,
                txn,
                exec::AlterSequenceCommand {
                    name,
                    if_exists: *if_exists,
                    action: *action,
                },
                guc.seq_session(),
                arena,
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
                &mut self.dml_scratch,
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
            Stmt::CreateComposite { name, fields } => exec::create_composite(
                &mut self.storage,
                &mut self.wal,
                txn,
                name,
                fields,
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
            } => exec::drop_type(
                &mut self.storage,
                &mut self.wal,
                txn,
                &mut self.dml_scratch,
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
                build,
                scope,
                if_not_exists,
                columns,
                include_columns,
                nulls_not_distinct,
                predicate,
                predicate_text,
                options,
                tablespace,
                unique,
            } => {
                let default_tablespace = guc.default_tablespace();
                exec::create_index(
                    &mut self.storage,
                    &mut self.wal,
                    txn,
                    exec::CreateIndexCommand {
                        name: *name,
                        table: *table,
                        build: *build,
                        scope: *scope,
                        if_not_exists: *if_not_exists,
                        columns,
                        include_columns,
                        nulls_not_distinct: *nulls_not_distinct,
                        predicate: *predicate,
                        predicate_text: *predicate_text,
                        options: *options,
                        tablespace: (*tablespace).or_else(|| {
                            (!default_tablespace.as_str().is_empty())
                                .then_some(default_tablespace.as_str())
                        }),
                        unique: *unique,
                    },
                    arena,
                    responder,
                )
            }
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
            Stmt::AlterIndexesTablespace {
                source,
                owners,
                target,
                nowait,
            } => exec::alter_indexes_tablespace(
                &mut self.storage,
                &mut self.wal,
                txn,
                exec::AlterIndexesTablespaceCommand {
                    source,
                    owners,
                    target,
                    nowait: *nowait,
                },
                responder,
            ),
            Stmt::AlterTablesTablespace {
                source,
                owners,
                target,
                nowait,
            } => exec::alter_tables_tablespace(
                &mut self.storage,
                &mut self.wal,
                txn,
                exec::AlterTablesTablespaceCommand {
                    source,
                    owners,
                    target,
                    nowait: *nowait,
                },
                responder,
            ),
            Stmt::DropIndex {
                names,
                if_exists,
                build,
                cascade,
            } => exec::drop_index(
                &mut self.storage,
                &mut self.wal,
                txn,
                exec::DropIndexCommand {
                    names,
                    if_exists: *if_exists,
                    build: *build,
                    cascade: *cascade,
                },
                responder,
            ),
            Stmt::Reindex {
                target,
                name,
                options,
            } => exec::reindex(
                &mut self.storage,
                &mut self.wal,
                txn,
                *target,
                *name,
                *options,
                responder,
            ),
            Stmt::Cluster { target, verbose } => exec::cluster(
                &mut self.storage,
                &mut self.wal,
                txn,
                *target,
                *verbose,
                responder,
            ),
            Stmt::CreateTablespace {
                name,
                owner,
                location,
                options,
            } => exec::create_tablespace(
                &mut self.storage,
                &mut self.wal,
                txn,
                exec::CreateTablespaceCommand {
                    name,
                    owner: *owner,
                    location,
                    options: *options,
                },
                responder,
            ),
            Stmt::AlterTablespace { name, action } => exec::alter_tablespace(
                &mut self.storage,
                &mut self.wal,
                txn,
                name,
                *action,
                responder,
            ),
            Stmt::DropTablespace { name, if_exists } => exec::drop_tablespace(
                &mut self.storage,
                &mut self.wal,
                txn,
                name,
                *if_exists,
                responder,
            ),
            Stmt::CreateAccessMethod {
                name,
                method_type,
                handler,
            } => exec::create_access_method(
                &mut self.storage,
                &mut self.wal,
                txn,
                name,
                *method_type,
                *handler,
                responder,
            ),
            Stmt::DropAccessMethod {
                names,
                if_exists,
                cascade,
            } => exec::drop_access_method(
                &mut self.storage,
                &mut self.wal,
                txn,
                names,
                *if_exists,
                *cascade,
                responder,
            ),
            Stmt::CreateDatabase { name, options } => {
                let template = options.template.unwrap_or("template1");
                let mut connections = self.database_connection_count(template, txn.txid);
                if self
                    .storage
                    .database_slot(template, txn.txid)
                    .is_some_and(|slot| {
                        self.storage.database(slot).oid == self.storage.current_database_oid()
                    })
                {
                    connections = connections.saturating_sub(1);
                }
                exec::create_database(
                    &mut self.storage,
                    &mut self.wal,
                    txn,
                    name,
                    *options,
                    connections,
                    responder,
                )
            }
            Stmt::AlterDatabase { name, action } => {
                let connections = self.database_connection_count(name, txn.txid);
                exec::alter_database(
                    &mut self.storage,
                    &mut self.wal,
                    txn,
                    exec::AlterDatabaseCommand {
                        name,
                        action: *action,
                        active_connections: connections,
                        guc,
                    },
                    responder,
                )
            }
            Stmt::DropDatabase {
                name,
                if_exists,
                force,
            } => {
                let connections = self.database_connection_count(name, txn.txid);
                exec::drop_database(
                    &mut self.storage,
                    &mut self.wal,
                    txn,
                    exec::DropDatabaseCommand {
                        name,
                        if_exists: *if_exists,
                        force: *force,
                        active_connections: connections,
                    },
                    responder,
                )
            }
            Stmt::Insert(_) | Stmt::Update(_) | Stmt::Delete(_) | Stmt::Merge(_)
                if reset_workspace =>
            {
                debug_assert!(capture.is_none());
                self.execute_modification_resumable(
                    statement, arena, params, txn, sqlprep, cursors, guc, responder,
                )
            }
            Stmt::Insert(_) | Stmt::Update(_) | Stmt::Delete(_) => Self::execute_data_modification(
                &mut self.storage,
                &mut self.dml_scratch,
                &self.work,
                statement,
                exec::DmlAuthorization::Invoker,
                txn,
                params,
                guc,
                responder,
                capture,
                self.current_conn_id,
            ),
            Stmt::Merge(_) => Self::execute_merge(
                &mut self.storage,
                &mut self.dml_scratch,
                arena,
                statement,
                txn,
                params,
                guc,
                responder,
                capture,
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
                &mut self.dml_scratch,
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
                let resolved = match resolve_create_schema(*name, *authorization, guc) {
                    Ok(resolved) => resolved,
                    Err(error) => return Ok(Err(error)),
                };
                let name = resolved.name.as_str();
                let out = exec::create_schema(
                    &mut self.storage,
                    &mut self.wal,
                    txn,
                    name,
                    resolved.authorization.as_ref().map(|role| role.as_str()),
                    *if_not_exists,
                    responder,
                )?;
                if let Err(e) = out {
                    return Ok(Err(e));
                }
                let prior_role = guc.current_role();
                let prior_user = eval::funcs::system::current_user_owned();
                if let Some(owner) = resolved.authorization.as_ref() {
                    guc.set_role(owner.as_str(), true);
                    eval::funcs::system::set_current_user(owner.as_str());
                }
                // Schema elements run with the new schema as their creation
                // target; an element naming a different schema is refused, as
                // PostgreSQL has it (42P15).
                let elements_result = (|| {
                    for element in *elements {
                        let requalified = match requalify_schema_element(element, name, arena) {
                            Ok(r) => r,
                            Err(e) => return Ok(Err(e)),
                        };
                        let result = if let Stmt::CreateView {
                            name,
                            columns,
                            or_replace,
                            security,
                            security_barrier,
                            check_option,
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
                            let path =
                                self.storage
                                    .compute_path(schema_path, role.as_str(), txn.txid);
                            let old_path = self.storage.swap_path(path);
                            let result = exec::create_view(
                                &mut self.storage,
                                &mut self.wal,
                                txn,
                                exec::CreateViewCommand {
                                    name,
                                    columns,
                                    or_replace: *or_replace,
                                    security: *security,
                                    security_barrier: *security_barrier,
                                    check_option: *check_option,
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
                                exec::PlpgsqlTransactionContext::Atomic,
                                responder,
                            )
                        };
                        let result = result?;
                        if let Err(e) = result {
                            return Ok(Err(e));
                        }
                    }
                    Ok(Ok(()))
                })();
                if resolved.authorization.is_some() {
                    guc.set_role(prior_role.as_str(), true);
                    eval::funcs::system::set_current_user(prior_user.as_str());
                }
                elements_result
            }
            Stmt::DropSchema {
                names,
                if_exists,
                cascade,
            } => exec::drop_schema(
                &mut self.storage,
                &mut self.wal,
                txn,
                &mut self.dml_scratch,
                names,
                *if_exists,
                *cascade,
                arena,
                guc.seq_session(),
                responder,
            ),
            Stmt::AlterSchema { name, action } => exec::alter_schema(
                &mut self.storage,
                &mut self.wal,
                txn,
                name,
                *action,
                arena,
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
            Stmt::AlterLargeObjectOwner { oid, role } => {
                exec::alter_large_object_owner(&mut self.storage, txn, *oid, role, responder)
            }
            Stmt::CreateRole {
                name,
                options,
                memberships,
            } => exec::create_role(
                &mut self.storage,
                &mut self.wal,
                txn,
                exec::CreateRoleRequest {
                    name,
                    options,
                    memberships,
                },
                guc,
                responder,
            ),
            Stmt::AlterRole { role, options } => exec::alter_role(
                &mut self.storage,
                &mut self.wal,
                txn,
                *role,
                options,
                guc,
                responder,
            ),
            Stmt::AlterRoleRename { role, new_name } => exec::rename_role(
                &mut self.storage,
                &mut self.wal,
                txn,
                *role,
                new_name,
                responder,
            ),
            Stmt::AlterRoleSetting {
                role,
                database,
                action,
            } => exec::alter_role_setting(
                &mut self.storage,
                &mut self.wal,
                txn,
                *role,
                *database,
                *action,
                guc,
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
                grantor,
            } => exec::grant_role(
                &mut self.storage,
                &mut self.wal,
                txn,
                exec::GrantRoleRequest {
                    roles,
                    members,
                    options: *options,
                    grantor: *grantor,
                },
                responder,
            ),
            Stmt::RevokeRole {
                roles,
                members,
                option,
                grantor,
                cascade,
            } => exec::revoke_role(
                &mut self.storage,
                &mut self.wal,
                txn,
                exec::RevokeRoleRequest {
                    roles,
                    members,
                    option: *option,
                    grantor: *grantor,
                    cascade: *cascade,
                },
                responder,
            ),
            Stmt::GrantPrivileges {
                privileges,
                target,
                grantees,
                grant_option,
                grantor,
            } => exec::grant_privileges(
                &mut self.storage,
                txn,
                arena,
                privileges,
                *target,
                grantees,
                *grant_option,
                *grantor,
                responder,
            ),
            Stmt::RevokePrivileges {
                grant_option_only,
                privileges,
                target,
                grantees,
                grantor,
                cascade,
            } => exec::revoke_privileges(
                &mut self.storage,
                txn,
                arena,
                *grant_option_only,
                privileges,
                *target,
                grantees,
                *grantor,
                *cascade,
                responder,
            ),
            Stmt::GrantParameterPrivileges {
                privileges,
                names,
                grantees,
                grant_option,
                grantor,
            } => exec::grant_parameter_privileges(
                &mut self.storage,
                txn,
                exec::ParameterGrantCommand {
                    target: exec::ParameterPrivilegeTarget {
                        privileges: *privileges,
                        names,
                        grantees,
                        grantor: *grantor,
                    },
                    grant_option: *grant_option,
                },
                responder,
            ),
            Stmt::RevokeParameterPrivileges {
                grant_option_only,
                privileges,
                names,
                grantees,
                grantor,
                cascade,
            } => exec::revoke_parameter_privileges(
                &mut self.storage,
                txn,
                exec::ParameterRevokeCommand {
                    target: exec::ParameterPrivilegeTarget {
                        privileges: *privileges,
                        names,
                        grantees,
                        grantor: *grantor,
                    },
                    grant_option_only: *grant_option_only,
                    cascade: *cascade,
                },
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
            Stmt::ReassignOwned { roles, new_owner } => exec::reassign_owned(
                &mut self.storage,
                &mut self.wal,
                txn,
                roles,
                new_owner,
                responder,
            ),
            Stmt::DropOwned { roles, cascade } => exec::drop_owned(
                &mut self.storage,
                &mut self.wal,
                txn,
                &mut self.dml_scratch,
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
                let at = match cursors.open(name, *scroll, *hold, *binary) {
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
                                sqlstate: SqlState::known(e.sqlstate),
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
                    let (text, binary) = cursors.result_buffers(at);
                    let mut capture = Responder::for_cursor(text, binary);
                    capture.set_render(guc.render());
                    let sequence_state = sequence::SequenceReplayState::new();
                    let sequence = sequence::ReplaySeqEval::new(
                        sequence::SeqEval::new(&self.storage, guc.seq_session(), txn.txid),
                        &sequence_state,
                    );
                    match &parsed {
                        Stmt::Select(sel) => {
                            let sel = match query::expand_ctes_exec(
                                sel,
                                &self.storage,
                                txn.txid,
                                &self.work,
                                params,
                                &[],
                                Some(&sequence),
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
                    let requested = responder.result_formats();
                    let wire = cursors.wire_parts(name).expect("fetch found it");
                    let formats = if requested.count() == 0 && wire.declared_binary {
                        crate::pg::respond::ResultFmt::ALL_BINARY
                    } else {
                        requested
                    };
                    responder.cursor_row_description(wire.description, formats)?;
                    for &row in cursors.emitted() {
                        let (text_offset, text_len) = wire.text_spans[row as usize];
                        let (binary_offset, binary_len) = wire.binary_spans[row as usize];
                        responder.cursor_data_row(
                            &wire.text[text_offset as usize..(text_offset + text_len) as usize],
                            &wire.binary
                                [binary_offset as usize..(binary_offset + binary_len) as usize],
                            formats,
                        )?;
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
                if txn.is_explicit() {
                    // PostgreSQL warns and continues.
                    responder.warning(
                        crate::sql::eval::sqlstate::ACTIVE_SQL_TRANSACTION,
                        "there is already a transaction in progress",
                    )?;
                    responder.command_complete("BEGIN")?;
                    return Ok(Ok(()));
                }
                self.ensure_txn(txn, TxnMode::Explicit, guc);
                txn.apply_begin_characteristics(*characteristics);
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
                    if let Err(e) = self.commit_txn_with_triggers(txn, guc, arena, responder) {
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
            Stmt::PrepareTransaction(gid) => {
                if let Err(error) =
                    self.prepare_transaction(*gid, txn, guc, cursors, arena, responder)
                {
                    return Ok(Err(error));
                }
                responder.command_complete("PREPARE TRANSACTION")?;
                datetime::begin_statement();
                self.ensure_txn(txn, TxnMode::Implicit, guc);
                Ok(Ok(()))
            }
            Stmt::CommitPrepared(gid) => {
                if let Err(error) = self.resolve_prepared_transaction(*gid, true, txn, guc) {
                    return Ok(Err(error));
                }
                responder.command_complete("COMMIT PREPARED")?;
                datetime::begin_statement();
                self.ensure_txn(txn, TxnMode::Implicit, guc);
                Ok(Ok(()))
            }
            Stmt::RollbackPrepared(gid) => {
                if let Err(error) = self.resolve_prepared_transaction(*gid, false, txn, guc) {
                    return Ok(Err(error));
                }
                responder.command_complete("ROLLBACK PREPARED")?;
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
                let mut slots = [usize::MAX; parser::MAX_LOCK_TABLES];
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
            Stmt::SetConstraints { targets, mode } => {
                if !txn.is_explicit() {
                    responder.warning(
                        sqlstate::NO_ACTIVE_SQL_TRANSACTION,
                        "SET CONSTRAINTS can only be used in transaction blocks",
                    )?;
                    responder.command_complete("SET CONSTRAINTS")?;
                    return Ok(Ok(()));
                }
                let empty = txn::ConstraintIdentity::Table {
                    table: 0,
                    name: crate::storage::SqlName::EMPTY,
                    generation: 0,
                };
                let mut identities = [empty; txn::MAX_DEFERRED_CONSTRAINTS];
                let count = match targets {
                    crate::sql::ast::ConstraintTargets::All => 0,
                    crate::sql::ast::ConstraintTargets::Named(names) => {
                        let mut count = 0;
                        for name in *names {
                            let mut matches = [empty; txn::MAX_DEFERRED_CONSTRAINTS];
                            let matched = match exec::constraints::resolve_constraint_name(
                                &self.storage,
                                name,
                                *mode,
                                txn.txid,
                                &mut matches,
                            ) {
                                Ok(matched) => matched,
                                Err(error) => return Ok(Err(error)),
                            };
                            for identity in matches[..matched].iter().copied() {
                                let identity = txn.catalog_constraint_identity(identity);
                                if identities[..count].contains(&identity) {
                                    continue;
                                }
                                if count == identities.len() {
                                    return Ok(Err(sql_err!(
                                        sqlstate::PROGRAM_LIMIT_EXCEEDED,
                                        "SET CONSTRAINTS matches more than {} constraints",
                                        identities.len()
                                    )));
                                }
                                identities[count] = identity;
                                count += 1;
                            }
                        }
                        count
                    }
                };
                let additional = if matches!(targets, crate::sql::ast::ConstraintTargets::All) {
                    1
                } else {
                    count
                };
                if !txn.can_record_constraint_modes(additional) {
                    return Ok(Err(sql_err!(
                        sqlstate::PROGRAM_LIMIT_EXCEEDED,
                        "transaction changes constraint modes more than {} times",
                        txn::MAX_DEFERRED_CONSTRAINTS
                    )));
                }
                if *mode == crate::sql::ast::ConstraintMode::Immediate {
                    if matches!(targets, crate::sql::ast::ConstraintTargets::All) {
                        if let Err(error) = exec::constraints::validate_deferred_constraints(
                            &self.storage,
                            txn,
                            false,
                            arena,
                        ) {
                            return Ok(Err(error));
                        }
                    } else {
                        for identity in identities[..count].iter().copied() {
                            while let Some((index, obligation)) =
                                txn.deferred_constraint_for(identity)
                            {
                                if let Err(error) =
                                    exec::constraints::validate_constraint_obligation(
                                        &self.storage,
                                        identity,
                                        obligation.rowid,
                                        txn.txid,
                                        arena,
                                    )
                                {
                                    return Ok(Err(error));
                                }
                                if let Err(error) = txn.complete_deferred_constraint(index) {
                                    return Ok(Err(error));
                                }
                            }
                        }
                    }
                    if matches!(targets, crate::sql::ast::ConstraintTargets::All) {
                        if let Err(error) = self.fire_constraint_trigger_boundary(
                            txn,
                            guc,
                            arena,
                            responder,
                            exec::TriggerQueueBoundary::Constraints(None),
                        ) {
                            return Ok(Err(error));
                        }
                    } else {
                        for identity in identities[..count].iter().copied() {
                            if let Err(error) = self.fire_constraint_trigger_boundary(
                                txn,
                                guc,
                                arena,
                                responder,
                                exec::TriggerQueueBoundary::Constraints(Some(identity)),
                            ) {
                                return Ok(Err(error));
                            }
                        }
                    }
                }
                let result = if matches!(targets, crate::sql::ast::ConstraintTargets::All) {
                    txn.record_constraint_mode(None, *mode)
                } else {
                    let mut result = Ok(());
                    for identity in identities[..count].iter().copied() {
                        if let Err(error) = txn.record_constraint_mode(Some(identity), *mode) {
                            result = Err(error);
                            break;
                        }
                    }
                    result
                };
                if let Err(error) = result {
                    return Ok(Err(error));
                }
                responder.command_complete("SET CONSTRAINTS")?;
                Ok(Ok(()))
            }
            Stmt::Set {
                name,
                value,
                local,
                syntax,
            } => {
                if crate::sql::guc::requires_set_privilege(name)
                    && self
                        .storage
                        .current_role_slot(txn.txid)
                        .is_some_and(|role| {
                            !self.storage.role(role).attributes_to(txn.txid).superuser
                                && crate::sql::ast::ParameterName::parse(name).is_none_or(
                                    |parameter| {
                                        !self.storage.has_parameter_privilege(
                                            parameter,
                                            role,
                                            crate::sql::ast::ParameterPrivileges::SET,
                                            txn.txid,
                                        )
                                    },
                                )
                        })
                {
                    return Ok(Err(sql_err!(
                        sqlstate::INSUFFICIENT_PRIVILEGE,
                        "permission denied to set parameter \"{}\"",
                        name
                    )));
                }
                if *local && !txn.is_explicit() {
                    responder.warning(
                        sqlstate::NO_ACTIVE_SQL_TRANSACTION,
                        "SET LOCAL can only be used in transaction blocks",
                    )?;
                }
                if *syntax == ast::SettingSyntax::FromCurrent {
                    if let Some(characteristics) = guc.current_transaction_setting_from_current(
                        name,
                        txn.isolation,
                        txn.read_only,
                        txn.deferrable,
                    ) {
                        let characteristics = match characteristics {
                            Ok(characteristics) => characteristics,
                            Err(error) => return Ok(Err(error)),
                        };
                        if let Err(error) = apply_current_transaction_setting(txn, characteristics)
                        {
                            return Ok(Err(error));
                        }
                    } else if let Err(error) = guc.set_from_current(name, *local) {
                        return Ok(Err(error));
                    }
                    guc::publish_active_setting(guc, name);
                    responder.command_complete("SET")?;
                    return Ok(Ok(()));
                }
                if let Some(characteristics) = guc.current_transaction_setting(name, value) {
                    let characteristics = match characteristics {
                        Ok(characteristics) => characteristics,
                        Err(error) => return Ok(Err(error)),
                    };
                    if let Err(error) = apply_current_transaction_setting(txn, characteristics) {
                        return Ok(Err(error));
                    }
                    responder.command_complete("SET")?;
                    return Ok(Ok(()));
                }
                let changed = match syntax {
                    ast::SettingSyntax::Generic => guc.set(name, value, *local),
                    ast::SettingSyntax::FromCurrent => {
                        unreachable!("FROM CURRENT is handled before value application")
                    }
                    ast::SettingSyntax::TimeZone => guc.set_time_zone_sql(value, *local),
                    ast::SettingSyntax::TimeZoneInterval(type_mod) => {
                        let interval = match datetime::parse_interval(value) {
                            Ok(interval) => interval,
                            Err(error) => return Ok(Err(error)),
                        };
                        let interval = match exec::apply_typmod(
                            Datum::Interval(interval),
                            types::ColType::Interval,
                            *type_mod,
                            arena,
                        ) {
                            Ok(Datum::Interval(interval)) => interval,
                            Ok(_) => unreachable!("interval typmod preserves its type"),
                            Err(error) => return Ok(Err(error)),
                        };
                        guc.set_time_zone_interval(interval, *local)
                    }
                };
                match changed {
                    Ok(()) => {
                        if name.eq_ignore_ascii_case("default_tablespace") {
                            let tablespace = guc.default_tablespace();
                            let name = tablespace.as_str();
                            if !name.is_empty()
                                && !name.eq_ignore_ascii_case("pg_default")
                                && !name.eq_ignore_ascii_case("pg_global")
                                && self.storage.tablespace_slot(name, txn.txid).is_none()
                            {
                                return Ok(Err(sql_err!(
                                    sqlstate::INVALID_PARAMETER_VALUE,
                                    "invalid value for parameter \"default_tablespace\": \"{}\": tablespace does not exist",
                                    name
                                )));
                            }
                        }
                        guc::publish_active_setting(guc, name);
                        responder.set_render(guc.render());
                        responder.command_complete("SET")?;
                        Ok(Ok(()))
                    }
                    Err(e) => Ok(Err(e)),
                }
            }
            Stmt::SetCatalog(_) => Ok(Err(sql_err!(
                sqlstate::FEATURE_NOT_SUPPORTED,
                "current database cannot be changed"
            ))),
            Stmt::AlterSystem { name, value } => exec::alter_system(
                &mut self.storage,
                &mut self.wal,
                txn,
                *name,
                *value,
                guc,
                responder,
            ),
            Stmt::Reset(name) => {
                if name.is_some_and(crate::sql::guc::requires_set_privilege)
                    && self
                        .storage
                        .current_role_slot(txn.txid)
                        .is_some_and(|role| {
                            !self.storage.role(role).attributes_to(txn.txid).superuser
                                && name
                                    .and_then(crate::sql::ast::ParameterName::parse)
                                    .is_none_or(|parameter| {
                                        !self.storage.has_parameter_privilege(
                                            parameter,
                                            role,
                                            crate::sql::ast::ParameterPrivileges::SET,
                                            txn.txid,
                                        )
                                    })
                        })
                {
                    return Ok(Err(sql_err!(
                        sqlstate::INSUFFICIENT_PRIVILEGE,
                        "permission denied to set parameter \"{}\"",
                        name.unwrap_or("")
                    )));
                }
                if let Some(name) = name
                    && guc.transaction_reset_owned(name).is_some()
                {
                    return Ok(Err(sql_err!(
                        sqlstate::FEATURE_NOT_SUPPORTED,
                        "parameter \"{}\" cannot be reset",
                        name
                    )));
                }
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
            Stmt::SetTransaction {
                target,
                characteristics,
            } => {
                if *target == TransactionTarget::SessionDefaults {
                    if let Err(error) = guc.set_transaction_defaults(*characteristics) {
                        return Ok(Err(error));
                    }
                    responder.command_complete("SET")?;
                    return Ok(Ok(()));
                }
                if !txn.is_explicit() {
                    responder.warning(
                        sqlstate::NO_ACTIVE_SQL_TRANSACTION,
                        "SET TRANSACTION can only be used in transaction blocks",
                    )?;
                    responder.command_complete("SET")?;
                    return Ok(Ok(()));
                }
                if let Err(error) = apply_current_transaction_setting(txn, *characteristics) {
                    return Ok(Err(error));
                }
                responder.command_complete("SET")?;
                Ok(Ok(()))
            }
            Stmt::SetTransactionSnapshot(snapshot) => {
                if let Err(error) = self.import_replication_snapshot(txn, snapshot) {
                    return Ok(Err(error));
                }
                responder.command_complete("SET")?;
                Ok(Ok(()))
            }
            Stmt::Show(name) => self.show(name, guc, txn, responder),
            Stmt::ShowAll => self.show_all(guc, txn, responder),
            Stmt::Discard(target) => {
                match target {
                    ast::DiscardTarget::All => {
                        sqlprep.clear();
                        cursors.close_all();
                        guc.discard_all();
                        if let Err(error) = self.apply_system_settings(guc) {
                            return Ok(Err(error));
                        }
                        let role = self
                            .storage
                            .find_role(guc.authenticated_user())
                            .expect("authenticated role remains present");
                        if let Err(error) = self.apply_role_settings(role as u16, guc) {
                            return Ok(Err(error));
                        }
                        self.notify.drop_conn(self.current_conn_id);
                        self.discard_protocol_state = true;
                    }
                    ast::DiscardTarget::Sequences => guc.seq_session().discard(),
                    ast::DiscardTarget::Plans | ast::DiscardTarget::Temporary => {}
                }
                responder.command_complete("DISCARD")?;
                Ok(Ok(()))
            }
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
                    if let Err(error) =
                        self.copy_start(&setup, txn, guc.seq_session(), arena, responder)
                    {
                        return Ok(Err(error));
                    }
                    responder.copy_in_response(setup.n_targets, setup.fmt.binary)?;
                    self.pending_copy = Some(setup);
                    Ok(Ok(()))
                }
            }
            Stmt::Checkpoint => self.execute_checkpoint_statement(responder),
            // VACUUM reclaims space; in this LSM that is a checkpoint (flush +
            // compaction, pruning superseded versions and tombstones). The
            // options and per-table targets are parsed; a checkpoint compacts
            // the whole store, which subsumes any named table. Without object
            // storage there is nothing to compact to, and — as VACUUM on a
            // table with nothing to reclaim does in PostgreSQL — it succeeds.
            Stmt::Vacuum { targets, options } => {
                self.execute_vacuum_statement(targets, *options, txn, responder)
            }
            // ANALYZE resolves every requested relation/column and walks its
            // MVCC-visible row state. Cardinality and widths are exact for that
            // snapshot; distinct counts use the fixed-size estimator.
            Stmt::Analyze(targets) => self.execute_analyze_statement(targets, txn, responder),
            Stmt::Listen(channel) => self.execute_listen_statement(channel, txn, responder),
            Stmt::Unlisten(channel) => self.execute_unlisten_statement(*channel, txn, responder),
            Stmt::Notify { channel, payload } => {
                self.execute_notify_statement(channel, *payload, txn, responder)
            }
            Stmt::AlterTable(a) => exec::alter_table(
                &mut self.storage,
                &mut self.wal,
                txn,
                &mut self.dml_scratch,
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
                                sqlstate: SqlState::known(sqlstate::UNDEFINED_OBJECT),
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
                        sqlstate: SqlState::known(sqlstate::INVALID_SQL_STATEMENT_NAME),
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
                            sqlstate: SqlState::known(sqlstate::PROGRAM_LIMIT_EXCEEDED),
                            message: stack_format!(192, "statement too large for SQL arena"),
                        }));
                    }
                };
                // If the statement declared parameter types, the argument count
                // must match and each argument is coerced to its declared type.
                if n_decl > 0 && args.len() != n_decl {
                    return Ok(Err(SqlError {
                        sqlstate: SqlState::known(sqlstate::PROTOCOL_VIOLATION),
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
                            sqlstate: SqlState::known(sqlstate::SYNTAX_ERROR),
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
                        exec::PlpgsqlTransactionContext::Atomic,
                        responder,
                    ),
                    Ok(None) => Ok(Ok(())),
                    Err(e) => Ok(Err(SqlError {
                        sqlstate: SqlState::known(sqlstate::SYNTAX_ERROR),
                        message: stack_format!(192, "{}", e.message.as_str()),
                    })),
                }
            }
            Stmt::Deallocate(name) => {
                match name {
                    Some(n) => {
                        if !sqlprep.remove(n) {
                            return Ok(Err(SqlError {
                                sqlstate: SqlState::known(sqlstate::INVALID_SQL_STATEMENT_NAME),
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
        };
        if matches!(outcome, Ok(Ok(())))
            && (event_drop || event_end)
            && let Some(tag) = event_tag
        {
            let mut commands = [event_trigger::DdlCommand::EMPTY; event_trigger::MAX_EVENT_OBJECTS];
            let mut drops = [event_trigger::DroppedObject::EMPTY; event_trigger::MAX_EVENT_OBJECTS];
            let (command_count, drop_count) = match event_trigger::collect(
                &self.storage,
                txn.txid,
                statement,
                tag,
                event_trigger::CollectChanges {
                    before: event_before,
                    undo: &txn.ddl()[event_ddl_mark..],
                    undo_origins: &txn.ddl_origins()[event_ddl_mark..],
                    origin: event_ddl_origin,
                    in_extension: txn.in_extension_script(),
                },
                event_trigger::EventGraphs {
                    commands: &mut commands,
                    drops: &mut drops,
                },
            ) {
                Ok(counts) => counts,
                Err(error) => return Ok(Err(error)),
            };
            if event_drop {
                let _scope = event_trigger::enter_dropped_objects(&drops[..drop_count]);
                if let Err(error) = self.fire_event_triggers(
                    EventTriggerInvocation::SqlDrop { tag },
                    EventTriggerExecution {
                        txn,
                        cursors,
                        guc,
                        arena,
                        responder,
                    },
                ) {
                    return Ok(Err(error));
                }
            }
            if event_end {
                let _scope = event_trigger::enter_ddl_commands(&commands[..command_count]);
                if let Err(error) = self.fire_event_triggers(
                    EventTriggerInvocation::DdlCommandEnd { tag },
                    EventTriggerExecution {
                        txn,
                        cursors,
                        guc,
                        arena,
                        responder,
                    },
                ) {
                    return Ok(Err(error));
                }
            }
        }
        outcome
    }

    #[allow(clippy::too_many_arguments)]
    fn execute_extension_plan(
        &mut self,
        plan: exec::ExtensionExecutionPlan,
        owner: &str,
        arena: &Arena,
        params: &[Datum],
        txn: &mut TxnState,
        sqlprep: &mut SqlPreparedPool,
        cursors: &mut cursor::CursorPool,
        guc: &mut GucState,
        responder: &mut Responder,
    ) -> Result<Result<crate::storage::ExtensionPackage, SqlError>, WireFull> {
        if plan.run_as_bootstrap {
            guc.set_role("postgres", true);
        }
        for script in &plan.scripts[..plan.script_count] {
            let effective = self.storage.extension_script(*script as usize).effective;
            if let Err(error) =
                self.reconcile_extension_requirements(plan.extension, effective, txn)
            {
                if plan.run_as_bootstrap {
                    guc.set_role(owner, true);
                }
                return Ok(Err(error));
            }
            txn.enter_extension_script();
            let script_result = self.execute_extension_script(
                plan.extension,
                plan.package,
                *script as usize,
                owner,
                arena,
                params,
                txn,
                sqlprep,
                cursors,
                guc,
                responder,
            );
            txn.leave_extension_script();
            match script_result {
                Ok(Ok(())) => {}
                Ok(Err(error)) => {
                    if plan.run_as_bootstrap {
                        guc.set_role(owner, true);
                    }
                    return Ok(Err(error));
                }
                Err(full) => {
                    if plan.run_as_bootstrap {
                        guc.set_role(owner, true);
                    }
                    return Err(full);
                }
            }
        }
        if plan.run_as_bootstrap {
            guc.set_role(owner, true);
        }
        Ok(Ok(self
            .storage
            .extension_script(plan.scripts[plan.script_count - 1] as usize)
            .effective))
    }

    fn execute_extension_config_dump(
        &mut self,
        extension: usize,
        arguments: [&Expr<'_>; 2],
        arena: &Arena,
        params: &[Datum],
        txn: &mut TxnState,
    ) -> Result<(), SqlError> {
        let catalog = query::storage_catalog(&self.storage, &self.work, txn.txid);
        let hooks = EvalHooks {
            catalog: Some(&catalog),
            ..NO_HOOKS
        };
        let relation = eval::eval_full(arguments[0], arena, params, &NoColumns, &hooks)?;
        let condition = eval::eval_full(arguments[1], arena, params, &NoColumns, &hooks)?;
        if relation == Datum::Null || condition == Datum::Null {
            return Ok(());
        }
        let relation_oid = match relation {
            Datum::RegObject {
                type_oid: types::oid::REGCLASS,
                referenced_oid,
                ..
            } => referenced_oid,
            Datum::Text(name) | Datum::Bpchar(name) => {
                catalog::reloid_of_name(&self.storage, txn.txid, name).ok_or_else(|| {
                    sql_err!(
                        sqlstate::UNDEFINED_TABLE,
                        "relation \"{}\" does not exist",
                        name
                    )
                })?
            }
            Datum::Oid(oid) => i32::try_from(oid).map_err(|_| {
                sql_err!(
                    sqlstate::UNDEFINED_TABLE,
                    "relation with OID {} does not exist",
                    oid
                )
            })?,
            Datum::Int4(oid) if oid >= 0 => oid,
            _ => {
                return Err(sql_err!(
                    sqlstate::DATATYPE_MISMATCH,
                    "pg_extension_config_dump first argument must be regclass"
                ));
            }
        };
        let relation =
            catalog::extension_config_relation_by_oid(&self.storage, txn.txid, relation_oid)
                .ok_or_else(|| {
                    sql_err!(
                        sqlstate::WRONG_OBJECT_TYPE,
                        "relation with OID {} is not a table or sequence",
                        relation_oid
                    )
                })?;
        let condition = match condition {
            Datum::Text(condition) | Datum::Bpchar(condition) => condition,
            _ => {
                return Err(sql_err!(
                    sqlstate::DATATYPE_MISMATCH,
                    "pg_extension_config_dump second argument must be text"
                ));
            }
        };
        exec::set_extension_config(&mut self.storage, txn, extension, relation, condition)
    }

    #[allow(clippy::too_many_arguments)]
    fn execute_extension_script(
        &mut self,
        extension: usize,
        package: usize,
        script_slot: usize,
        owner: &str,
        arena: &Arena,
        params: &[Datum],
        txn: &mut TxnState,
        sqlprep: &mut SqlPreparedPool,
        cursors: &mut cursor::CursorPool,
        guc: &mut GucState,
        responder: &mut Responder,
    ) -> Result<Result<(), SqlError>, WireFull> {
        use core::fmt::Write as _;
        let script = self.storage.extension_script(script_slot);
        let package_definition = script.effective;
        let mut required_names =
            [crate::storage::SqlName::EMPTY; crate::storage::MAX_EXTENSION_REQUIRES];
        let mut required_schemas = [""; crate::storage::MAX_EXTENSION_REQUIRES];
        for (index, required) in package_definition.requires().iter().enumerate() {
            let Some(required_slot) = self.storage.extension_slot(required.as_str(), txn.txid)
            else {
                return Ok(Err(sql_err!(
                    sqlstate::OBJECT_NOT_IN_PREREQUISITE_STATE,
                    "required extension \"{}\" is not installed",
                    required.as_str()
                )));
            };
            let namespace = self
                .storage
                .extension(required_slot)
                .definition_to(txn.txid)
                .0 as usize;
            required_names[index] = *required;
            required_schemas[index] = self.storage.schema_def(namespace).name.as_str();
        }
        let required_count = package_definition.requires().len();
        let namespace = self.storage.extension(extension).definition_to(txn.txid).0 as usize;
        let schema = self.storage.schema_def(namespace).name.as_str();
        debug_assert_eq!(script.package as usize, package);
        let source = self.storage.extension_script_source(script);
        if let Err(error) =
            validate_extension_script_substitutions(source, &required_names[..required_count])
        {
            return Ok(Err(error));
        }
        let rendered = match arena.alloc_str_display(ExtensionScriptText {
            source,
            schema,
            substitute_schema: !package_definition.relocatable,
            owner,
            required_names: &required_names[..required_count],
            required_schemas: &required_schemas[..required_count],
        }) {
            Ok(rendered) => rendered,
            Err(_) => return Ok(Err(query::arena_full_pub())),
        };

        let mut path = crate::util::StackStr::<128>::new();
        let mut append_schema = |name: &str| -> Result<(), SqlError> {
            if !path.as_str().is_empty() && write!(path, ", ").is_err() {
                return Err(sql_err!(
                    sqlstate::PROGRAM_LIMIT_EXCEEDED,
                    "extension search path is too long"
                ));
            }
            if write!(path, "\"").is_err() {
                return Err(sql_err!(
                    sqlstate::PROGRAM_LIMIT_EXCEEDED,
                    "extension search path is too long"
                ));
            }
            for character in name.chars() {
                if character == '"' && write!(path, "\"").is_err() {
                    return Err(sql_err!(
                        sqlstate::PROGRAM_LIMIT_EXCEEDED,
                        "extension search path is too long"
                    ));
                }
                if write!(path, "{}", character).is_err() {
                    return Err(sql_err!(
                        sqlstate::PROGRAM_LIMIT_EXCEEDED,
                        "extension search path is too long"
                    ));
                }
            }
            if write!(path, "\"").is_err() {
                return Err(sql_err!(
                    sqlstate::PROGRAM_LIMIT_EXCEEDED,
                    "extension search path is too long"
                ));
            }
            Ok(())
        };
        if let Err(error) = append_schema(schema) {
            return Ok(Err(error));
        }
        for required_schema in &required_schemas[..required_count] {
            if let Err(error) = append_schema(required_schema) {
                return Ok(Err(error));
            }
        }
        if write!(path, ", pg_temp").is_err() {
            return Ok(Err(sql_err!(
                sqlstate::PROGRAM_LIMIT_EXCEEDED,
                "extension search path is too long"
            )));
        }
        let old_path = guc.search_path();
        if let Err(error) = guc.set("search_path", path.as_str(), true) {
            return Ok(Err(error));
        }
        let mut parser = match Parser::new(rendered, arena) {
            Ok(parser) => parser,
            Err(error) => {
                let _ = guc.set("search_path", old_path.as_str(), true);
                return Ok(Err(parse_error_to_sql(&error)));
            }
        };
        loop {
            let statement_mark = arena.mark();
            let statement = match parser.next_stmt() {
                Ok(Some(statement)) => statement,
                Ok(None) => break,
                Err(error) => {
                    let _ = guc.set("search_path", old_path.as_str(), true);
                    return Ok(Err(parse_error_to_sql(&error)));
                }
            };
            if matches!(
                statement,
                Stmt::Begin(_)
                    | Stmt::Commit
                    | Stmt::Rollback
                    | Stmt::Savepoint(_)
                    | Stmt::ReleaseSavepoint(_)
                    | Stmt::RollbackToSavepoint(_)
                    | Stmt::CreateExtension { .. }
                    | Stmt::AlterExtension { .. }
                    | Stmt::DropExtension { .. }
            ) {
                let _ = guc.set("search_path", old_path.as_str(), true);
                return Ok(Err(sql_err!(
                    sqlstate::FEATURE_NOT_SUPPORTED,
                    "extension scripts cannot control transactions or extension lifecycle"
                )));
            }
            if let Some(arguments) = extension_config_dump_arguments(&statement) {
                if let Err(error) =
                    self.execute_extension_config_dump(extension, arguments, arena, params, txn)
                {
                    let _ = guc.set("search_path", old_path.as_str(), true);
                    return Ok(Err(error));
                }
                continue;
            }
            let ddl_start = txn.ddl().len();
            let result = responder.without_query_output(|responder| {
                self.execute_stmt(
                    &statement,
                    arena,
                    params,
                    txn,
                    sqlprep,
                    cursors,
                    guc,
                    exec::PlpgsqlTransactionContext::Atomic,
                    responder,
                )
            });
            let result = match result {
                Ok(result) => result,
                Err(full) => {
                    let _ = guc.set("search_path", old_path.as_str(), true);
                    return Err(full);
                }
            };
            if let Err(error) = result {
                let _ = guc.set("search_path", old_path.as_str(), true);
                return Ok(Err(error));
            }
            let ddl_end = txn.ddl().len();
            for index in ddl_start..ddl_end {
                if let Some(object) = created_access_object(txn.ddl()[index])
                    && let Err(error) =
                        exec::record_extension_member(&mut self.storage, txn, extension, object)
                {
                    let _ = guc.set("search_path", old_path.as_str(), true);
                    return Ok(Err(error));
                }
            }
            // The parser cursor and rendered script live below this mark; the
            // completed statement has published every durable effect.
            unsafe { arena.rewind_to(statement_mark) };
        }
        let _ = guc.set("search_path", old_path.as_str(), true);
        Ok(Ok(()))
    }

    fn reconcile_extension_requirements(
        &mut self,
        extension: usize,
        package: crate::storage::ExtensionPackage,
        txn: &mut TxnState,
    ) -> Result<(), SqlError> {
        let object = crate::storage::AccessObject {
            class: crate::storage::AccessClass::Extension,
            slot: extension as u16,
        };
        let mut existing = [usize::MAX; crate::storage::MAX_EXTENSION_REQUIRES];
        let mut existing_count = 0usize;
        for (slot, dependency) in self.storage.extension_dependencies_visible_to(txn.txid) {
            if dependency.object == object
                && dependency.kind == crate::storage::ExtensionDependencyKind::Required
            {
                existing[existing_count] = slot;
                existing_count += 1;
            }
        }
        for slot in &existing[..existing_count] {
            let dependency = *self.storage.extension_dependency(*slot);
            let required = self.storage.extension(dependency.extension as usize).name;
            if package.requires().contains(&required) {
                continue;
            }
            let (changed, prior) = self.storage.change_extension_dependency(
                dependency.extension as usize,
                object,
                crate::storage::ExtensionDependencyKind::Required,
                false,
                txn.txid,
            )?;
            if let Err(error) = txn.record_ddl(DdlUndo::ExtensionDependencyChanged {
                slot: changed as u32,
                prior,
            }) {
                self.storage.rollback_extension_dependency(changed, prior);
                return Err(error);
            }
        }
        for required in package.requires() {
            let required_extension = self
                .storage
                .extension_slot(required.as_str(), txn.txid)
                .ok_or_else(|| {
                    sql_err!(
                        sqlstate::OBJECT_NOT_IN_PREREQUISITE_STATE,
                        "required extension \"{}\" is not installed",
                        required.as_str()
                    )
                })?;
            if self
                .storage
                .extension_dependencies_visible_to(txn.txid)
                .any(|(_, dependency)| {
                    dependency.extension as usize == required_extension
                        && dependency.object == object
                        && dependency.kind == crate::storage::ExtensionDependencyKind::Required
                })
            {
                continue;
            }
            exec::set_extension_dependency(
                &mut self.storage,
                txn,
                required.as_str(),
                object,
                crate::storage::ExtensionDependencyKind::Required,
                true,
            )?;
        }
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    fn alter_extension_schema(
        &mut self,
        name: &str,
        new_schema: &str,
        arena: &Arena,
        params: &[Datum],
        txn: &mut TxnState,
        sqlprep: &mut SqlPreparedPool,
        cursors: &mut cursor::CursorPool,
        guc: &mut GucState,
        responder: &mut Responder,
    ) -> Result<Result<(), SqlError>, WireFull> {
        use core::fmt::Write as _;
        let Some(extension) = self.storage.extension_slot(name, txn.txid) else {
            return Ok(Err(sql_err!(
                sqlstate::UNDEFINED_OBJECT,
                "extension \"{}\" does not exist",
                name
            )));
        };
        let old_namespace = self.storage.extension(extension).definition_to(txn.txid).0 as usize;
        let old_schema = self.storage.schema_def(old_namespace).name;
        let mut objects = [crate::storage::AccessObject {
            class: crate::storage::AccessClass::Table,
            slot: 0,
        }; crate::storage::MAX_EXTENSION_DEPENDENCIES];
        let mut count = 0usize;
        let mut covered_by_relation_move = [false; crate::storage::MAX_EXTENSION_DEPENDENCIES];
        for (_, dependency) in self.storage.extension_dependencies_visible_to(txn.txid) {
            if dependency.extension as usize == extension
                && dependency.kind == crate::storage::ExtensionDependencyKind::Member
            {
                objects[count] = dependency.object;
                count += 1;
            }
        }
        for (index, object) in objects[..count].iter().enumerate() {
            let (schema, object_name) = self.storage.access_object_name_to(*object, txn.txid);
            if object.class != crate::storage::AccessClass::Schema
                && !schema.as_str().is_empty()
                && schema != old_schema
            {
                return Ok(Err(sql_err!(
                    sqlstate::FEATURE_NOT_SUPPORTED,
                    "extension \"{}\" does not occupy a single schema",
                    name
                )));
            }
            if matches!(
                object.class,
                crate::storage::AccessClass::Schema
                    | crate::storage::AccessClass::Tablespace
                    | crate::storage::AccessClass::Extension
            ) {
                return Ok(Err(sql_err!(
                    sqlstate::FEATURE_NOT_SUPPORTED,
                    "extension member \"{}\" cannot be moved to another schema",
                    object_name.as_str()
                )));
            }
            if object.class == crate::storage::AccessClass::MaterializedView {
                covered_by_relation_move[index] = objects[..count].iter().any(|candidate| {
                    candidate.class == crate::storage::AccessClass::Table
                        && self.storage.access_object_name_to(*candidate, txn.txid)
                            == (schema, object_name)
                });
            }
        }
        for (index, object) in objects[..count].iter().enumerate() {
            if covered_by_relation_move[index] {
                continue;
            }
            if object.class == crate::storage::AccessClass::View {
                let (schema, object_name) = self.storage.access_object_name_to(*object, txn.txid);
                if self
                    .storage
                    .relation_name_taken(new_schema, object_name.as_str(), txn.txid)
                {
                    return Ok(Err(sql_err!(
                        sqlstate::DUPLICATE_TABLE,
                        "relation \"{}\" already exists in schema \"{}\"",
                        object_name.as_str(),
                        new_schema
                    )));
                }
                let target = match crate::storage::SqlName::parse(new_schema) {
                    Ok(schema) => schema,
                    Err(error) => return Ok(Err(error)),
                };
                let prior =
                    match self
                        .storage
                        .stage_view_schema(object.slot as usize, target, txn.txid)
                    {
                        Ok(prior) => prior,
                        Err(error) => return Ok(Err(error)),
                    };
                let lsn = self.storage.bump_lsn();
                if let Err(error) = self.wal.stage(
                    txn.txid,
                    lsn,
                    &WalOp::SetViewSchema {
                        schema: schema.as_str(),
                        name: object_name.as_str(),
                        new_schema,
                    },
                ) {
                    self.storage
                        .rollback_view_schema(object.slot as usize, prior);
                    return Ok(Err(error));
                }
                if let Err(error) = txn.record_ddl(DdlUndo::ViewSchemaChanged {
                    slot: object.slot as u32,
                    prior,
                }) {
                    self.storage
                        .rollback_view_schema(object.slot as usize, prior);
                    return Ok(Err(error));
                }
                continue;
            }
            if object.class == crate::storage::AccessClass::Index {
                let Some(table_slot) = self
                    .storage
                    .index_table_slot_to(object.slot as usize, txn.txid)
                else {
                    return Ok(Err(sql_err!(
                        sqlstate::OBJECT_NOT_IN_PREREQUISITE_STATE,
                        "extension index member has no table"
                    )));
                };
                let table_object = self.storage.table_access_object(table_slot, txn.txid);
                if !objects[..count].contains(&table_object) {
                    return Ok(Err(sql_err!(
                        sqlstate::FEATURE_NOT_SUPPORTED,
                        "an extension index can move schemas only with its member table"
                    )));
                }
                continue;
            }
            let (schema, object_name) = self.storage.access_object_name_to(*object, txn.txid);
            let mut command = crate::util::StackStr::<1024>::new();
            let keyword = match object.class {
                crate::storage::AccessClass::Table => "ALTER TABLE ",
                crate::storage::AccessClass::MaterializedView => "ALTER TABLE ",
                crate::storage::AccessClass::Sequence => "ALTER SEQUENCE ",
                crate::storage::AccessClass::Domain => "ALTER DOMAIN ",
                crate::storage::AccessClass::Enum | crate::storage::AccessClass::Composite => {
                    "ALTER TYPE "
                }
                crate::storage::AccessClass::Routine => {
                    match self
                        .storage
                        .routine_for(object.slot as usize, txn.txid)
                        .kind
                    {
                        crate::storage::RoutineKind::Procedure => "ALTER PROCEDURE ",
                        crate::storage::RoutineKind::Aggregate(_) => "ALTER AGGREGATE ",
                        _ => "ALTER FUNCTION ",
                    }
                }
                crate::storage::AccessClass::Statistics => "ALTER STATISTICS ",
                crate::storage::AccessClass::Index => continue,
                _ => unreachable!("unsupported classes were rejected above"),
            };
            let _ = write!(command, "{}", keyword);
            if let Err(error) = write_extension_qualified_identifier(
                &mut command,
                schema.as_str(),
                object_name.as_str(),
            ) {
                return Ok(Err(error));
            }
            if object.class == crate::storage::AccessClass::Routine {
                let routine = self.storage.routine_for(object.slot as usize, txn.txid);
                let arguments = match catalog::function_arguments_text(
                    &self.storage,
                    txn.txid,
                    crate::storage::routine_oid(&routine),
                    true,
                    arena,
                ) {
                    Ok(Some(arguments)) => arguments,
                    Ok(None) => {
                        return Ok(Err(sql_err!(
                            sqlstate::UNDEFINED_FUNCTION,
                            "extension routine does not exist"
                        )));
                    }
                    Err(error) => return Ok(Err(error)),
                };
                let _ = write!(command, "({})", arguments);
            }
            if write!(command, " SET SCHEMA ").is_err()
                || write_extension_identifier(&mut command, new_schema).is_err()
                || command.is_truncated()
            {
                return Ok(Err(sql_err!(
                    sqlstate::PROGRAM_LIMIT_EXCEEDED,
                    "extension member identity is too long"
                )));
            }
            let sql = match arena.alloc_str(command.as_str()) {
                Ok(sql) => sql,
                Err(_) => return Ok(Err(query::arena_full_pub())),
            };
            let mut parser = match Parser::new(sql, arena) {
                Ok(parser) => parser,
                Err(error) => return Ok(Err(parse_error_to_sql(&error))),
            };
            let statement = match parser.next_stmt() {
                Ok(Some(statement)) => statement,
                Ok(None) => {
                    return Ok(Err(sql_err!(
                        sqlstate::INTERNAL_ERROR,
                        "empty generated extension alter"
                    )));
                }
                Err(error) => return Ok(Err(parse_error_to_sql(&error))),
            };
            let result = responder.without_command_complete(|responder| {
                self.execute_stmt(
                    &statement,
                    arena,
                    params,
                    txn,
                    sqlprep,
                    cursors,
                    guc,
                    exec::PlpgsqlTransactionContext::Atomic,
                    responder,
                )
            })?;
            if let Err(error) = result {
                return Ok(Err(error));
            }
        }
        match exec::set_extension_schema(&mut self.storage, txn, name, new_schema) {
            Ok(()) => {
                responder.command_complete("ALTER EXTENSION")?;
                Ok(Ok(()))
            }
            Err(error) => Ok(Err(error)),
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn drop_extension_object(
        &mut self,
        object: crate::storage::AccessObject,
        arena: &Arena,
        params: &[Datum],
        txn: &mut TxnState,
        sqlprep: &mut SqlPreparedPool,
        cursors: &mut cursor::CursorPool,
        guc: &mut GucState,
        responder: &mut Responder,
    ) -> Result<Result<(), SqlError>, WireFull> {
        use core::fmt::Write as _;
        // An earlier member's ordinary CASCADE may already have removed this
        // object (for example a table-owned sequence or index). All extension
        // edges were detached before the first DROP, so there is no dangling
        // catalog state to clean up here.
        if !self.storage.access_object_visible_to(object, txn.txid) {
            return Ok(Ok(()));
        }
        let (schema, name) = self.storage.access_object_name_to(object, txn.txid);
        let mut command = crate::util::StackStr::<1024>::new();
        let keyword = match object.class {
            crate::storage::AccessClass::Table => "DROP TABLE ",
            crate::storage::AccessClass::View => "DROP VIEW ",
            crate::storage::AccessClass::MaterializedView => "DROP MATERIALIZED VIEW ",
            crate::storage::AccessClass::Sequence => "DROP SEQUENCE ",
            crate::storage::AccessClass::Domain => "DROP DOMAIN ",
            crate::storage::AccessClass::Enum | crate::storage::AccessClass::Composite => {
                "DROP TYPE "
            }
            crate::storage::AccessClass::Routine => "DROP ROUTINE ",
            crate::storage::AccessClass::Index => "DROP INDEX ",
            crate::storage::AccessClass::Schema => "DROP SCHEMA ",
            crate::storage::AccessClass::Statistics => "DROP STATISTICS ",
            crate::storage::AccessClass::Trigger => {
                let trigger = self.storage.trigger(object.slot as usize);
                let (relation_schema, relation_name) = match trigger.target {
                    crate::storage::TriggerTarget::Table(table) => {
                        let definition = self.storage.table_def(table as usize, txn.txid);
                        (definition.schema, definition.name)
                    }
                    crate::storage::TriggerTarget::View(view) => {
                        let definition = self.storage.view(view as usize);
                        (definition.schema, definition.name)
                    }
                };
                let _ = write!(command, "DROP TRIGGER ");
                if let Err(error) = write_extension_identifier(&mut command, name.as_str()) {
                    return Ok(Err(error));
                }
                let _ = write!(command, " ON ");
                if let Err(error) = write_extension_qualified_identifier(
                    &mut command,
                    relation_schema.as_str(),
                    relation_name.as_str(),
                ) {
                    return Ok(Err(error));
                }
                ""
            }
            crate::storage::AccessClass::EventTrigger => {
                return Ok(Err(sql_err!(
                    sqlstate::FEATURE_NOT_SUPPORTED,
                    "event triggers cannot be extension members"
                )));
            }
            crate::storage::AccessClass::Tablespace
            | crate::storage::AccessClass::Extension
            | crate::storage::AccessClass::Database
            | crate::storage::AccessClass::LargeObject
            | crate::storage::AccessClass::ForeignDataWrapper
            | crate::storage::AccessClass::ForeignServer
            | crate::storage::AccessClass::Language => {
                return Ok(Err(sql_err!(
                    sqlstate::FEATURE_NOT_SUPPORTED,
                    "unsupported extension member class"
                )));
            }
        };
        let _ = write!(command, "{}", keyword);
        if object.class != crate::storage::AccessClass::Trigger
            && let Err(error) =
                write_extension_qualified_identifier(&mut command, schema.as_str(), name.as_str())
        {
            return Ok(Err(error));
        }
        if object.class == crate::storage::AccessClass::Routine {
            let routine = self.storage.routine_for(object.slot as usize, txn.txid);
            let arguments = match catalog::function_arguments_text(
                &self.storage,
                txn.txid,
                crate::storage::routine_oid(&routine),
                true,
                arena,
            ) {
                Ok(Some(arguments)) => arguments,
                Ok(None) => {
                    return Ok(Err(sql_err!(
                        sqlstate::UNDEFINED_FUNCTION,
                        "extension routine member does not exist"
                    )));
                }
                Err(error) => return Ok(Err(error)),
            };
            if write!(command, "({})", arguments).is_err() {
                return Ok(Err(sql_err!(
                    sqlstate::PROGRAM_LIMIT_EXCEEDED,
                    "extension member identity is too long"
                )));
            }
        }
        if write!(command, " CASCADE").is_err() {
            return Ok(Err(sql_err!(
                sqlstate::PROGRAM_LIMIT_EXCEEDED,
                "extension member identity is too long"
            )));
        }
        let sql = match arena.alloc_str(command.as_str()) {
            Ok(sql) => sql,
            Err(_) => return Ok(Err(query::arena_full_pub())),
        };
        let mut parser = match Parser::new(sql, arena) {
            Ok(parser) => parser,
            Err(error) => return Ok(Err(parse_error_to_sql(&error))),
        };
        let statement = match parser.next_stmt() {
            Ok(Some(statement)) => statement,
            Ok(None) => {
                return Ok(Err(sql_err!(
                    sqlstate::INTERNAL_ERROR,
                    "empty generated extension drop"
                )));
            }
            Err(error) => return Ok(Err(parse_error_to_sql(&error))),
        };
        responder.without_command_complete(|responder| {
            self.execute_stmt(
                &statement,
                arena,
                params,
                txn,
                sqlprep,
                cursors,
                guc,
                exec::PlpgsqlTransactionContext::Atomic,
                responder,
            )
        })
    }

    #[allow(clippy::too_many_arguments)]
    fn drop_extension(
        &mut self,
        names: &[&str],
        if_exists: bool,
        cascade: bool,
        arena: &Arena,
        params: &[Datum],
        txn: &mut TxnState,
        sqlprep: &mut SqlPreparedPool,
        cursors: &mut cursor::CursorPool,
        guc: &mut GucState,
        responder: &mut Responder,
    ) -> Result<Result<(), SqlError>, WireFull> {
        for name in names {
            let Some(extension) = self.storage.extension_slot(name, txn.txid) else {
                if if_exists {
                    responder.notice(
                        sqlstate::SUCCESSFUL_COMPLETION,
                        stack_format!(128, "extension \"{}\" does not exist, skipping", name)
                            .as_str(),
                    )?;
                    continue;
                }
                return Ok(Err(sql_err!(
                    sqlstate::UNDEFINED_OBJECT,
                    "extension \"{}\" does not exist",
                    name
                )));
            };
            let mut dependent_extensions =
                [crate::storage::SqlName::EMPTY; crate::storage::MAX_EXTENSIONS];
            let mut dependent_count = 0usize;
            for (_, dependency) in self.storage.extension_dependencies_visible_to(txn.txid) {
                if dependency.extension as usize == extension
                    && dependency.kind == crate::storage::ExtensionDependencyKind::Required
                    && dependency.object.class == crate::storage::AccessClass::Extension
                {
                    dependent_extensions[dependent_count] =
                        self.storage.extension(dependency.object.slot as usize).name;
                    dependent_count += 1;
                }
            }
            if dependent_count != 0 && !cascade {
                return Ok(Err(sql_err!(
                    sqlstate::DEPENDENT_OBJECTS_STILL_EXIST,
                    "cannot drop extension \"{}\" because other extensions depend on it",
                    name
                )));
            }
            for dependent in &dependent_extensions[..dependent_count] {
                let targets = [dependent.as_str()];
                let result = responder.without_command_complete(|responder| {
                    self.drop_extension(
                        &targets, false, true, arena, params, txn, sqlprep, cursors, guc, responder,
                    )
                })?;
                if let Err(error) = result {
                    return Ok(Err(error));
                }
            }
            let mut objects = [crate::storage::AccessObject {
                class: crate::storage::AccessClass::Table,
                slot: 0,
            }; crate::storage::MAX_EXTENSION_DEPENDENCIES];
            let mut kinds = [crate::storage::ExtensionDependencyKind::Member;
                crate::storage::MAX_EXTENSION_DEPENDENCIES];
            let mut object_count = 0usize;
            for (_, dependency) in self.storage.extension_dependencies_visible_to(txn.txid) {
                if dependency.extension as usize == extension
                    && matches!(
                        dependency.kind,
                        crate::storage::ExtensionDependencyKind::Member
                            | crate::storage::ExtensionDependencyKind::Automatic
                    )
                {
                    objects[object_count] = dependency.object;
                    kinds[object_count] = dependency.kind;
                    object_count += 1;
                }
            }
            let mut covered_by_matview = [false; crate::storage::MAX_EXTENSION_DEPENDENCIES];
            for index in 0..object_count {
                if objects[index].class != crate::storage::AccessClass::Table {
                    continue;
                }
                let identity = self.storage.access_object_name_to(objects[index], txn.txid);
                covered_by_matview[index] = objects[..object_count].iter().any(|candidate| {
                    candidate.class == crate::storage::AccessClass::MaterializedView
                        && self.storage.access_object_name_to(*candidate, txn.txid) == identity
                });
            }
            // Detach the entire member set before executing a cascading DROP.
            // Ordinary dependency processing may sweep several members at
            // once, but cannot then leave extension edges aimed at dead slots.
            for index in 0..object_count {
                let (dependency, prior) = match self.storage.change_extension_dependency(
                    extension,
                    objects[index],
                    kinds[index],
                    false,
                    txn.txid,
                ) {
                    Ok(changed) => changed,
                    Err(error) => return Ok(Err(error)),
                };
                if let Err(error) = txn.record_ddl(DdlUndo::ExtensionDependencyChanged {
                    slot: dependency as u32,
                    prior,
                }) {
                    self.storage
                        .rollback_extension_dependency(dependency, prior);
                    return Ok(Err(error));
                }
            }
            // Namespace members go last: their contained objects have their own
            // dependency edges and must publish their ordinary DROP WAL first.
            for schema_pass in [false, true] {
                for index in 0..object_count {
                    let object = objects[index];
                    if covered_by_matview[index] {
                        continue;
                    }
                    if (object.class == crate::storage::AccessClass::Schema) != schema_pass {
                        continue;
                    }
                    let result = self.drop_extension_object(
                        object, arena, params, txn, sqlprep, cursors, guc, responder,
                    )?;
                    if let Err(error) = result {
                        return Ok(Err(error));
                    }
                }
            }
            let extension_object = crate::storage::AccessObject {
                class: crate::storage::AccessClass::Extension,
                slot: extension as u16,
            };
            let mut requirement_slots = [usize::MAX; crate::storage::MAX_EXTENSION_REQUIRES];
            let mut requirement_count = 0usize;
            for (slot, dependency) in self.storage.extension_dependencies_visible_to(txn.txid) {
                if dependency.object == extension_object
                    && dependency.kind == crate::storage::ExtensionDependencyKind::Required
                {
                    requirement_slots[requirement_count] = slot;
                    requirement_count += 1;
                }
            }
            for slot in &requirement_slots[..requirement_count] {
                let dependency = *self.storage.extension_dependency(*slot);
                let (changed, prior) = match self.storage.change_extension_dependency(
                    dependency.extension as usize,
                    dependency.object,
                    dependency.kind,
                    false,
                    txn.txid,
                ) {
                    Ok(changed) => changed,
                    Err(error) => return Ok(Err(error)),
                };
                if let Err(error) = txn.record_ddl(DdlUndo::ExtensionDependencyChanged {
                    slot: changed as u32,
                    prior,
                }) {
                    self.storage.rollback_extension_dependency(changed, prior);
                    return Ok(Err(error));
                }
            }
            if let Err(error) =
                exec::drop_extension_catalog(&mut self.storage, txn, name, false, cascade)
            {
                return Ok(Err(error));
            }
        }
        responder.command_complete("DROP EXTENSION")?;
        Ok(Ok(()))
    }

    #[allow(clippy::too_many_arguments)]
    fn create_extension(
        &mut self,
        name: &str,
        if_not_exists: bool,
        schema: Option<&str>,
        version: Option<&str>,
        cascade: bool,
        arena: &Arena,
        params: &[Datum],
        txn: &mut TxnState,
        sqlprep: &mut SqlPreparedPool,
        cursors: &mut cursor::CursorPool,
        guc: &mut GucState,
        responder: &mut Responder,
    ) -> Result<Result<(), SqlError>, WireFull> {
        let mut installing =
            [crate::storage::SqlName::EMPTY; crate::storage::MAX_EXTENSION_REQUIRES + 1];
        self.create_extension_inner(
            name,
            if_not_exists,
            schema,
            version,
            cascade,
            &mut installing,
            0,
            arena,
            params,
            txn,
            sqlprep,
            cursors,
            guc,
            responder,
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn create_extension_inner(
        &mut self,
        name: &str,
        if_not_exists: bool,
        schema: Option<&str>,
        version: Option<&str>,
        cascade: bool,
        installing: &mut [crate::storage::SqlName; crate::storage::MAX_EXTENSION_REQUIRES + 1],
        depth: usize,
        arena: &Arena,
        params: &[Datum],
        txn: &mut TxnState,
        sqlprep: &mut SqlPreparedPool,
        cursors: &mut cursor::CursorPool,
        guc: &mut GucState,
        responder: &mut Responder,
    ) -> Result<Result<(), SqlError>, WireFull> {
        let parsed_name = match crate::storage::SqlName::parse(name) {
            Ok(name) => name,
            Err(error) => return Ok(Err(error)),
        };
        if installing[..depth].contains(&parsed_name) {
            return Ok(Err(sql_err!(
                sqlstate::INVALID_RECURSION,
                "cyclic extension requirement involving \"{}\"",
                name
            )));
        }
        if depth == installing.len() {
            return Ok(Err(sql_err!(
                sqlstate::PROGRAM_LIMIT_EXCEEDED,
                "extension requirement depth exceeds {}",
                installing.len()
            )));
        }
        installing[depth] = parsed_name;
        if self.storage.extension_slot(name, txn.txid).is_some() {
            if if_not_exists {
                responder.notice(
                    sqlstate::SUCCESSFUL_COMPLETION,
                    stack_format!(128, "extension \"{}\" already exists, skipping", name).as_str(),
                )?;
                responder.command_complete("CREATE EXTENSION")?;
                return Ok(Ok(()));
            }
            return Ok(Err(sql_err!(
                sqlstate::DUPLICATE_OBJECT,
                "extension \"{}\" already exists",
                name
            )));
        }
        let package = match self.storage.extension_package_for_version(name, version) {
            Ok((_, _, package)) => package,
            Err(error) => return Ok(Err(error)),
        };
        let cascade_schema = match package.schema {
            Some(schema) => schema,
            None => match schema {
                Some(schema) => match crate::storage::SqlName::parse(schema) {
                    Ok(schema) => schema,
                    Err(error) => return Ok(Err(error)),
                },
                None => match self.storage.creation_schema(None, name, txn.txid) {
                    Ok(schema) => schema,
                    Err(error) => return Ok(Err(error)),
                },
            },
        };
        for required in package.requires() {
            if self
                .storage
                .extension_slot(required.as_str(), txn.txid)
                .is_none()
            {
                if !cascade {
                    return Ok(Err(sql_err!(
                        sqlstate::OBJECT_NOT_IN_PREREQUISITE_STATE,
                        "required extension \"{}\" is not installed",
                        required.as_str()
                    )));
                }
                let required_package = match self
                    .storage
                    .extension_package_for_version(required.as_str(), None)
                {
                    Ok((_, _, package)) => package,
                    Err(error) => return Ok(Err(error)),
                };
                let required_schema = required_package.schema.unwrap_or(cascade_schema);
                let result = responder.without_command_complete(|responder| {
                    self.create_extension_inner(
                        required.as_str(),
                        false,
                        Some(required_schema.as_str()),
                        None,
                        true,
                        installing,
                        depth + 1,
                        arena,
                        params,
                        txn,
                        sqlprep,
                        cursors,
                        guc,
                        responder,
                    )
                })?;
                if let Err(error) = result {
                    return Ok(Err(error));
                }
            }
        }
        if let Some(fixed_schema) = package.schema
            && self
                .storage
                .find_schema_visible(fixed_schema.as_str(), txn.txid)
                .is_none()
        {
            let outcome = responder.without_command_complete(|responder| {
                exec::create_schema(
                    &mut self.storage,
                    &mut self.wal,
                    txn,
                    fixed_schema.as_str(),
                    None,
                    false,
                    responder,
                )
            })?;
            if let Err(error) = outcome {
                return Ok(Err(error));
            }
        }
        let owner = guc.current_role();
        let plan = match exec::prepare_create_extension(
            &mut self.storage,
            txn,
            name,
            if_not_exists,
            schema,
            version,
        ) {
            Ok(Some(plan)) => plan,
            Ok(None) => {
                responder.notice(
                    sqlstate::SUCCESSFUL_COMPLETION,
                    stack_format!(128, "extension \"{}\" already exists, skipping", name).as_str(),
                )?;
                responder.command_complete("CREATE EXTENSION")?;
                return Ok(Ok(()));
            }
            Err(error) => return Ok(Err(error)),
        };
        if let Err(error) = self.reconcile_extension_requirements(plan.extension, package, txn) {
            return Ok(Err(error));
        }
        if let Some(fixed_schema) = package.schema
            && let Some(schema_object) = self.storage.resolve_access_object(
                crate::storage::AccessClass::Schema,
                "",
                fixed_schema.as_str(),
                txn.txid,
            )
            && let Err(error) =
                exec::record_extension_member(&mut self.storage, txn, plan.extension, schema_object)
        {
            return Ok(Err(error));
        }
        for script in &plan.scripts[..plan.script_count] {
            let effective = self.storage.extension_script(*script as usize).effective;
            for required in effective.requires() {
                if self
                    .storage
                    .extension_slot(required.as_str(), txn.txid)
                    .is_some()
                {
                    continue;
                }
                if !cascade {
                    return Ok(Err(sql_err!(
                        sqlstate::OBJECT_NOT_IN_PREREQUISITE_STATE,
                        "required extension \"{}\" is not installed",
                        required.as_str()
                    )));
                }
                let namespace = self
                    .storage
                    .extension(plan.extension)
                    .definition_to(txn.txid)
                    .0 as usize;
                let cascade_schema = self.storage.schema_def(namespace).name;
                let required_package = match self
                    .storage
                    .extension_package_for_version(required.as_str(), None)
                {
                    Ok((_, _, package)) => package,
                    Err(error) => return Ok(Err(error)),
                };
                let required_schema = required_package.schema.unwrap_or(cascade_schema);
                let result = responder.without_command_complete(|responder| {
                    self.create_extension_inner(
                        required.as_str(),
                        false,
                        Some(required_schema.as_str()),
                        None,
                        true,
                        installing,
                        depth + 1,
                        arena,
                        params,
                        txn,
                        sqlprep,
                        cursors,
                        guc,
                        responder,
                    )
                })?;
                if let Err(error) = result {
                    return Ok(Err(error));
                }
            }
        }
        let package = match self.execute_extension_plan(
            plan,
            owner.as_str(),
            arena,
            params,
            txn,
            sqlprep,
            cursors,
            guc,
            responder,
        )? {
            Ok(package) => package,
            Err(error) => return Ok(Err(error)),
        };
        if !package.comment.as_str().is_empty() {
            let outcome = responder.without_command_complete(|responder| {
                exec::comment(
                    &mut self.storage,
                    &mut self.wal,
                    txn,
                    &ast::CommentTarget::Extension(name),
                    Some(package.comment.as_str()),
                    arena,
                    responder,
                )
            })?;
            if let Err(error) = outcome {
                return Ok(Err(error));
            }
        }
        responder.command_complete("CREATE EXTENSION")?;
        Ok(Ok(()))
    }

    fn show(
        &mut self,
        name: &str,
        guc: &GucState,
        txn: &TxnState,
        responder: &mut Responder,
    ) -> Result<Result<(), SqlError>, WireFull> {
        // Session GUCs come from the per-session store; the rest are fixed
        // server parameters.
        let value = if let Some(value) = self
            .fixed_setting_for(name, txn)
            .or_else(|| guc.get_owned(name))
        {
            value
        } else {
            return Ok(Err(SqlError {
                sqlstate: SqlState::known(sqlstate::UNDEFINED_OBJECT),
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
        responder.data_row(&[Datum::Text(value.as_str())])?;
        responder.command_complete("SHOW")?;
        Ok(Ok(()))
    }

    /// SHOW ALL: every readable setting as (name, setting, description). Tools
    /// read name/setting; descriptions are left empty.
    fn show_all(
        &mut self,
        guc: &GucState,
        txn: &TxnState,
        responder: &mut Responder,
    ) -> Result<Result<(), SqlError>, WireFull> {
        responder.row_description(&[
            ColDesc::new("name", types::oid::TEXT, -1),
            ColDesc::new("setting", types::oid::TEXT, -1),
            ColDesc::new("description", types::oid::TEXT, -1),
        ])?;
        for &name in SETTING_NAMES {
            if let Some(value) = self
                .fixed_setting_for(name, txn)
                .or_else(|| guc.get_owned(name))
            {
                responder.data_row(&[
                    Datum::Text(name),
                    Datum::Text(value.as_str()),
                    Datum::Text(""),
                ])?;
            }
        }
        responder.command_complete("SHOW")?;
        Ok(Ok(()))
    }

    fn fixed_setting_for(&self, name: &str, txn: &TxnState) -> Option<crate::util::StackStr<256>> {
        if name.eq_ignore_ascii_case("transaction_isolation") {
            return Some(crate::util::StackStr::from_str(txn.isolation.as_str()));
        }
        if name.eq_ignore_ascii_case("transaction_read_only") {
            return Some(crate::util::StackStr::from_str(if txn.read_only {
                "on"
            } else {
                "off"
            }));
        }
        if name.eq_ignore_ascii_case("transaction_deferrable") {
            return Some(crate::util::StackStr::from_str(if txn.deferrable {
                "on"
            } else {
                "off"
            }));
        }
        if name.eq_ignore_ascii_case("is_superuser") {
            let role = crate::sql::eval::funcs::system::current_user_owned();
            let superuser = self
                .storage
                .find_role_visible(role.as_str(), txn.txid)
                .is_some_and(|slot| self.storage.role(slot).attributes_to(txn.txid).superuser);
            return Some(crate::util::StackStr::from_str(if superuser {
                "on"
            } else {
                "off"
            }));
        }
        if name.eq_ignore_ascii_case("max_connections") {
            return Some(stack_format!(256, "{}", self.max_connections));
        }
        if name.eq_ignore_ascii_case("max_prepared_transactions") {
            return Some(stack_format!(256, "{}", self.max_prepared_transactions));
        }
        fixed_setting(name).map(crate::util::StackStr::from_str)
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
    "default_transaction_deferrable",
    "default_transaction_isolation",
    "default_transaction_read_only",
    "default_table_access_method",
    "default_tablespace",
    "default_text_search_config",
    "extra_float_digits",
    "idle_in_transaction_session_timeout",
    "integer_datetimes",
    "IntervalStyle",
    "is_superuser",
    "lock_timeout",
    "max_connections",
    "max_prepared_transactions",
    "row_security",
    "search_path",
    "server_encoding",
    "server_version",
    "server_version_num",
    "standard_conforming_strings",
    "statement_timeout",
    "synchronize_seqscans",
    "TimeZone",
    "transaction_deferrable",
    "transaction_isolation",
    "transaction_read_only",
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
        sqlstate: SqlState::known(error.sqlstate),
        message: stack_format!(192, "{}", error.message.as_str()),
    }
}

/// Rewrites a CREATE SCHEMA element to create inside the new schema. An
/// element that already names that schema passes through; one naming another
/// schema is PostgreSQL's 42P15.
pub(crate) fn requalify_schema_element<'a>(
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
            columns,
            or_replace,
            security,
            security_barrier,
            check_option,
            sql,
        } => Stmt::CreateView {
            name: requalify(*name)?,
            columns,
            or_replace: *or_replace,
            security: *security,
            security_barrier: *security_barrier,
            check_option: *check_option,
            sql,
        },
        ast::CreateSchemaElement::Index {
            name,
            table,
            build,
            scope,
            if_not_exists,
            columns,
            include_columns,
            nulls_not_distinct,
            predicate,
            predicate_text,
            options,
            tablespace,
            unique,
        } => Stmt::CreateIndex {
            name: *name,
            table: requalify(*table)?,
            build: *build,
            scope: *scope,
            if_not_exists: *if_not_exists,
            columns,
            include_columns,
            nulls_not_distinct: *nulls_not_distinct,
            predicate: *predicate,
            predicate_text: *predicate_text,
            options: *options,
            tablespace: *tablespace,
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
        ast::CreateSchemaElement::Trigger(trigger) => {
            let kind = match trigger.kind {
                ast::TriggerKind::Ordinary => ast::TriggerKind::Ordinary,
                ast::TriggerKind::Constraint {
                    referenced_table,
                    timing,
                } => ast::TriggerKind::Constraint {
                    referenced_table: referenced_table.map(requalify).transpose()?,
                    timing,
                },
            };
            Stmt::CreateTrigger(ast::CreateTrigger {
                table: requalify(trigger.table)?,
                kind,
                ..*trigger
            })
        }
        ast::CreateSchemaElement::Grant {
            privileges,
            target,
            grantees,
            grant_option,
            grantor,
        } => {
            let target = match target {
                ast::PrivilegeTarget::Objects {
                    kind:
                        kind @ (ast::PrivilegeObjectKind::Table | ast::PrivilegeObjectKind::Sequence),
                    names,
                } => {
                    for name in names.iter().copied() {
                        requalify(name)?;
                    }
                    let names = arena
                        .alloc_slice_with(names.len(), |index| {
                            requalify(names[index])
                                .expect("schema grant names were validated before allocation")
                        })
                        .map_err(|_| query::arena_full_pub())?;
                    ast::PrivilegeTarget::Objects { kind: *kind, names }
                }
                _ => *target,
            };
            Stmt::GrantPrivileges {
                privileges,
                target,
                grantees,
                grant_option: *grant_option,
                grantor: *grantor,
            }
        }
    };
    arena
        .alloc(rewritten)
        .map(|r| &*r)
        .map_err(|_| query::arena_full_pub())
}

/// The runtime form of a parsed `CREATE SCHEMA` identity. Role keywords are
/// resolved once against the session before any catalog mutation begins.
pub(crate) struct ResolvedCreateSchema {
    pub name: crate::util::StackStr<64>,
    pub authorization: Option<crate::util::StackStr<64>>,
}

pub(crate) fn resolve_create_schema(
    name: ast::SchemaName<'_>,
    authorization: Option<ast::SchemaAuthorization<'_>>,
    guc: &guc::GucState,
) -> Result<ResolvedCreateSchema, SqlError> {
    let authorization = match authorization {
        Some(ast::SchemaAuthorization::Name(role)) => Some(crate::util::StackStr::from_str(role)),
        Some(ast::SchemaAuthorization::CurrentRole | ast::SchemaAuthorization::CurrentUser) => {
            Some(guc.current_role())
        }
        Some(ast::SchemaAuthorization::SessionUser) => Some(guc.session_user()),
        None => None,
    };
    let name = match name {
        ast::SchemaName::Explicit(name) => crate::util::StackStr::from_str(name),
        ast::SchemaName::Authorization => authorization.ok_or_else(|| {
            sql_err!(
                crate::sql::eval::sqlstate::SYNTAX_ERROR,
                "CREATE SCHEMA AUTHORIZATION requires a role"
            )
        })?,
    };
    Ok(ResolvedCreateSchema {
        name,
        authorization,
    })
}

/// Reapplies one journal record to storage during recovery.
fn decode_wal_routine_signature(
    encoded: &[u8],
) -> Result<
    (
        [crate::storage::RoutineArgumentDef; crate::storage::MAX_ROUTINE_ARGUMENTS],
        usize,
    ),
    SqlError,
> {
    let count = usize::from(*encoded.first().ok_or_else(|| {
        sql_err!(
            sqlstate::DATA_EXCEPTION,
            "routine WAL signature is missing its argument count"
        )
    })?);
    if count > crate::storage::MAX_ROUTINE_ARGUMENTS {
        return Err(sql_err!(
            sqlstate::PROGRAM_LIMIT_EXCEEDED,
            "routine WAL signature has too many arguments"
        ));
    }
    let mut arguments =
        [crate::storage::RoutineArgumentDef::EMPTY; crate::storage::MAX_ROUTINE_ARGUMENTS];
    let mut at = 1;
    for argument in &mut arguments[..count] {
        argument.ctype =
            crate::sql::types::ColType::from_code(*encoded.get(at).ok_or_else(|| {
                sql_err!(
                    sqlstate::DATA_EXCEPTION,
                    "routine WAL signature is truncated"
                )
            })?)
            .ok_or_else(|| {
                sql_err!(
                    sqlstate::DATA_EXCEPTION,
                    "routine WAL signature has invalid type"
                )
            })?;
        at += 1;
        let tag = *encoded.get(at).ok_or_else(|| {
            sql_err!(
                sqlstate::DATA_EXCEPTION,
                "routine WAL signature is truncated"
            )
        })?;
        at += 1;
        if tag == 1 {
            let mut names = [crate::storage::SqlName::EMPTY; 2];
            for name in &mut names {
                let len = usize::from(*encoded.get(at).ok_or_else(|| {
                    sql_err!(
                        sqlstate::DATA_EXCEPTION,
                        "routine WAL type identity is truncated"
                    )
                })?);
                at += 1;
                let bytes = encoded.get(at..at + len).ok_or_else(|| {
                    sql_err!(
                        sqlstate::DATA_EXCEPTION,
                        "routine WAL type identity is truncated"
                    )
                })?;
                *name =
                    crate::storage::SqlName::parse(core::str::from_utf8(bytes).map_err(|_| {
                        sql_err!(
                            sqlstate::DATA_EXCEPTION,
                            "routine WAL type identity is not UTF-8"
                        )
                    })?)?;
                at += len;
            }
            argument.user_type = Some(crate::storage::UserTypeName {
                schema: names[0],
                name: names[1],
            });
        } else if tag != 0 {
            return Err(sql_err!(
                sqlstate::DATA_EXCEPTION,
                "routine WAL signature has invalid identity tag"
            ));
        }
    }
    if at != encoded.len() {
        return Err(sql_err!(
            sqlstate::DATA_EXCEPTION,
            "routine WAL signature has trailing bytes"
        ));
    }
    Ok((arguments, count))
}

fn replay_transaction_batches(
    storage: &mut Storage,
    prepared: &mut two_phase::PreparedTransactions,
    recovered: &std::collections::BTreeMap<u64, Vec<u8>>,
    apply_floor: u64,
) -> Result<(), SqlError> {
    let mut batch: Vec<(u64, &[u8])> = Vec::new();
    for (lsn, record) in recovered {
        let operator = crate::wal::decode_record(record).ok_or_else(|| {
            sql_err!(
                sqlstate::INTERNAL_ERROR,
                "corrupt uploaded WAL record at LSN {} (kind {})",
                lsn,
                record.first().copied().unwrap_or_default()
            )
        })?;
        let terminal = matches!(
            operator,
            WalOp::Commit { .. } | WalOp::PrepareTransaction { .. }
        );
        batch.push((*lsn, record));
        if !terminal {
            continue;
        }
        match operator {
            WalOp::PrepareTransaction {
                transaction_id,
                owner,
                database,
                prepared_at,
                gid,
            } => {
                let gid = ast::PreparedTransactionId::parse(gid).ok_or_else(|| {
                    sql_err!(
                        sqlstate::DATA_EXCEPTION,
                        "prepared transaction WAL has an invalid identifier"
                    )
                })?;
                if prepared.find(gid).is_some() {
                    return Err(sql_err!(
                        sqlstate::DUPLICATE_OBJECT,
                        "recovered prepared transaction identifier \"{}\" is duplicated",
                        gid.as_str()
                    ));
                }
                let database = crate::storage::DatabaseOid::parse(database).ok_or_else(|| {
                    sql_err!(
                        sqlstate::DATA_EXCEPTION,
                        "prepared transaction WAL has an invalid database"
                    )
                })?;
                let metadata = two_phase::PreparedTransactionMetadata {
                    gid,
                    transaction_id,
                    owner,
                    database,
                    prepared_at,
                    first_lsn: batch.first().map_or(*lsn, |(record_lsn, _)| *record_lsn),
                    prepared_lsn: *lsn,
                };
                let slot = prepared.reserve(metadata).ok_or_else(|| {
                    sql_err!(
                        sqlstate::OUT_OF_MEMORY,
                        "recovered prepared transactions exceed max_prepared_transactions ({})",
                        prepared.capacity()
                    )
                })?;
                prepared
                    .slot_mut(slot)
                    .transaction
                    .restore_prepared_identity(transaction_id);
                storage.select_database_for_recovery(database)?;
                for (record_lsn, raw) in &batch[..batch.len() - 1] {
                    prepared.slot_mut(slot).push_record(*record_lsn, raw)?;
                }
                for (_, raw) in &batch[..batch.len() - 1] {
                    match crate::wal::decode_record(raw).ok_or_else(|| {
                        sql_err!(
                            sqlstate::DATA_EXCEPTION,
                            "prepared transaction WAL is corrupt"
                        )
                    })? {
                        WalOp::CreateTable(definition) => {
                            if storage
                                .table_access_method_name(definition.access_method, transaction_id)
                                .is_none()
                            {
                                return Err(sql_err!(
                                    sqlstate::INTERNAL_ERROR,
                                    "prepared transaction references an unknown table access method"
                                ));
                            }
                            if storage
                                .find_visible(
                                    definition.schema.as_str(),
                                    definition.name.as_str(),
                                    transaction_id,
                                )
                                .is_none()
                            {
                                let table = storage.create_table_in(definition, transaction_id)?;
                                prepared
                                    .slot_mut(slot)
                                    .transaction
                                    .record_ddl(DdlUndo::Created(table as u32))?;
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
                            if let Some(sequence) =
                                storage.sequence_slot(schema, name, transaction_id)
                            {
                                let prior = storage.stage_sequence_alter(
                                    sequence,
                                    crate::storage::SequenceAlteration {
                                        schema: crate::storage::SqlName::parse(schema)?,
                                        name: crate::storage::SqlName::parse(name)?,
                                        spec,
                                        owner,
                                        generator_for,
                                        restart: None,
                                    },
                                    transaction_id,
                                )?;
                                prepared.slot_mut(slot).transaction.record_ddl(
                                    DdlUndo::SequenceAltered {
                                        slot: sequence as u32,
                                        prior,
                                    },
                                )?;
                            } else {
                                let sequence = storage.create_sequence(
                                    crate::storage::SqlName::parse(schema)?,
                                    crate::storage::SqlName::parse(name)?,
                                    spec,
                                    owner,
                                    generator_for,
                                    transaction_id,
                                )?;
                                prepared
                                    .slot_mut(slot)
                                    .transaction
                                    .record_ddl(DdlUndo::SequenceCreated(sequence as u32))?;
                            }
                        }
                        WalOp::PreparedLocks {
                            transaction_id: lock_owner,
                            encoded,
                        } => {
                            if lock_owner != transaction_id {
                                return Err(sql_err!(
                                    sqlstate::DATA_EXCEPTION,
                                    "prepared lock WAL has the wrong transaction owner"
                                ));
                            }
                            storage.restore_transaction_locks(transaction_id, encoded)?;
                        }
                        WalOp::Upsert {
                            schema,
                            table,
                            rowid,
                            row,
                            command_id,
                            ..
                        } => {
                            let Some(table_slot) =
                                storage.find_visible(schema, table, transaction_id)
                            else {
                                continue;
                            };
                            let (location, bytes) = storage.heap.append(row.len())?;
                            bytes.copy_from_slice(row);
                            storage.observe_rowid(rowid);
                            let prior = storage.write_pending(
                                table_slot,
                                rowid,
                                transaction_id,
                                command_id,
                                Some(location),
                            )?;
                            prepared.slot_mut(slot).transaction.touch(
                                table_slot as u32,
                                rowid,
                                prior,
                            )?;
                        }
                        WalOp::Delete {
                            schema,
                            table,
                            rowid,
                            command_id,
                            ..
                        } => {
                            let Some(table_slot) =
                                storage.find_visible(schema, table, transaction_id)
                            else {
                                continue;
                            };
                            let prior = storage.write_pending(
                                table_slot,
                                rowid,
                                transaction_id,
                                command_id,
                                None,
                            )?;
                            prepared.slot_mut(slot).transaction.touch(
                                table_slot as u32,
                                rowid,
                                prior,
                            )?;
                        }
                        WalOp::SequenceSet {
                            schema,
                            table,
                            column,
                            last,
                        } => {
                            if let Some(table_slot) = storage.find_table(schema, table)
                                && usize::from(column) < crate::storage::MAX_COLUMNS
                            {
                                storage.table_mut(table_slot).serial_last[usize::from(column)] =
                                    last;
                            }
                        }
                        WalOp::SequenceAdvance {
                            schema,
                            name,
                            last,
                            is_called,
                        } => {
                            if let Some(sequence) =
                                storage.sequence_slot(schema, name, transaction_id)
                                && storage.sequence_slot(schema, name, 0).is_none()
                            {
                                storage.set_sequence_value(
                                    sequence,
                                    transaction_id,
                                    last,
                                    is_called,
                                )?;
                                storage.clear_sequence_value_dirty(sequence, transaction_id);
                            } else {
                                storage.apply_sequence_advance(schema, name, last, is_called);
                            }
                        }
                        _ => {}
                    }
                }
                prepared.slot_mut(slot).recovered = true;
                storage.set_lsn(*lsn);
            }
            WalOp::Commit { .. } => {
                let mut resolution: Option<(bool, ast::PreparedTransactionId)> = None;
                for (_, raw) in &batch[..batch.len() - 1] {
                    match crate::wal::decode_record(raw).ok_or_else(|| {
                        sql_err!(sqlstate::DATA_EXCEPTION, "transaction WAL is corrupt")
                    })? {
                        WalOp::CommitPrepared { gid } | WalOp::RollbackPrepared { gid } => {
                            if resolution.is_some() {
                                return Err(sql_err!(
                                    sqlstate::DATA_EXCEPTION,
                                    "one WAL transaction contains multiple prepared resolutions"
                                ));
                            }
                            let commit = matches!(
                                crate::wal::decode_record(raw),
                                Some(WalOp::CommitPrepared { .. })
                            );
                            let gid = ast::PreparedTransactionId::parse(gid).ok_or_else(|| {
                                sql_err!(
                                    sqlstate::DATA_EXCEPTION,
                                    "prepared resolution WAL has an invalid identifier"
                                )
                            })?;
                            resolution = Some((commit, gid));
                        }
                        _ => {}
                    }
                }
                if let Some((commit, gid)) = resolution {
                    let slot = prepared.find(gid).ok_or_else(|| {
                        sql_err!(
                            sqlstate::UNDEFINED_OBJECT,
                            "prepared resolution targets unknown transaction \"{}\"",
                            gid.as_str()
                        )
                    })?;
                    if commit && *lsn > apply_floor {
                        prepared.slot(slot).visit_records(|_, raw| {
                            let operation = crate::wal::decode_record(raw).ok_or_else(|| {
                                sql_err!(
                                    sqlstate::DATA_EXCEPTION,
                                    "prepared transaction WAL is corrupt"
                                )
                            })?;
                            apply_wal_op(storage, *lsn, operation)
                        })?;
                    }
                    prepared.release(slot);
                    storage.set_lsn(*lsn);
                } else {
                    if *lsn > apply_floor {
                        for (record_lsn, raw) in &batch {
                            let operation = crate::wal::decode_record(raw).ok_or_else(|| {
                                sql_err!(sqlstate::DATA_EXCEPTION, "transaction WAL is corrupt")
                            })?;
                            apply_wal_op(storage, *record_lsn, operation)?;
                        }
                    }
                }
            }
            _ => unreachable!("terminal marker was matched above"),
        }
        batch.clear();
    }
    if !batch.is_empty() {
        return Err(sql_err!(
            sqlstate::DATA_EXCEPTION,
            "recovery input ends inside a transaction"
        ));
    }
    Ok(())
}

fn apply_wal_op(storage: &mut Storage, lsn: u64, operator: WalOp) -> Result<(), SqlError> {
    match operator {
        WalOp::DatabaseScope { oid } => {
            let database = crate::storage::DatabaseOid::parse(oid)
                .ok_or_else(|| sql_err!(sqlstate::INTERNAL_ERROR, "corrupt WAL database scope"))?;
            storage.select_database(database)?;
        }
        WalOp::CreateLargeObject {
            oid,
            created_at,
            allocated,
        } => {
            let oid = ast::LargeObjectId::parse(oid)
                .ok_or_else(|| sql_err!(sqlstate::INTERNAL_ERROR, "corrupt large-object OID"))?;
            storage.restore_large_object(oid, created_at, allocated)?;
        }
        WalOp::DropLargeObject { oid } => {
            let oid = ast::LargeObjectId::parse(oid)
                .ok_or_else(|| sql_err!(sqlstate::INTERNAL_ERROR, "corrupt large-object OID"))?;
            let slot = storage.drop_large_object(oid, 0)?.ok_or_else(|| {
                sql_err!(
                    sqlstate::UNDEFINED_OBJECT,
                    "journal drops unknown large object {}",
                    oid.get()
                )
            })?;
            storage.commit_large_object_drop(slot);
        }
        WalOp::SetForeignDataWrapper {
            slot,
            created_at,
            owner,
            definition,
        } => storage.replay_set_foreign_wrapper(slot as usize, created_at, owner, definition)?,
        WalOp::SetForeignServer {
            slot,
            created_at,
            owner,
            definition,
        } => storage.replay_set_foreign_server(slot as usize, created_at, owner, definition)?,
        WalOp::SetUserMapping {
            slot,
            created_at,
            definition,
        } => storage.replay_set_foreign_user_mapping(slot as usize, created_at, definition)?,
        WalOp::SetForeignTable {
            slot,
            created_at,
            definition,
        } => storage.replay_set_foreign_table(slot as usize, created_at, definition)?,
        WalOp::Commit { .. }
        | WalOp::PrepareTransaction { .. }
        | WalOp::PreparedLocks { .. }
        | WalOp::CommitPrepared { .. }
        | WalOp::RollbackPrepared { .. } => {}
        WalOp::Truncate { .. } => {}
        WalOp::SetCast(definition) => {
            storage.create_cast_from_image(definition)?;
        }
        WalOp::DropCast { source, target } => {
            let source = storage.bind_routine_result(source, 0)?;
            let target = storage.bind_routine_result(target, 0)?;
            let slot = storage.cast_slot(source, target, 0).ok_or_else(|| {
                sql_err!(sqlstate::UNDEFINED_OBJECT, "journal cast does not exist")
            })?;
            storage.drop_cast(source, target, 0);
            storage.commit_cast_drop(slot);
        }
        WalOp::SetOperator {
            created_at,
            definition,
        } => {
            storage.replay_set_operator(created_at, definition)?;
        }
        WalOp::DropOperator {
            schema,
            name,
            mut signature,
        } => {
            if let Some(left) = signature.left {
                signature.left = Some(storage.bind_routine_result(left, 0)?);
            }
            if let Some(right) = signature.right {
                signature.right = Some(storage.bind_routine_result(right, 0)?);
            }
            let slot = storage
                .operator_slot_exact(schema, name, signature, 0)
                .ok_or_else(|| {
                    sql_err!(
                        sqlstate::UNDEFINED_FUNCTION,
                        "journal operator does not exist"
                    )
                })?;
            storage.drop_operator(slot, 0);
            storage.commit_operator_drop(slot);
        }
        WalOp::SetCollation {
            slot,
            created_at,
            definition,
        } => storage.replay_collation(usize::from(slot), created_at, definition)?,
        WalOp::DropCollation { schema, name } => {
            storage.replay_drop_collation(schema, name);
        }
        WalOp::SetConversion {
            slot,
            created_at,
            definition,
        } => storage.replay_conversion(usize::from(slot), created_at, definition)?,
        WalOp::DropConversion { schema, name } => {
            storage.replay_drop_conversion(schema, name);
        }
        WalOp::SetTextSearch {
            slot,
            created_at,
            definition,
        } => {
            storage.replay_text_search_object(usize::from(slot), created_at, definition)?;
        }
        WalOp::DropTextSearch { kind, schema, name } => {
            storage.replay_drop_text_search_object(kind, schema, name);
        }
        WalOp::SetEventTrigger {
            slot,
            created_at,
            definition,
        } => storage.replay_event_trigger(usize::from(slot), created_at, definition)?,
        WalOp::DropEventTrigger { name } => storage.replay_drop_event_trigger(name),
        WalOp::SetOperatorFamily {
            created_at,
            definition,
        } => {
            storage.replay_set_operator_family(created_at, definition)?;
        }
        WalOp::DropOperatorFamily { schema, name } => {
            let slot = storage
                .operator_family_slot_exact(schema, name, 0)
                .ok_or_else(|| {
                    sql_err!(
                        sqlstate::UNDEFINED_OBJECT,
                        "journal operator family does not exist"
                    )
                })?;
            storage.drop_operator_family(slot, 0);
            storage.commit_operator_family_drop(slot);
        }
        WalOp::SetOperatorClass {
            created_at,
            definition,
        } => {
            storage.replay_set_operator_class(created_at, definition)?;
        }
        WalOp::DropOperatorClass { schema, name } => {
            let slot = storage
                .operator_class_slot_exact(schema, name, 0)
                .ok_or_else(|| {
                    sql_err!(
                        sqlstate::UNDEFINED_OBJECT,
                        "journal operator class does not exist"
                    )
                })?;
            storage.drop_operator_class(slot, 0);
            storage.commit_operator_class_drop(slot);
        }
        WalOp::CreateReplicationSlot {
            name,
            restart_lsn,
            behavior,
        } => {
            storage.create_replication_slot(
                crate::storage::ReplicationSlotName::parse(name)?,
                restart_lsn,
                behavior,
            )?;
        }
        WalOp::AlterReplicationSlot { name, behavior } => storage
            .alter_replication_slot(crate::storage::ReplicationSlotName::parse(name)?, behavior)?,
        WalOp::DropReplicationSlot { name } => storage.drop_replication_slot(name)?,
        WalOp::AdvanceReplicationSlot {
            name,
            confirmed_flush_lsn,
        } => {
            let advance = storage.prepare_replication_slot_advance(name, confirmed_flush_lsn)?;
            storage.apply_replication_slot_advance(advance);
        }
        WalOp::SetPolicy {
            schema,
            table,
            name,
            command,
            permissive,
            roles,
            role_count,
            using,
            with_check,
            dependencies,
        } => {
            let table_slot = match storage.resolve_relation(Some(schema), table, 0) {
                Some(crate::storage::ResolvedRelation::Table(slot)) => slot,
                _ => {
                    return Err(sql_err!(
                        sqlstate::UNDEFINED_TABLE,
                        "journal policy targets unknown table \"{}.{}\"",
                        schema,
                        table
                    ));
                }
            };
            let mut role_slots = [crate::storage::PUBLIC_ROLE; crate::storage::MAX_POLICY_ROLES];
            for (index, role) in roles[..role_count].iter().enumerate() {
                role_slots[index] = if role.as_str().eq_ignore_ascii_case("public") {
                    crate::storage::PUBLIC_ROLE
                } else {
                    storage.find_role_visible(role.as_str(), 0).ok_or_else(|| {
                        sql_err!(
                            sqlstate::UNDEFINED_OBJECT,
                            "journal policy references unknown role \"{}\"",
                            role.as_str()
                        )
                    })? as u16
                };
            }
            storage.replay_set_policy(crate::storage::PolicySpec {
                name: crate::storage::SqlName::parse(name)?,
                table: table_slot,
                command: crate::storage::PolicyCommandKind::from_code(command).ok_or_else(
                    || {
                        sql_err!(
                            sqlstate::DATA_EXCEPTION,
                            "journal policy has invalid command"
                        )
                    },
                )?,
                permissive,
                definition: crate::storage::PolicyDefinition {
                    roles: crate::storage::PolicyRoles::from_slice(&role_slots[..role_count])?,
                    using: using.map(crate::storage::policy_expression).transpose()?,
                    with_check: with_check
                        .map(crate::storage::policy_expression)
                        .transpose()?,
                    dependencies: dependencies.materialize()?,
                },
            })?;
        }
        WalOp::DropPolicy {
            schema,
            table,
            name,
        } => {
            if let Some(crate::storage::ResolvedRelation::Table(table_slot)) =
                storage.resolve_relation(Some(schema), table, 0)
            {
                storage.replay_drop_policy(table_slot, name);
            }
        }
        WalOp::CreateTrigger {
            name,
            target,
            table_schema,
            table,
            function_schema,
            function,
            or_replace,
            constraint,
            constraint_timing,
            referenced_schema,
            referenced_table,
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
            let target = match (
                target,
                storage.resolve_relation(Some(table_schema), table, 0),
            ) {
                (
                    crate::wal::TriggerTargetKind::Table,
                    Some(crate::storage::ResolvedRelation::Table(slot)),
                ) => crate::storage::TriggerTarget::Table(slot as u16),
                (
                    crate::wal::TriggerTargetKind::View,
                    Some(crate::storage::ResolvedRelation::View(slot)),
                ) => crate::storage::TriggerTarget::View(slot as u16),
                _ => {
                    return Err(sql_err!(
                        sqlstate::UNDEFINED_TABLE,
                        "journal trigger targets unknown relation \"{}.{}\"",
                        table_schema,
                        table
                    ));
                }
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
            let kind = if constraint {
                let referenced_table = match (referenced_schema, referenced_table) {
                    (Some(schema), Some(table)) => {
                        match storage.resolve_relation(Some(schema), table, 0) {
                            Some(crate::storage::ResolvedRelation::Table(slot)) => {
                                Some(slot as u16)
                            }
                            _ => {
                                return Err(sql_err!(
                                    sqlstate::UNDEFINED_TABLE,
                                    "journal constraint trigger references unknown table \"{}.{}\"",
                                    schema,
                                    table
                                ));
                            }
                        }
                    }
                    (None, None) => None,
                    _ => {
                        return Err(sql_err!(
                            sqlstate::INVALID_OBJECT_DEFINITION,
                            "journal constraint trigger has incomplete referenced table"
                        ));
                    }
                };
                crate::storage::TriggerKind::Constraint {
                    referenced_table,
                    timing: crate::storage::ConstraintTiming::from_code(constraint_timing)
                        .ok_or_else(|| {
                            sql_err!(
                                sqlstate::INVALID_OBJECT_DEFINITION,
                                "journal constraint trigger has invalid timing"
                            )
                        })?,
                }
            } else {
                crate::storage::TriggerKind::Ordinary
            };
            let spec = crate::storage::TriggerSpec {
                name: crate::storage::SqlName::parse(name)?,
                target,
                kind,
                function: function_slot,
                timing: crate::sql::ast::TriggerTiming::from_code(timing).ok_or_else(|| {
                    sql_err!(
                        sqlstate::INVALID_OBJECT_DEFINITION,
                        "journal trigger has invalid timing"
                    )
                })?,
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
                arguments: crate::storage::TriggerArguments::parse(&arguments[..argument_count])?,
            };
            if or_replace && let Some(existing) = storage.trigger_slot_on(target, name, 0) {
                storage.replace_trigger(existing, spec, 0)?;
                storage.commit_trigger_alter(existing, 0);
            } else {
                let slot = storage.create_trigger(spec, 0)?;
                storage.commit_trigger_create(slot);
            }
        }
        WalOp::DropTrigger {
            name,
            target,
            table_schema,
            table,
        } => {
            let target = match (
                target,
                storage.resolve_relation(Some(table_schema), table, 0),
            ) {
                (
                    crate::wal::TriggerTargetKind::Table,
                    Some(crate::storage::ResolvedRelation::Table(slot)),
                ) => crate::storage::TriggerTarget::Table(slot as u16),
                (
                    crate::wal::TriggerTargetKind::View,
                    Some(crate::storage::ResolvedRelation::View(slot)),
                ) => crate::storage::TriggerTarget::View(slot as u16),
                _ => {
                    return Err(sql_err!(
                        sqlstate::UNDEFINED_TABLE,
                        "journal trigger targets unknown relation"
                    ));
                }
            };
            if let Some(slot) = storage.trigger_slot_on(target, name, 0) {
                storage.drop_trigger(slot, 0);
                storage.commit_trigger_drop(slot);
            }
        }
        WalOp::AlterTrigger {
            name,
            target,
            table_schema,
            table,
            new_name,
            enabled,
        } => {
            let target = match (
                target,
                storage.resolve_relation(Some(table_schema), table, 0),
            ) {
                (
                    crate::wal::TriggerTargetKind::Table,
                    Some(crate::storage::ResolvedRelation::Table(slot)),
                ) => crate::storage::TriggerTarget::Table(slot as u16),
                (
                    crate::wal::TriggerTargetKind::View,
                    Some(crate::storage::ResolvedRelation::View(slot)),
                ) => crate::storage::TriggerTarget::View(slot as u16),
                _ => {
                    return Err(sql_err!(
                        sqlstate::UNDEFINED_TABLE,
                        "journal trigger targets unknown relation"
                    ));
                }
            };
            let enabled = crate::storage::TriggerEnabled::from_code(enabled).ok_or_else(|| {
                sql_err!(
                    sqlstate::INTERNAL_ERROR,
                    "journal trigger has invalid enabled mode"
                )
            })?;
            if let Some(slot) = storage.trigger_slot_on(target, name, 0) {
                storage.alter_trigger(
                    slot,
                    crate::storage::SqlName::parse(new_name)?,
                    enabled,
                    0,
                )?;
                storage.commit_trigger_alter(slot, 0);
            } else if let crate::storage::TriggerTarget::Table(table) = target
                && name == new_name
                && let Some(slot) = storage.trigger_slot_inherited_by(usize::from(table), name, 0)
            {
                if let Some((state, _)) =
                    storage.stage_partition_trigger_enabled(slot, usize::from(table), enabled, 0)?
                {
                    storage.commit_partition_trigger_state(state, 0);
                }
            } else {
                return Err(sql_err!(
                    sqlstate::UNDEFINED_OBJECT,
                    "journal alters unknown trigger \"{}\"",
                    name
                ));
            }
        }
        WalOp::CreateTable(def) => {
            // A journal written before its schema existed cannot occur going
            // forward (CreateSchema precedes in LSN order), but a pre-schema
            // journal names only public, which always exists.
            if storage
                .table_access_method_name(def.access_method, 0)
                .is_none()
            {
                return Err(sql_err!(
                    sqlstate::INTERNAL_ERROR,
                    "journal table references an unknown access method"
                ));
            }
            if !storage.complete_replay_table_rewrite(def)? {
                storage.create_table(def)?;
            }
        }
        WalOp::BeginTableRewrite {
            previous_schema,
            previous_name,
            preserve_rows,
            column_mapping,
        } => {
            storage.begin_replay_table_rewrite(
                previous_schema,
                previous_name,
                preserve_rows,
                column_mapping,
            )?;
        }
        WalOp::SequenceSet {
            schema,
            table,
            column,
            last,
        } => {
            let Some(index) = storage.wal_table_slot(schema, table) else {
                return Err(SqlError {
                    sqlstate: SqlState::known(sqlstate::UNDEFINED_TABLE),
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
            let Some(index) = storage.wal_table_slot(schema, table) else {
                return Err(sql_err!(
                    sqlstate::UNDEFINED_TABLE,
                    "journal analyzes unknown table \"{}\"",
                    table
                ));
            };
            storage.replay_table_statistics(index, statistics.materialize()?);
        }
        WalOp::SetExtendedStatistics {
            created_at,
            schema,
            name,
            table_schema,
            table,
            target,
            kinds,
            expression_only,
            keys,
            key_count,
        } => {
            let Some(table_slot) = storage.find_table(table_schema, table) else {
                return Err(sql_err!(
                    sqlstate::UNDEFINED_TABLE,
                    "journal statistics object references unknown table \"{}\"",
                    table
                ));
            };
            let kinds = if expression_only {
                crate::sql::ast::StatisticsKinds::EXPRESSION
            } else {
                crate::sql::ast::StatisticsKinds::from_code(kinds).ok_or_else(|| {
                    sql_err!(
                        sqlstate::DATA_EXCEPTION,
                        "journal has invalid statistics kinds"
                    )
                })?
            };
            let mut stored_keys =
                [crate::storage::ExtendedStatisticsKey::Column(crate::storage::SqlName::EMPTY);
                    crate::storage::MAX_EXTENDED_STATISTICS_KEYS];
            for (position, key) in keys[..key_count].iter().copied().enumerate() {
                stored_keys[position] = match key {
                    crate::wal::WalExtendedStatisticsKey::Column(column) => {
                        let Some(column) = storage.table_def(table_slot, 0).column_index(column)
                        else {
                            return Err(sql_err!(
                                sqlstate::DATA_EXCEPTION,
                                "journal statistics key references unknown column"
                            ));
                        };
                        crate::storage::ExtendedStatisticsKey::Column(
                            storage.table_def(table_slot, 0).columns()[column].name,
                        )
                    }
                    crate::wal::WalExtendedStatisticsKey::Expression(expression) => {
                        crate::storage::ExtendedStatisticsKey::Expression(
                            crate::storage::extended_statistics_expression(expression)?,
                        )
                    }
                };
            }
            storage.replay_extended_statistics(crate::storage::ExtendedStatisticsSpec {
                created_at,
                schema: crate::storage::SqlName::parse(schema)?,
                name: crate::storage::SqlName::parse(name)?,
                table: table_slot as u16,
                target,
                keys: stored_keys,
                n_keys: key_count as u8,
                kinds,
                expression_only,
            })?;
        }
        WalOp::DropExtendedStatistics { schema, name } => {
            storage.replay_drop_extended_statistics(schema, name)?;
        }
        WalOp::AnalyzeExtendedStatistics {
            schema,
            name,
            statistics,
        } => {
            let Some(slot) = storage.extended_statistics_slot(schema, name, 0) else {
                return Err(sql_err!(
                    sqlstate::UNDEFINED_OBJECT,
                    "journal analyzes unknown statistics object \"{}\"",
                    name
                ));
            };
            storage.install_extended_statistics_data(slot, statistics.materialize()?);
        }
        WalOp::DropTable { schema, name } => {
            let Some(index) = storage.find_table(schema, name) else {
                return Err(SqlError {
                    sqlstate: SqlState::known(sqlstate::UNDEFINED_TABLE),
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
            let Some(index) = storage.wal_table_slot(schema, table) else {
                return Err(SqlError {
                    sqlstate: SqlState::known(sqlstate::UNDEFINED_TABLE),
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
                    sqlstate: SqlState::known(sqlstate::PROGRAM_LIMIT_EXCEEDED),
                    message: stack_format!(192, "journal replay overflows {}", e.what),
                })?;
        }
        WalOp::Delete {
            schema,
            table,
            rowid,
            ..
        } => {
            let Some(index) = storage.wal_table_slot(schema, table) else {
                return Err(SqlError {
                    sqlstate: SqlState::known(sqlstate::UNDEFINED_TABLE),
                    message: stack_format!(192, "journal deletes from unknown table \"{}\"", table),
                });
            };
            storage.remove_committed(index, rowid, lsn);
        }
        WalOp::CreateView {
            schema,
            name,
            columns,
            sql,
            path,
            security_invoker,
            security_barrier,
            check_option,
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
                crate::storage::ViewDefinition {
                    columns,
                    query: crate::storage::StoredQueryDefinition {
                        sql: buffer,
                        creation_path,
                        dependencies,
                    },
                    options: crate::storage::ViewOptions {
                        security: if security_invoker {
                            crate::storage::ViewSecurity::Invoker
                        } else {
                            crate::storage::ViewSecurity::Definer
                        },
                        security_barrier: crate::storage::ViewSecurityBarrier::from_code(
                            security_barrier,
                        )
                        .ok_or_else(|| {
                            sql_err!(
                                sqlstate::DATA_EXCEPTION,
                                "journal has invalid view security barrier"
                            )
                        })?,
                        check_option: match check_option {
                            0 => None,
                            code => {
                                Some(crate::storage::ViewCheckOption::from_code(code).ok_or_else(
                                    || {
                                        sql_err!(
                                            sqlstate::DATA_EXCEPTION,
                                            "journal has invalid view check option"
                                        )
                                    },
                                )?)
                            }
                        },
                    },
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
        WalOp::SetViewOptions {
            schema,
            name,
            security_invoker,
            security_barrier,
            check_option,
        } => {
            let slot = storage
                .resolve_access_object(crate::storage::AccessClass::View, schema, name, 0)
                .map(|object| object.slot as usize)
                .ok_or_else(|| {
                    sql_err!(
                        sqlstate::UNDEFINED_TABLE,
                        "journal changes unknown view \"{}\"",
                        name
                    )
                })?;
            let check_option = match check_option {
                0 => None,
                code => Some(crate::storage::ViewCheckOption::from_code(code).ok_or_else(
                    || {
                        sql_err!(
                            sqlstate::DATA_EXCEPTION,
                            "journal has invalid view check option"
                        )
                    },
                )?),
            };
            storage.stage_view_options(
                slot,
                crate::storage::ViewOptions {
                    security: if security_invoker {
                        crate::storage::ViewSecurity::Invoker
                    } else {
                        crate::storage::ViewSecurity::Definer
                    },
                    security_barrier: crate::storage::ViewSecurityBarrier::from_code(
                        security_barrier,
                    )
                    .ok_or_else(|| {
                        sql_err!(
                            sqlstate::DATA_EXCEPTION,
                            "journal has invalid view security barrier"
                        )
                    })?,
                    check_option,
                },
                0,
            )?;
            storage.commit_view_options(slot, 0);
        }
        WalOp::SetViewColumns {
            schema,
            name,
            columns,
        } => {
            let slot = storage
                .resolve_access_object(crate::storage::AccessClass::View, schema, name, 0)
                .map(|object| object.slot as usize)
                .ok_or_else(|| {
                    sql_err!(
                        sqlstate::UNDEFINED_TABLE,
                        "journal changes unknown view \"{}\"",
                        name
                    )
                })?;
            storage.stage_view_columns(slot, columns, 0)?;
            storage.commit_view_columns(slot, 0);
        }
        WalOp::RenameView {
            schema,
            name,
            new_name,
        } => {
            let slot = storage
                .resolve_access_object(crate::storage::AccessClass::View, schema, name, 0)
                .map(|object| object.slot as usize)
                .ok_or_else(|| {
                    sql_err!(
                        sqlstate::UNDEFINED_TABLE,
                        "journal changes unknown view \"{}\"",
                        name
                    )
                })?;
            storage.stage_view_rename(slot, crate::storage::SqlName::parse(new_name)?, 0)?;
            storage.commit_view_rename(slot, 0);
        }
        WalOp::SetRule {
            slot,
            created_at,
            target,
            table_schema,
            table,
            name,
            event,
            mode,
            enabled,
            source,
            condition,
            actions,
            action_count,
            returning_action,
            path,
            dependencies,
        } => {
            use core::fmt::Write as _;
            let target = match target {
                crate::wal::TriggerTargetKind::Table => storage
                    .find_table(table_schema, table)
                    .and_then(|slot| u16::try_from(slot).ok())
                    .map(crate::storage::RuleTarget::Table),
                crate::wal::TriggerTargetKind::View => storage
                    .views_visible_to(0)
                    .find(|(_, view)| {
                        view.schema.as_str() == table_schema && view.name.as_str() == table
                    })
                    .and_then(|(slot, _)| u16::try_from(slot).ok())
                    .map(crate::storage::RuleTarget::View),
            }
            .ok_or_else(|| {
                sql_err!(
                    sqlstate::UNDEFINED_TABLE,
                    "journal rewrite-rule relation does not exist"
                )
            })?;
            let mut stored_source =
                crate::util::StackStr::<{ crate::storage::RULE_SQL_MAX }>::new();
            let _ = stored_source.write_str(source);
            let mut creation_path = crate::util::StackStr::<128>::new();
            let _ = creation_path.write_str(path);
            if stored_source.is_truncated() || creation_path.is_truncated() {
                return Err(sql_err!(
                    sqlstate::PROGRAM_LIMIT_EXCEEDED,
                    "journal rewrite-rule definition exceeds configured bounds"
                ));
            }
            let dependencies =
                storage.rebind_stored_query_dependencies(dependencies.materialize()?, 0)?;
            storage.replay_rule(
                usize::from(slot),
                created_at,
                crate::storage::RuleDefinition {
                    name: crate::storage::SqlName::parse(name)?,
                    target,
                    event,
                    mode,
                    enabled,
                    source: stored_source,
                    condition,
                    actions,
                    action_count,
                    returning_action,
                    creation_path,
                    dependencies,
                },
            )?;
        }
        WalOp::DropRule {
            target,
            table_schema,
            table,
            name,
        } => {
            let target = match target {
                crate::wal::TriggerTargetKind::Table => storage
                    .find_table(table_schema, table)
                    .and_then(|slot| u16::try_from(slot).ok())
                    .map(crate::storage::RuleTarget::Table),
                crate::wal::TriggerTargetKind::View => storage
                    .views_visible_to(0)
                    .find(|(_, view)| {
                        view.schema.as_str() == table_schema && view.name.as_str() == table
                    })
                    .and_then(|(slot, _)| u16::try_from(slot).ok())
                    .map(crate::storage::RuleTarget::View),
            };
            if let Some(target) = target {
                storage.replay_drop_rule(target, name);
            }
        }
        WalOp::CreatePublication {
            name,
            owner,
            all_tables,
            tables,
            table_column_masks,
            table_filter_sql,
            table_count,
            schemas,
            schema_count,
            publish_insert,
            publish_update,
            publish_delete,
            publish_truncate,
            publish_via_partition_root,
            publish_generated_columns,
        } => {
            let slot = storage.create_publication(
                crate::storage::PublicationSpec {
                    name: crate::storage::SqlName::parse(name)?,
                    all_tables,
                    tables: &tables[..table_count],
                    table_column_masks: &table_column_masks[..table_count],
                    table_filter_sql: &table_filter_sql[..table_count],
                    schemas: &schemas[..schema_count],
                    publish_insert,
                    publish_update,
                    publish_delete,
                    publish_truncate,
                    publish_via_partition_root,
                    publish_generated_columns,
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
            table_column_masks,
            table_filter_sql,
            table_count,
            schemas,
            schema_count,
            publish_insert,
            publish_update,
            publish_delete,
            publish_truncate,
            publish_via_partition_root,
            publish_generated_columns,
        } => {
            let definition = crate::storage::PublicationDefinition {
                all_tables,
                tables,
                table_column_masks,
                table_filters: crate::storage::PublicationFilters::from_sql(
                    &table_filter_sql[..table_count],
                )?,
                table_count,
                schemas,
                schema_count,
                publish_insert,
                publish_update,
                publish_delete,
                publish_truncate,
                publish_via_partition_root,
                publish_generated_columns,
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
            slot: publisher_slot,
            behavior,
            bootstrap,
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
                    slot: publisher_slot,
                    behavior,
                    bootstrap,
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
            created_at,
            definition_generation,
            confirmed_lsn,
        } => {
            let (slot, _) = storage.subscription(name, 0).ok_or_else(|| {
                sql_err!(
                    sqlstate::UNDEFINED_OBJECT,
                    "journal subscription advance for unknown subscription \"{}\"",
                    name
                )
            })?;
            let stream = storage
                .subscription_stream(slot, 0)
                .filter(|stream| {
                    stream.created_at() == created_at
                        && stream.definition_generation() == definition_generation
                })
                .ok_or_else(|| {
                    sql_err!(
                        sqlstate::OBJECT_NOT_IN_PREREQUISITE_STATE,
                        "journal subscription advance targets a replaced stream definition"
                    )
                })?;
            if let Some(advance) = storage.subscription_advance(stream, confirmed_lsn, 0)? {
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
        WalOp::SetSubscriptionBootstrap { name, bootstrap } => {
            let (slot, _) = storage.subscription(name, 0).ok_or_else(|| {
                sql_err!(
                    sqlstate::UNDEFINED_OBJECT,
                    "journal bootstrap change for unknown subscription \"{}\"",
                    name
                )
            })?;
            if matches!(
                storage.set_subscription_bootstrap(slot, bootstrap, 0)?,
                crate::storage::SubscriptionBootstrapChange::Changed { .. }
            ) {
                storage.commit_subscription_bootstrap(slot, 0);
            }
        }
        WalOp::ResetSubscriptionRelations {
            name,
            created_at,
            definition_generation,
        } => {
            let (slot, _) = storage.subscription(name, 0).ok_or_else(|| {
                sql_err!(
                    sqlstate::UNDEFINED_OBJECT,
                    "journal relation reset for unknown subscription \"{}\"",
                    name
                )
            })?;
            let stream = storage
                .subscription_stream(slot, 0)
                .filter(|stream| {
                    stream.created_at() == created_at
                        && stream.definition_generation() == definition_generation
                })
                .ok_or_else(|| {
                    sql_err!(
                        sqlstate::OBJECT_NOT_IN_PREREQUISITE_STATE,
                        "journal relation reset targets a replaced subscription stream"
                    )
                })?;
            storage.begin_subscription_relation_refresh(stream, 0)?;
            storage.commit_subscription_relation_refresh(0);
        }
        WalOp::AddSubscriptionRelation {
            name,
            created_at,
            definition_generation,
            schema,
            table,
        } => {
            let (slot, _) = storage.subscription(name, 0).ok_or_else(|| {
                sql_err!(
                    sqlstate::UNDEFINED_OBJECT,
                    "journal relation registration for unknown subscription \"{}\"",
                    name
                )
            })?;
            let stream = storage
                .subscription_stream(slot, 0)
                .filter(|stream| {
                    stream.created_at() == created_at
                        && stream.definition_generation() == definition_generation
                })
                .ok_or_else(|| {
                    sql_err!(
                        sqlstate::OBJECT_NOT_IN_PREREQUISITE_STATE,
                        "journal relation registration targets a replaced subscription stream"
                    )
                })?;
            storage.stage_subscription_relation(stream, schema, table, 0)?;
            storage.commit_subscription_relation_refresh(0);
        }
        WalOp::CompleteSubscriptionCleanup { name, created_at } => {
            let slot = storage
                .subscriptions_with_slots_durable()
                .find(|(_, subscription)| {
                    subscription.name.as_str() == name && subscription.created_at == created_at
                })
                .map(|(slot, _)| slot)
                .ok_or_else(|| {
                    sql_err!(
                        sqlstate::UNDEFINED_OBJECT,
                        "journal cleanup completion targets an unknown subscription"
                    )
                })?;
            storage.complete_subscription_cleanup(slot, created_at)?;
        }
        WalOp::FailSubscription {
            name,
            created_at,
            definition_generation,
            sqlstate: failure_code,
            message,
        } => {
            let (slot, _) = storage.subscription(name, 0).ok_or_else(|| {
                sql_err!(
                    sqlstate::UNDEFINED_OBJECT,
                    "journal failure targets unknown subscription \"{}\"",
                    name
                )
            })?;
            let stream = storage
                .subscription_stream(slot, 0)
                .filter(|stream| {
                    stream.created_at() == created_at
                        && stream.definition_generation() == definition_generation
                })
                .ok_or_else(|| {
                    sql_err!(
                        sqlstate::OBJECT_NOT_IN_PREREQUISITE_STATE,
                        "journal failure targets a replaced subscription stream"
                    )
                })?;
            storage.fail_subscription(
                stream,
                crate::storage::SubscriptionFailure {
                    sqlstate: SqlState::parse(failure_code).ok_or_else(|| {
                        sql_err!(sqlstate::DATA_EXCEPTION, "journal SQLSTATE is invalid")
                    })?,
                    message: crate::util::StackStr::from_str(message),
                },
            )?;
        }
        WalOp::AlterSubscription {
            name,
            connection,
            publications,
            publication_count,
            slot: publisher_slot,
            behavior,
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
            let definition = crate::storage::SubscriptionDefinition::from_parts(
                connection,
                &publications[..publication_count],
                publisher_slot,
                behavior,
            )?;
            if storage
                .set_subscription_definition(slot, definition, 0)?
                .changed
            {
                storage.commit_subscription_definition(slot, 0);
            }
        }
        WalOp::SetSubscriptionOwner { name, owner } => {
            let (slot, _) = storage.subscription(name, 0).ok_or_else(|| {
                sql_err!(
                    sqlstate::UNDEFINED_OBJECT,
                    "journal owner change for unknown subscription \"{}\"",
                    name
                )
            })?;
            storage.restore_subscription_owner(slot, owner);
        }
        WalOp::RenameSubscription { name, new_name } => {
            let (slot, _) = storage.subscription(name, 0).ok_or_else(|| {
                sql_err!(
                    sqlstate::UNDEFINED_OBJECT,
                    "journal rename for unknown subscription \"{}\"",
                    name
                )
            })?;
            storage.rename_subscription(slot, crate::storage::SqlName::parse(new_name)?, 0)?;
            storage.commit_subscription_rename(slot, 0);
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
            let backing_table = storage.find_visible(schema, name, 0).ok_or_else(|| {
                sql_err!(
                    sqlstate::UNDEFINED_TABLE,
                    "journal materialized view backing relation \"{}.{}\" does not exist",
                    schema,
                    name
                )
            })?;
            let slot = storage.create_matview(
                backing_table,
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
                storage.stage_sequence_alter(
                    slot,
                    crate::storage::SequenceAlteration {
                        schema: crate::storage::SqlName::parse(schema)?,
                        name: crate::storage::SqlName::parse(name)?,
                        spec,
                        owner,
                        generator_for,
                        restart: None,
                    },
                    0,
                )?;
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
                base_user_type: def.base_user_type,
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
        WalOp::CreateRoutine {
            definition,
            dependencies,
        } => storage.replay_create_routine(definition, dependencies.materialize()?)?,
        WalOp::DropRoutine {
            schema,
            name,
            argument_signature,
        } => {
            let (arguments, count) = decode_wal_routine_signature(argument_signature)?;
            if let Some(slot) =
                storage.routine_slot_by_declared_signature(schema, name, &arguments[..count], 0)
            {
                storage.drop_routine(slot, 0);
                storage.commit_routine_drop(slot);
            }
        }
        WalOp::AlterRoutineIdentity {
            schema,
            name,
            argument_signature,
            new_schema,
            new_name,
        } => {
            let (arguments, count) = decode_wal_routine_signature(argument_signature)?;
            let slot = storage
                .routine_slot_by_declared_signature(schema, name, &arguments[..count], 0)
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
        WalOp::CreateComposite {
            slot,
            definition: def,
        } => {
            let spec = crate::storage::CompositeSpec {
                fields: def.fields,
                n_fields: def.n_fields,
            };
            let slot = slot as usize;
            if storage.composite(slot).visible_to(0) {
                let mut definition = storage.composite_for(slot, 0);
                definition.schema = def.schema;
                definition.name = def.name;
                definition.fields = spec.fields;
                definition.n_fields = spec.n_fields;
                storage.stage_composite_alter(slot, definition, 0)?;
                storage.commit_composite_alter(slot, 0);
            } else {
                storage.create_composite_at(slot, def.schema, def.name, spec, 0)?;
            }
        }
        WalOp::DropComposite { schema, name } => {
            if let Some(slot) = storage.drop_composite(schema, name, 0)? {
                storage.commit_composite_drop(slot);
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
        WalOp::AlterEnumIdentity {
            schema,
            name,
            new_schema,
            new_name,
        } => {
            let slot = storage.enum_slot(schema, name, 0).ok_or_else(|| {
                sql_err!(
                    eval::sqlstate::UNDEFINED_OBJECT,
                    "enum type \"{}\" for WAL identity change does not exist",
                    name
                )
            })?;
            let mut definition = storage.enum_for(slot, 0);
            definition.schema = crate::storage::SqlName::parse(new_schema)?;
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
            created_at,
            schema,
            name,
            table,
            columns,
            include_columns,
            collations,
            explicit_collations,
            operator_classes,
            resolved_operator_classes,
            descending,
            nulls_first,
            n_cols,
            n_include_cols,
            nulls_not_distinct,
            predicate,
            expressions,
            unique,
            definition,
        } => {
            let mut stored_expressions = [None; crate::storage::MAX_INDEX_COLS];
            for (index, expression) in expressions.into_iter().enumerate() {
                stored_expressions[index] = expression
                    .map(crate::storage::index_expression_stackstr)
                    .transpose()?;
            }
            let slot = storage.create_index(
                crate::storage::IndexDef {
                    database: storage.current_database_oid(),
                    created_at,
                    schema: crate::storage::SqlName::parse(schema)?,
                    name: crate::storage::SqlName::parse(name)?,
                    pending_name: None,
                    table: crate::storage::SqlName::parse(table)?,
                    ownership: crate::storage::Ownership::BOOTSTRAP,
                    columns,
                    expressions: stored_expressions,
                    include_columns,
                    collations,
                    explicit_collations,
                    operator_classes,
                    resolved_operator_classes,
                    descending,
                    nulls_first,
                    n_cols,
                    n_include_cols,
                    nulls_not_distinct,
                    predicate: predicate
                        .map(crate::storage::index_predicate_stackstr)
                        .transpose()?,
                    unique,
                    mutable: definition,
                    pending_definition: None,
                    ddl_state: crate::storage::CatalogDdlState::Present,
                },
                0,
            )?;
            storage.commit_index_create(slot);
        }
        WalOp::AlterIndexDefinition {
            schema,
            name,
            definition,
        } => {
            let slot = storage.index_slot(schema, name, 0).ok_or_else(|| {
                sql_err!(
                    sqlstate::UNDEFINED_OBJECT,
                    "index \"{}\" for WAL definition change does not exist",
                    name
                )
            })?;
            storage.alter_index_definition(slot, definition, 0)?;
            storage.commit_index_definition(slot, 0);
        }
        WalOp::CreateTablespace {
            created_at,
            name,
            location,
            options,
            owner,
        } => {
            let location = crate::util::StackStr::from_str(location);
            if location.is_truncated() {
                return Err(sql_err!(
                    sqlstate::PROGRAM_LIMIT_EXCEEDED,
                    "tablespace location is too long"
                ));
            }
            let slot = storage.create_tablespace(
                created_at,
                crate::storage::SqlName::parse(name)?,
                location,
                options,
                owner,
                0,
            )?;
            storage.commit_tablespace_create(slot);
        }
        WalOp::AlterTablespace {
            name,
            new_name,
            options,
            owner,
        } => {
            let slot = storage.tablespace_slot(name, 0).ok_or_else(|| {
                sql_err!(
                    sqlstate::UNDEFINED_OBJECT,
                    "tablespace \"{}\" does not exist",
                    name
                )
            })?;
            storage.alter_tablespace_definition(
                slot,
                crate::storage::SqlName::parse(new_name)?,
                options,
                0,
            )?;
            storage.set_object_owner(
                crate::storage::AccessObject {
                    class: crate::storage::AccessClass::Tablespace,
                    slot: slot as u16,
                },
                usize::from(owner),
                0,
            );
            storage.commit_tablespace_alter(slot, 0);
        }
        WalOp::DropTablespace { name } => {
            let slot = storage.tablespace_slot(name, 0).ok_or_else(|| {
                sql_err!(
                    sqlstate::UNDEFINED_OBJECT,
                    "tablespace \"{}\" does not exist",
                    name
                )
            })?;
            storage.drop_tablespace(slot, 0)?;
            storage.commit_tablespace_drop(slot);
        }
        WalOp::CreateAccessMethod {
            created_at,
            name,
            handler,
        } => {
            let slot = storage.create_access_method(
                created_at,
                crate::storage::AccessMethodDefinition {
                    name: crate::storage::SqlName::parse(name)?,
                    handler,
                },
                0,
            )?;
            storage.commit_access_method_create(slot);
        }
        WalOp::DropAccessMethod { name } => {
            let slot = storage.access_method_slot(name, 0).ok_or_else(|| {
                sql_err!(
                    sqlstate::UNDEFINED_OBJECT,
                    "journal access method \"{}\" does not exist",
                    name
                )
            })?;
            storage.drop_access_method(slot, 0);
            storage.commit_access_method_drop(slot);
        }
        WalOp::CreateDatabase {
            oid,
            template_oid,
            definition,
            owner,
        } => {
            let oid = crate::storage::DatabaseOid::parse(oid)
                .ok_or_else(|| sql_err!(sqlstate::INTERNAL_ERROR, "corrupt WAL database OID"))?;
            let template_oid =
                crate::storage::DatabaseOid::parse(template_oid).ok_or_else(|| {
                    sql_err!(
                        sqlstate::INTERNAL_ERROR,
                        "corrupt WAL template database OID"
                    )
                })?;
            if storage.database_slot_by_oid(template_oid, 0).is_none() {
                return Err(sql_err!(
                    sqlstate::INTERNAL_ERROR,
                    "journal template database does not exist"
                ));
            }
            let slot = storage.create_database(Some(oid), template_oid, definition, owner, 0)?;
            storage.commit_database_create(slot);
        }
        WalOp::AlterDatabase {
            oid,
            definition,
            owner,
        } => {
            let oid = crate::storage::DatabaseOid::parse(oid)
                .ok_or_else(|| sql_err!(sqlstate::INTERNAL_ERROR, "corrupt WAL database OID"))?;
            let slot = storage.database_slot_by_oid(oid, 0).ok_or_else(|| {
                sql_err!(sqlstate::INTERNAL_ERROR, "journal database does not exist")
            })?;
            storage.alter_database_definition(slot, definition, 0)?;
            storage.set_object_owner(
                crate::storage::AccessObject {
                    class: crate::storage::AccessClass::Database,
                    slot: slot as u16,
                },
                owner as usize,
                0,
            );
            storage.commit_database_alter(slot, 0);
        }
        WalOp::DropDatabase { oid } => {
            let oid = crate::storage::DatabaseOid::parse(oid)
                .ok_or_else(|| sql_err!(sqlstate::INTERNAL_ERROR, "corrupt WAL database OID"))?;
            let slot = storage.database_slot_by_oid(oid, 0).ok_or_else(|| {
                sql_err!(sqlstate::INTERNAL_ERROR, "journal database does not exist")
            })?;
            storage.drop_database(slot, 0);
            storage.commit_database_drop(slot);
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
        WalOp::RenameSchema { name, new_name } => {
            let slot = storage.find_schema(name).ok_or_else(|| {
                sql_err!(sqlstate::INTERNAL_ERROR, "journal schema does not exist")
            })?;
            storage.rename_schema(slot, crate::storage::SqlName::parse(new_name)?)?;
        }
        WalOp::UpsertExtension {
            name,
            schema,
            version,
            relocatable,
            owner,
            created_at,
        } => {
            let namespace = storage.find_schema(schema).ok_or_else(|| {
                sql_err!(
                    sqlstate::INVALID_SCHEMA_NAME,
                    "journal extension references unknown schema \"{}\"",
                    schema
                )
            })?;
            let owner = storage.find_role(owner).ok_or_else(|| {
                sql_err!(
                    sqlstate::UNDEFINED_OBJECT,
                    "journal extension references unknown owner"
                )
            })?;
            storage.install_extension(
                crate::storage::SqlName::parse(name)?,
                namespace,
                relocatable,
                crate::storage::ExtensionVersion::parse(version)?,
                owner,
                created_at,
            )?;
        }
        WalOp::DropExtension { name } => {
            if let Some(slot) = storage.extension_slot(name, 0) {
                storage.drop_extension_in(slot, 0);
                storage.commit_extension_drop(slot);
            }
        }
        WalOp::SetExtensionDependency {
            extension,
            class,
            object_oid,
            schema,
            name,
            kind,
            exists,
        } => {
            let extension = storage.extension_slot(extension, 0).ok_or_else(|| {
                sql_err!(
                    sqlstate::UNDEFINED_OBJECT,
                    "journal extension does not exist"
                )
            })?;
            let object = if class == crate::storage::AccessClass::Routine {
                storage.routine_slot_by_oid(object_oid, 0).map(|slot| {
                    crate::storage::AccessObject {
                        class,
                        slot: slot as u16,
                    }
                })
            } else {
                storage.resolve_access_object(class, schema, name, 0)
            }
            .ok_or_else(|| {
                sql_err!(
                    sqlstate::UNDEFINED_OBJECT,
                    "journal extension dependency target {}.{} (class {}) does not exist",
                    schema,
                    name,
                    class as u8
                )
            })?;
            let (slot, _) =
                storage.change_extension_dependency(extension, object, kind, exists, 0)?;
            storage.commit_extension_dependency(slot, 0);
        }
        WalOp::SetExtensionConfig {
            extension,
            ordinal,
            relation_kind,
            schema,
            name,
            condition,
            exists,
        } => {
            let extension = storage.extension_slot(extension, 0).ok_or_else(|| {
                sql_err!(
                    sqlstate::UNDEFINED_OBJECT,
                    "journal extension does not exist"
                )
            })?;
            let object = match relation_kind {
                crate::storage::ExtensionConfigRelationKind::Table => storage
                    .resolve_access_object(crate::storage::AccessClass::Table, schema, name, 0),
                crate::storage::ExtensionConfigRelationKind::Sequence => storage
                    .resolve_access_object(crate::storage::AccessClass::Sequence, schema, name, 0),
            }
            .ok_or_else(|| {
                sql_err!(
                    sqlstate::UNDEFINED_TABLE,
                    "journal extension configuration relation \"{}.{}\" does not exist",
                    schema,
                    name
                )
            })?;
            let relation = crate::storage::ExtensionConfigRelation::from_access_object(object)
                .expect("configuration relation kind was resolved through its typed class");
            let condition = crate::storage::extension_config_condition(condition)?;
            let (slot, _) =
                storage.replay_extension_config(extension, relation, condition, exists, ordinal)?;
            storage.commit_extension_config(slot, 0);
        }
        WalOp::SetTableSchema {
            schema,
            name,
            new_schema,
        } => {
            let Some(index) = storage.find_table(schema, name) else {
                return Err(SqlError {
                    sqlstate: SqlState::known(sqlstate::UNDEFINED_TABLE),
                    message: stack_format!(192, "journal moves unknown table \"{}\"", name),
                });
            };
            storage.move_table_schema(index, crate::storage::SqlName::parse(new_schema)?);
        }
        WalOp::SetSequenceSchema {
            schema,
            name,
            new_schema,
        } => {
            let slot = storage.sequence_slot(schema, name, 0).ok_or_else(|| {
                sql_err!(
                    sqlstate::UNDEFINED_TABLE,
                    "journal moves unknown sequence \"{}\"",
                    name
                )
            })?;
            let current = storage.sequence_for(slot, 0);
            let spec = crate::storage::SeqSpec {
                data_type: current.data_type,
                increment: current.increment,
                min_value: current.min_value,
                max_value: current.max_value,
                start_value: current.start_value,
                cache: current.cache,
                cycle: current.cycle,
            };
            storage.stage_sequence_alter(
                slot,
                crate::storage::SequenceAlteration {
                    schema: crate::storage::SqlName::parse(new_schema)?,
                    name: current.name,
                    spec,
                    owner: current.owner,
                    generator_for: current.generator_for,
                    restart: None,
                },
                0,
            )?;
            storage.commit_sequence_alter(slot, 0);
        }
        WalOp::RenameSequence {
            schema,
            name,
            new_name,
        } => {
            let slot = storage.sequence_slot(schema, name, 0).ok_or_else(|| {
                sql_err!(
                    sqlstate::UNDEFINED_TABLE,
                    "journal renames unknown sequence \"{}\"",
                    name
                )
            })?;
            let current = storage.sequence_for(slot, 0);
            let spec = crate::storage::SeqSpec {
                data_type: current.data_type,
                increment: current.increment,
                min_value: current.min_value,
                max_value: current.max_value,
                start_value: current.start_value,
                cache: current.cache,
                cycle: current.cycle,
            };
            storage.stage_sequence_alter(
                slot,
                crate::storage::SequenceAlteration {
                    schema: current.schema,
                    name: crate::storage::SqlName::parse(new_name)?,
                    spec,
                    owner: current.owner,
                    generator_for: current.generator_for,
                    restart: None,
                },
                0,
            )?;
            storage.commit_sequence_alter(slot, 0);
        }
        WalOp::SetViewSchema {
            schema,
            name,
            new_schema,
        } => {
            let slot = storage
                .resolve_access_object(crate::storage::AccessClass::View, schema, name, 0)
                .map(|object| object.slot as usize)
                .ok_or_else(|| {
                    sql_err!(
                        sqlstate::UNDEFINED_TABLE,
                        "journal moves unknown view \"{}\"",
                        name
                    )
                })?;
            storage.stage_view_schema(slot, crate::storage::SqlName::parse(new_schema)?, 0)?;
            storage.commit_view_schema(slot, 0);
        }
        WalOp::DropTableFk {
            schema,
            table,
            fk_name,
        } => {
            let Some(index) = storage.find_table(schema, table) else {
                return Err(SqlError {
                    sqlstate: SqlState::known(sqlstate::UNDEFINED_TABLE),
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
        WalOp::RenameRole { name, new_name } => storage.replay_rename_role(name, new_name)?,
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
        WalOp::SetRoleSetting {
            role,
            database,
            name,
            value,
        } => {
            let database = database
                .map(|oid| {
                    let oid = crate::storage::DatabaseOid::parse(oid).ok_or_else(|| {
                        sql_err!(
                            sqlstate::INTERNAL_ERROR,
                            "corrupt WAL role setting database"
                        )
                    })?;
                    storage.database_slot_by_oid(oid, 0).ok_or_else(|| {
                        sql_err!(
                            sqlstate::INTERNAL_ERROR,
                            "journal configures unknown database"
                        )
                    })?;
                    Ok(oid)
                })
                .transpose()?;
            let scope = match (role, database) {
                (Some(role), None) => crate::storage::RoleSettingScope::RoleAllDatabases(
                    storage.find_role(role).ok_or_else(|| {
                        sql_err!(
                            sqlstate::UNDEFINED_OBJECT,
                            "journal configures unknown role \"{}\"",
                            role
                        )
                    })? as u16,
                ),
                (Some(role), Some(database)) => crate::storage::RoleSettingScope::RoleInDatabase {
                    role: storage.find_role(role).ok_or_else(|| {
                        sql_err!(
                            sqlstate::UNDEFINED_OBJECT,
                            "journal configures unknown role \"{}\"",
                            role
                        )
                    })? as u16,
                    database,
                },
                (None, Some(database)) => {
                    crate::storage::RoleSettingScope::AllRolesInDatabase(database)
                }
                (None, None) => {
                    return Err(sql_err!(
                        sqlstate::INTERNAL_ERROR,
                        "corrupt WAL role setting scope"
                    ));
                }
            };
            let value = value
                .map(|value| {
                    let stored = crate::util::StackStr::from_str(value);
                    if stored.is_truncated() {
                        Err(sql_err!(
                            sqlstate::INTERNAL_ERROR,
                            "corrupt WAL role setting value"
                        ))
                    } else {
                        Ok(stored)
                    }
                })
                .transpose()?;
            storage.install_role_setting(scope, crate::storage::SqlName::parse(name)?, value)?;
        }
        WalOp::SetSystemSetting { name, value } => {
            let value = value
                .map(|value| {
                    let stored = crate::util::StackStr::from_str(value);
                    if stored.is_truncated() {
                        Err(sql_err!(
                            sqlstate::INTERNAL_ERROR,
                            "corrupt WAL system setting value"
                        ))
                    } else {
                        Ok(stored)
                    }
                })
                .transpose()?;
            storage.install_system_setting(crate::storage::SqlName::parse(name)?, value)?;
        }
        WalOp::SetParameterAcl {
            parameter,
            grantee,
            grantor,
            privileges,
            grant_options,
        } => {
            let parameter = crate::sql::ast::ParameterName::parse(parameter).ok_or_else(|| {
                sql_err!(sqlstate::INTERNAL_ERROR, "corrupt WAL parameter ACL name")
            })?;
            let grantee = if grantee.eq_ignore_ascii_case("public") {
                crate::storage::PUBLIC_ROLE
            } else {
                storage.find_role(grantee).ok_or_else(|| {
                    sql_err!(
                        sqlstate::UNDEFINED_OBJECT,
                        "journal configures unknown role \"{}\"",
                        grantee
                    )
                })? as u16
            };
            let grantor = storage.find_role(grantor).ok_or_else(|| {
                sql_err!(
                    sqlstate::UNDEFINED_OBJECT,
                    "journal configures unknown role \"{}\"",
                    grantor
                )
            })? as u16;
            storage.change_parameter_acl(
                parameter,
                grantee,
                grantor,
                privileges,
                grant_options,
                0,
            )?;
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
            let acl_count = storage.acl_entry_count();
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
            let column_acl_count = storage.column_acl_entry_count();
            for slot in 0..column_acl_count {
                let entry = *storage.column_acl_entry(slot);
                if entry.target.relation() != object {
                    continue;
                }
                let (grantee, grantor) = storage.column_acl_identity(slot, 0);
                if grantee == old_owner || grantor == old_owner {
                    storage.change_column_acl_identity(
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
        WalOp::SetColumnAcl {
            class,
            schema,
            name,
            column,
            grantee,
            grantor,
            privileges,
            grant_options,
        } => {
            let class = crate::storage::AccessClass::from_u8(class).ok_or_else(|| {
                sql_err!(sqlstate::INTERNAL_ERROR, "corrupt WAL column ACL class")
            })?;
            if !matches!(
                class,
                crate::storage::AccessClass::Table
                    | crate::storage::AccessClass::View
                    | crate::storage::AccessClass::MaterializedView
            ) {
                return Err(sql_err!(
                    sqlstate::INTERNAL_ERROR,
                    "corrupt WAL column ACL class"
                ));
            }
            let relation = storage.resolve_access_object(class, schema, name, 0);
            let Some(relation) = relation else {
                if privileges.0 == 0 {
                    storage.set_lsn(lsn);
                    return Ok(());
                }
                return Err(sql_err!(
                    sqlstate::UNDEFINED_OBJECT,
                    "WAL column privilege target \"{}.{}\" does not exist",
                    schema,
                    name
                ));
            };
            let column_count = match class {
                crate::storage::AccessClass::Table => {
                    Some(storage.table_def(relation.slot as usize, 0).n_columns)
                }
                crate::storage::AccessClass::MaterializedView => storage
                    .find_table(schema, name)
                    .map(|table| storage.table_def(table, 0).n_columns),
                crate::storage::AccessClass::View => Some(crate::sql::exec::MAX_PROJ),
                _ => unreachable!("column ACL WAL decoder restricts object classes"),
            };
            if column_count.is_some_and(|count| column as usize >= count) {
                return Err(sql_err!(
                    sqlstate::UNDEFINED_COLUMN,
                    "WAL column privilege target {} does not exist",
                    column + 1
                ));
            }
            let target = crate::storage::ColumnPrivilegeTarget::new(relation, column)?;
            let grantee = if grantee == "PUBLIC" {
                crate::storage::PUBLIC_ROLE
            } else {
                let Some(role) = storage.find_role(grantee) else {
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
                role as u16
            };
            let Some(grantor) = storage.find_role(grantor) else {
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
            storage.change_column_acl(
                target,
                grantee,
                grantor as u16,
                privileges,
                grant_options,
                0,
            )?;
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
    connection.require_endpoint().map(|_| ())
}

#[cfg(test)]
mod tests;
