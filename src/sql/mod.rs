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
pub mod guc;
pub mod json;
pub mod lexer;
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
use crate::mem::fixed_vec::FixedVec;
use crate::pg::respond::Responder;
use crate::pg::wire::WireFull;
use crate::sql_err;
use crate::stack_format;
use crate::storage::{RowHome, RowLoc, Storage};
use crate::wal::{Wal, WalOp, WalSetupError};

use crate::pg::conn::MAX_BIND_PARAMS;
use ast::{Delete, Expr, Insert, Stmt, Update};
use eval::{NO_PARAMS, NoColumns, SqlError, eval, sqlstate};
use exec::MAX_PROJ;
use guc::GucState;
use parser::{ParseError, Parser};
use prep::SqlPreparedPool;
use txn::{DdlUndo, IsolationLevel, TxnMode, TxnState};
use types::{ColDesc, ColType, Datum};

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
    rows: &[],
};

/// The query engine: catalog, memtable storage, WAL, object-storage
/// checkpointing, and statement execution.
pub struct Engine {
    storage: Storage,
    wal: Wal,
    ckpt: Option<Checkpointer>,
    /// A COPY FROM STDIN the last statement started: the connection takes
    /// it, switches into copy-in mode, and feeds data lines back through
    /// [`Engine::copy_row_line`] until CopyDone.
    pending_copy: Option<exec::CopySetup>,
    wal_upload: bool,
    /// When set, a commit blocks until its WAL batch is uploaded (RPO=0 to
    /// durable object tier). Otherwise upload is drained off the commit path.
    wal_upload_sync: bool,
    /// Backpressure threshold: once this many bytes of committed WAL await
    /// asynchronous upload, the next commit drains synchronously.
    wal_upload_backpressure: usize,
    /// Scratch buffer for reading committed WAL batches before upload; sized
    /// to hold a full asynchronous accumulation.
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
            let Some(second) = words.next() else {
                return Err(first);
            };
            if !level.eq_ignore_ascii_case("level") {
                return Err(level);
            }
            if first.eq_ignore_ascii_case("read") && second.eq_ignore_ascii_case("committed") {
                parsed.isolation = Some(IsolationLevel::ReadCommitted);
            } else if first.eq_ignore_ascii_case("repeatable")
                && second.eq_ignore_ascii_case("read")
            {
                parsed.isolation = Some(IsolationLevel::RepeatableRead);
            } else {
                return Err(first);
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
        | Stmt::DropView { .. }
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
        | Stmt::DropIndex { .. }
        | Stmt::Checkpoint
        | Stmt::AlterTable(_)
        | Stmt::CreateSchema { .. }
        | Stmt::DropSchema { .. }
        | Stmt::Vacuum { .. }
        | Stmt::Notify { .. }
        | Stmt::Comment { .. }
        | Stmt::AlterOwner { .. } => true,
    }
}

fn statement_changes_schema(statement: &Stmt<'_>) -> bool {
    matches!(
        statement,
        Stmt::CreateTable(_)
            | Stmt::DropTable(_)
            | Stmt::Truncate { .. }
            | Stmt::CreateView { .. }
            | Stmt::DropView { .. }
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
            | Stmt::DropIndex { .. }
            | Stmt::AlterTable(_)
            | Stmt::CreateSchema { .. }
            | Stmt::DropSchema { .. }
            | Stmt::Comment { .. }
            | Stmt::AlterOwner { .. }
    )
}

fn statement_tag(statement: &Stmt<'_>) -> &'static str {
    match statement {
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
        Stmt::Notify { .. } => "NOTIFY",
        _ => "DDL",
    }
}

impl Engine {
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
            + config.wal_upload_buffer_bytes.max(config.wal_buffer_bytes)
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
        let mut storage = Storage::new(config, budget)?;
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
        wal.replay(floor, |lsn, operator| {
            apply_wal_op(&mut storage, lsn, operator)
        })?;
        storage.reconcile_serials();
        // RPO=0: replay any WAL segments in the bucket newer than what the
        // local journal (possibly empty after disk loss) already covered.
        if let Some(c) = ckpt.as_mut() {
            let seg_floor = storage.lsn().max(floor);
            let applied_to = c
                .replay_wal_segments(seg_floor, |lsn, record| {
                    match crate::wal::decode_record(record) {
                        Some(operator) => apply_wal_op(&mut storage, lsn, operator),
                        None => Err(SqlError {
                            sqlstate: sqlstate::INTERNAL_ERROR,
                            message: stack_format!(192, "corrupt uploaded WAL record"),
                        }),
                    }
                })
                .map_err(EngineSetupError::Checkpoint)?;
            if applied_to > storage.lsn() {
                storage.set_lsn(applied_to);
            }
        }
        // Replay's row installs bypass the per-row value-index maintenance, so
        // rebuild every table's uniqueness indexes from the recovered committed
        // rows before serving queries.
        storage.rebuild_all_enforcers()?;
        // The upload buffer must hold at least one full WAL batch, plus room
        // to accumulate more before backpressure forces a synchronous drain.
        let upload_buf = config.wal_upload_buffer_bytes.max(config.wal_buffer_bytes);
        let backpressure = upload_buf.saturating_sub(config.wal_buffer_bytes).max(1);
        Ok(Self {
            storage,
            wal,
            ckpt,
            pending_copy: None,
            wal_upload: config.wal_upload && config.object_store_on,
            wal_upload_sync: config.wal_upload_sync,
            wal_upload_backpressure: backpressure,
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
        })
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
        txn.wal_mark = self.wal.mark();
    }

    /// Commits: journals every touched row, fsyncs once, then promotes the
    /// in-memory images. On failure the transaction rolls back entirely.
    pub fn commit_txn(&mut self, txn: &mut TxnState, guc: &GucState) -> Result<(), SqlError> {
        // The next statement starts a fresh transaction clock.
        datetime::end_transaction();
        if !txn.is_active() {
            return Ok(());
        }
        // This transaction no longer needs its historical view. Release it
        // before promotion so only other live snapshots cause old row images
        // to be retained.
        self.storage.release_snapshot(txn.txid);
        self.storage.release_table_locks(txn.txid);
        // A failed synchronous upload keeps its batch marker, so the next
        // commit retries it. Whether *this* transaction added records to
        // that batch decides who owns a retry failure below: the statement
        // (its outcome really is unknown), or nobody (the records belong to
        // commits already reported failed — the retry is background work).
        let batch_bytes_before = self.wal.pending_batch_bytes();
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
            let appended = match p.loc {
                Some(loc) => self.wal.append(
                    lsn,
                    &WalOp::Upsert {
                        schema: schema.as_str(),
                        table: name.as_str(),
                        rowid,
                        row: self.storage.heap.get(loc),
                    },
                ),
                None => self.wal.append(
                    lsn,
                    &WalOp::Delete {
                        schema: schema.as_str(),
                        table: name.as_str(),
                        rowid,
                    },
                ),
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
            if !self.storage.table(i).serial_dirty || !self.storage.table(i).live {
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
                if let Err(e) = self.wal.append(
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
            self.storage.table_mut(i).serial_dirty = false;
        }
        // Journal sequence advances (this transaction's or ones a rolled-back
        // transaction left dirty). Absolute positions, like serial advances, and
        // deliberately non-transactional: a `nextval` in a rolled-back
        // transaction still consumes its number, matching PostgreSQL's gaps.
        for i in 0..self.storage.sequence_count() {
            let seq = self.storage.sequence(i);
            if !seq.live || !seq.dirty.get() {
                continue;
            }
            let schema = seq.schema;
            let name = seq.name;
            let last = seq.last_value.get();
            let is_called = seq.is_called.get();
            let lsn = self.storage.lsn() + 1;
            if let Err(e) = self.wal.append(
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
            self.storage.sequence(i).dirty.set(false);
        }
        // One fsync per transaction, before any promotion: this is the
        // durability point — and the point of no return. A restart replays
        // everything past it, so from here the transaction commits in this
        // incarnation too, whatever the bucket says: an upload failure below
        // is reported to the client (outcome unknown) only after the
        // promotions, never instead of them. Failing first left a committed
        // transaction invisible until the next restart resurrected it —
        // state a client could watch move backward and then forward.
        self.wal.commit();
        let contributed = self.wal.pending_batch_bytes() > batch_bytes_before;
        let upload_result = if self.wal_upload_sync
            || self.wal.pending_batch_bytes() as usize >= self.wal_upload_backpressure
        {
            match self.upload_wal_batch() {
                Err(e) if !contributed => {
                    // Retrying a previous commit's batch: everything in it
                    // is locally durable and already reported failed to its
                    // own client; a statement that wrote nothing must not
                    // inherit the retry's error.
                    eprintln!(
                        "pos3ql: WAL segment upload retry failed ({}): {}",
                        e.sqlstate,
                        e.message.as_str()
                    );
                    Ok(())
                }
                result => result,
            }
        } else {
            Ok(())
        };
        let mut altered_tables = [(usize::MAX, false); txn::MAX_TXN_DDL];
        let mut altered_count = 0usize;
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
        let commit_lsn = self.storage.lsn();
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
                DdlUndo::Created(slot) => self.storage.commit_create(*slot as usize),
                DdlUndo::Dropped(slot) => {
                    let name = self.storage.table(*slot as usize).def.name;
                    let schema = self.storage.table(*slot as usize).def.schema;
                    self.storage.commit_drop(*slot as usize);
                    // The table's indexes were pending-dropped with it.
                    self.storage
                        .commit_indexes_for(schema.as_str(), name.as_str(), txn.txid);
                }
                DdlUndo::TableAltered(_) => {}
                DdlUndo::ViewCreated(slot) => self.storage.commit_view_create(*slot as usize),
                DdlUndo::ViewDropped(slot) => self.storage.commit_view_drop(*slot as usize),
                DdlUndo::MatviewCreated(slot) => self.storage.commit_matview_create(*slot as usize),
                DdlUndo::MatviewDropped(slot) => self.storage.commit_matview_drop(*slot as usize),
                DdlUndo::SequenceCreated(slot) => {
                    self.storage.commit_sequence_create(*slot as usize)
                }
                DdlUndo::SequenceDropped(slot) => self.storage.commit_sequence_drop(*slot as usize),
                DdlUndo::DomainCreated(slot) => self.storage.commit_domain_create(*slot as usize),
                DdlUndo::DomainDropped(slot) => self.storage.commit_domain_drop(*slot as usize),
                DdlUndo::DomainNullabilityAltered { .. }
                | DdlUndo::DomainDefaultAltered { .. }
                | DdlUndo::DomainCheckAdded { .. }
                | DdlUndo::DomainCheckDropped { .. } => {}
                DdlUndo::EnumCreated(slot) => self.storage.commit_enum_create(*slot as usize),
                DdlUndo::EnumDropped(slot) => self.storage.commit_enum_drop(*slot as usize),
                DdlUndo::EnumValueAdded { .. }
                | DdlUndo::EnumValueRenamed { .. }
                | DdlUndo::EnumRenamed { .. } => {}
                DdlUndo::IndexCreated(slot) => self.storage.commit_index_create(*slot as usize),
                DdlUndo::IndexDropped(slot) => self.storage.commit_index_drop(*slot as usize),
                // The reset already happened in place; committing keeps it.
                DdlUndo::SequenceReset { .. } | DdlUndo::OwnedSequenceReset { .. } => {}
                DdlUndo::SchemaCreated(slot) => self.storage.commit_schema_create(*slot as usize),
                DdlUndo::SchemaDropped(slot) => self.storage.commit_schema_drop(*slot as usize),
                // Promote the uncommitted comment overlay to committed; its WAL
                // record was journaled at exec time (like other DDL).
                DdlUndo::CommentSet { slot, .. } => {
                    self.storage.commit_comment(*slot as usize, txn.txid);
                }
            }
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
        // Past the durability point, so these fire iff the transaction really
        // committed: apply its LISTEN/UNLISTEN to the shared registry and move
        // its notifications into the delivery outbox. A pool-exhaustion here is
        // a loud error reported to the client — like a post-commit upload
        // failure, the data is committed regardless — never a silent drop.
        let notify_result = self.flush_committed_notifications(txn);
        guc.commit_transaction();
        txn.clear();
        notify_result.and(index_result).and(upload_result)
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
            DdlUndo::ViewDropped(slot) => {
                self.storage.rollback_view_drop(slot as usize, txid);
            }
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
            DdlUndo::DomainCreated(slot) => self.storage.rollback_domain_create(slot as usize),
            DdlUndo::DomainDropped(slot) => {
                self.storage.rollback_domain_drop(slot as usize, txid);
            }
            DdlUndo::DomainNullabilityAltered { slot, prior } => self
                .storage
                .restore_domain_nullability(slot as usize, prior),
            DdlUndo::DomainDefaultAltered { slot, prior } => {
                self.storage.restore_domain_default(slot as usize, prior);
            }
            DdlUndo::DomainCheckAdded { slot, prior_count } => self
                .storage
                .undo_domain_check_add(slot as usize, prior_count as usize),
            DdlUndo::DomainCheckDropped { slot, index, prior } => {
                self.storage
                    .restore_domain_check(slot as usize, index as usize, prior);
            }
            DdlUndo::EnumCreated(slot) => self.storage.rollback_enum_create(slot as usize),
            DdlUndo::EnumDropped(slot) => {
                self.storage.rollback_enum_drop(slot as usize, txid);
            }
            DdlUndo::EnumValueAdded { slot, prior_count } => self
                .storage
                .undo_enum_value_add(slot as usize, prior_count as usize),
            DdlUndo::EnumValueRenamed { slot, index, prior } => {
                self.storage
                    .restore_enum_value_name(slot as usize, index as usize, prior);
            }
            DdlUndo::EnumRenamed { slot, prior } => {
                self.storage.rename_enum(slot as usize, prior);
            }
            DdlUndo::IndexCreated(slot) => self.storage.rollback_index_create(slot as usize),
            DdlUndo::IndexDropped(slot) => {
                self.storage.rollback_index_drop(slot as usize, txid);
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
            DdlUndo::OwnedSequenceReset {
                sequence,
                prior,
                prior_called,
            } => {
                let sequence = self.storage.sequence(sequence as usize);
                sequence.last_value.set(prior);
                sequence.is_called.set(prior_called);
                sequence.dirty.set(true);
            }
            DdlUndo::SchemaCreated(slot) => self.storage.rollback_schema_create(slot as usize),
            DdlUndo::SchemaDropped(slot) => self.storage.rollback_schema_drop(slot as usize),
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
        self.storage.release_snapshot(txn.txid);
        self.storage.release_table_locks(txn.txid);
        // Reverse-replay every write to its prior image (newest first), so a
        // row written multiple times unwinds to its pre-transaction state.
        for &(table, rowid, prior) in txn.touched().iter().rev() {
            self.storage
                .restore_pending(table as usize, rowid, txn.txid, prior);
        }
        for &undo in txn.ddl().iter().rev() {
            self.rollback_ddl(undo, txn.txid);
        }
        self.wal.truncate_to_mark(txn.wal_mark);
        guc.rollback_transaction();
        txn.clear();
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
        txn.rewind_touched(sp.touched_mark);
        txn.rewind_ddl(sp.ddl_mark);
        txn.rewind_notifications(sp.notify_mark, sp.notify_payload_mark, sp.listen_mark);
        txn.rollback_savepoints_after(index);
        self.wal.truncate_to_mark(sp.wal_mark);
        guc.rollback_to_savepoint(index);
        txn.failed = sp.failed;
    }

    /// Makes journaled work durable. Called once per query message, before
    /// results are flushed to the client.
    pub fn commit_wal(&mut self) {
        self.wal.commit();
        // Best-effort upload; a failure here is surfaced on the next
        // committing statement rather than crashing an unrelated one.
        if let Err(e) = self.upload_wal_batch() {
            eprintln!(
                "pos3ql: WAL segment upload failed ({}): {}",
                e.sqlstate,
                e.message.as_str()
            );
        }
    }

    /// Uploads the just-committed WAL batch to the bucket (RPO=0 mode).
    fn upload_wal_batch(&mut self) -> Result<(), SqlError> {
        if !self.wal_upload {
            return Ok(());
        }
        let Some((first_lsn, start, end)) = self.wal.last_committed_batch() else {
            return Ok(());
        };
        if end <= start {
            self.wal.clear_batch_marker();
            return Ok(());
        }
        let len = (end - start) as usize;
        self.wal_seg_buf.resize(len, 0);
        if self.wal.read_range(start, &mut self.wal_seg_buf).is_err() {
            return Err(SqlError {
                sqlstate: sqlstate::IO_ERROR,
                message: stack_format!(192, "cannot read WAL batch for upload"),
            });
        }
        if let Some(c) = self.ckpt.as_mut() {
            c.upload_wal_segment(first_lsn, &self.wal_seg_buf)?;
        }
        self.wal.clear_batch_marker();
        Ok(())
    }

    /// Whether committed WAL awaits asynchronous upload. The event loop polls
    /// this to drain uploads between requests without adding object-store
    /// latency to any commit.
    pub fn has_pending_wal_upload(&self) -> bool {
        self.wal_upload && !self.wal_upload_sync && self.wal.pending_batch_bytes() > 0
    }

    /// Uploads the committed WAL batch awaiting asynchronous upload, off the
    /// commit path. Returns whether the drain succeeded (or had nothing to do);
    /// a failure is logged, not propagated — the data is already durable on
    /// local disk, so a bucket hiccup must not disturb request processing. The
    /// caller backs off before retrying so a persistently-down bucket does not
    /// spin the event loop.
    pub fn drain_wal_upload(&mut self) -> bool {
        if !self.has_pending_wal_upload() {
            return true;
        }
        if let Err(e) = self.upload_wal_batch() {
            eprintln!(
                "pos3ql: async WAL segment upload failed ({}): {}",
                e.sqlstate,
                e.message.as_str()
            );
            return false;
        }
        true
    }

    /// Snapshots to object storage, then truncates the journal and compacts
    /// the heap. The atomic form — drives the sliced checkpoint's beats to
    /// completion in one call, for the explicit `CHECKPOINT` statement and
    /// shutdown. `Ok(false)` = nothing to do.
    pub fn checkpoint_enabled(&self) -> bool {
        self.ckpt.is_some()
    }

    fn analyze_targets(
        &self,
        targets: &[ast::MaintenanceTarget<'_>],
        txid: u32,
    ) -> Result<usize, SqlError> {
        let mut total_rows = 0usize;
        if targets.is_empty() {
            for slot in 0..self.storage.table_count() {
                if self.storage.table(slot).visible_to(txid) {
                    total_rows = total_rows
                        .checked_add(self.storage.visible_row_count(slot, txid)?)
                        .ok_or_else(|| {
                            sql_err!(
                                sqlstate::PROGRAM_LIMIT_EXCEEDED,
                                "ANALYZE row count exceeds addressable memory"
                            )
                        })?;
                }
            }
            return Ok(total_rows);
        }
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
            total_rows = total_rows
                .checked_add(self.storage.visible_row_count(slot, txid)?)
                .ok_or_else(|| {
                    sql_err!(
                        sqlstate::PROGRAM_LIMIT_EXCEEDED,
                        "ANALYZE row count exceeds addressable memory"
                    )
                })?;
        }
        Ok(total_rows)
    }

    pub fn checkpoint(&mut self) -> Result<bool, SqlError> {
        let Some(ckpt) = self.ckpt.as_mut() else {
            return Err(SqlError {
                sqlstate: sqlstate::FEATURE_NOT_SUPPORTED,
                message: stack_format!(192, "no object storage configured (object_store = off)"),
            });
        };
        // Everything the snapshot will contain must be journal-durable
        // first, so an interrupted checkpoint never strands acked writes.
        self.wal.commit();
        match ckpt.checkpoint(&mut self.storage, &mut self.scratch)? {
            Some(lsn) => {
                self.after_publish(lsn)?;
                Ok(true)
            }
            None => Ok(false),
        }
    }

    /// The journal and heap bookkeeping owed once a manifest has published:
    /// everything at or below `lsn` is bucket-durable, so the local journal
    /// restarts and the heap compacts (spilling under memory pressure).
    fn after_publish(&mut self, lsn: u64) -> Result<(), SqlError> {
        self.storage.clear_dirty();
        if !self.storage.has_active_snapshots() {
            if self.wal_upload
                && let Some(ckpt) = self.ckpt.as_mut()
            {
                let _ = ckpt.prune_wal_segments(lsn);
            }
            self.wal.reset_after_checkpoint();
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
        self.ckpt
            .as_ref()
            .is_some_and(|c| c.sweep_active() || c.merge_work_pending(&self.storage))
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
        let Some(ckpt) = self.ckpt.as_mut() else {
            return true;
        };
        let heap_full = self.storage.heap.used() * 100 >= self.storage.heap.capacity() * 65;
        let wal_full = self.wal.used_bytes() * 100 >= self.wal.capacity_bytes() * 50;
        let history_full = self.storage.history_pressure();
        if !(ckpt.sweep_active()
            || ckpt.merge_work_pending(&self.storage)
            || heap_full
            || wal_full
            || history_full)
        {
            return true;
        }
        self.wal.commit();
        match ckpt.checkpoint_step(&mut self.storage, &mut self.scratch) {
            Ok(CheckpointStep::Published { lsn }) => {
                if let Err(e) = self.after_publish(lsn) {
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
    ) -> Result<(), WireFull> {
        self.current_conn_id = conn_id;
        let mut parser = match Parser::new(text, arena) {
            Ok(p) => p,
            Err(e) => return report_parse_error(responder, &e),
        };
        // The whole message runs in one implicit transaction unless an
        // explicit block is open — an error undoes the entire message,
        // matching PostgreSQL's implicit-transaction rule.
        // Freeze this statement's clock before anything anchors a transaction
        // to it, so `now()` and `statement_timestamp()` agree on a lone
        // statement as they do in PostgreSQL.
        datetime::begin_statement();
        self.ensure_txn(txn, TxnMode::Implicit, guc);
        let mut executed_any = false;
        loop {
            match parser.next_stmt() {
                Ok(Some(statement)) => {
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
                        return Ok(());
                    }
                    executed_any = true;
                    emit_parse_warnings(&mut parser, responder)?;
                    if let Err(e) = self.execute_stmt(
                        &statement, arena, NO_PARAMS, txn, sqlprep, cursors, guc, responder,
                    )? {
                        if txn.is_explicit() {
                            txn.failed = true;
                        } else {
                            self.rollback_txn(txn, guc);
                        }
                        responder.error(e.sqlstate, e.message.as_str())?;
                        return Ok(());
                    }
                }
                Ok(None) => break,
                Err(e) => {
                    if txn.is_explicit() {
                        txn.failed = true;
                    } else {
                        self.rollback_txn(txn, guc);
                    }
                    return report_parse_error(responder, &e);
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
        Ok(())
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
    ) -> Result<bool, WireFull> {
        self.current_conn_id = conn_id;
        let mut parser = match Parser::new(text, arena) {
            Ok(p) => p,
            Err(e) => {
                report_parse_error(responder, &e)?;
                return Ok(false);
            }
        };
        // Freeze this statement's clock before anything anchors a transaction
        // to it, so `now()` and `statement_timestamp()` agree on a lone
        // statement as they do in PostgreSQL.
        datetime::begin_statement();
        self.ensure_txn(txn, TxnMode::Implicit, guc);
        let outcome = match parser.next_stmt() {
            Ok(Some(statement)) => {
                emit_parse_warnings(&mut parser, responder)?;
                self.execute_stmt(
                    &statement, arena, params, txn, sqlprep, cursors, guc, responder,
                )?
            }
            Ok(None) => {
                responder.empty_query_response()?;
                Ok(())
            }
            Err(e) => {
                if txn.is_explicit() {
                    txn.failed = true;
                } else {
                    self.rollback_txn(txn, guc);
                }
                report_parse_error(responder, &e)?;
                return Ok(false);
            }
        };
        match outcome {
            Ok(()) => {
                if txn.mode == TxnMode::Implicit
                    && self.pending_copy.is_none()
                    && let Err(e) = self.commit_txn(txn, guc)
                {
                    responder.error(e.sqlstate, e.message.as_str())?;
                    return Ok(false);
                }
                Ok(true)
            }
            Err(e) => {
                if txn.is_explicit() {
                    txn.failed = true;
                } else {
                    self.rollback_txn(txn, guc);
                }
                responder.error(e.sqlstate, e.message.as_str())?;
                Ok(false)
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
    fn column_oid(&self, table: &ast::QualName, col: &str, txid: u32) -> Option<i32> {
        let slot = match self
            .storage
            .resolve_relation(table.schema, table.name, txid)
        {
            Some(crate::storage::ResolvedRelation::Table(slot)) => slot,
            _ => return None,
        };
        let def = self.storage.table_def(slot, txid);
        let index = def.column_index(col)?;
        Some(def.columns()[index].ctype.oid())
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
                            ci.map(|ci| d.columns()[ci].ctype.oid())
                        });
                        if let Some(ty) = ty {
                            set(oids, value, ty);
                        }
                    }
                }
            }
            Stmt::Update(u) => {
                for (col, value) in u.assignments {
                    if let Some(ty) = self.column_oid(&u.table, col, txid) {
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
                            && let Some(ty) = self.column_oid(table, name, txid)
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
                            Ok(scope) => query::describe_scope_items(
                                s.items,
                                &scope,
                                &self.storage,
                                txn.txid,
                                &mut columns,
                            ),
                            Err(e) => {
                                responder.error(e.sqlstate, e.message.as_str())?;
                                return Ok(false);
                            }
                        }
                    }
                    None => query::describe_catalog_items(
                        s.items,
                        None,
                        &self.storage,
                        txn.txid,
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
            let mut types = [(0i32, 0i16); MAX_PROJ];
            for i in 0..ncols {
                // Copy the name into the statement arena: a described column
                // name borrows the (local, owned) table def, which drops here.
                let nm = cte.columns.get(i).copied().unwrap_or(descs[i].name);
                names[i] = arena.alloc_str(nm).map_err(|_| query::arena_full_pub())?;
                types[i] = (descs[i].type_oid, descs[i].typlen);
            }
            let column_names = arena
                .alloc_slice_copy(&names[..ncols])
                .map_err(|_| query::arena_full_pub())?;
            let column_types = arena
                .alloc_slice_copy(&types[..ncols])
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
                    rows,
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
    fn execute_data_modification<'a>(
        storage: &mut Storage,
        scratch: &mut FixedVec<(u64, RowHome)>,
        arena: &Arena,
        statement: &'a Stmt<'a>,
        txn: &mut TxnState,
        params: &[Datum<'a>],
        guc: &mut GucState,
        responder: &mut Responder,
        capture: Option<&mut dyn FnMut(&[Datum]) -> Result<(), SqlError>>,
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
                    insert,
                    arena,
                    params,
                    guc.seq_session(),
                    responder,
                    capture,
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
                    storage, txn, scratch, delete, arena, params, responder, capture,
                )
            }
            _ => Ok(Err(sql_err!(
                sqlstate::INTERNAL_ERROR,
                "expected a data-modifying statement"
            ))),
        }
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
        let _guc_eval_scope = guc::enter_eval_scope(guc);
        // Reclaim the shared execution arena from the previous statement: its
        // materialized rows have already been paged to the wire.
        self.work.reset();
        // Drop any diagnostic detail a swallowed error left behind, and
        // install this session's effective search path for the statement:
        // every name resolution below reads it from storage.
        let _ = eval::take_diagnostic();
        exec::reset_record_shapes();
        eval::funcs::system::set_session_user(guc.session_user());
        let raw_path = guc.search_path();
        let path = self
            .storage
            .compute_path(raw_path.as_str(), guc.session_user(), txn.txid);
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
        // old row undecodable; this is the fail-fast form of PostgreSQL's
        // ACCESS SHARE versus ACCESS EXCLUSIVE lock conflict.
        if (self.storage.has_active_snapshots() || self.storage.has_access_share_locks())
            && statement_changes_schema(statement)
        {
            return Ok(Err(sql_err!(
                sqlstate::LOCK_NOT_AVAILABLE,
                "could not obtain schema lock while a historical snapshot is active"
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
            if txn.isolation == IsolationLevel::RepeatableRead
                && let Err(error) = self.storage.register_snapshot(txn.txid, snapshot)
            {
                return Ok(Err(error));
            }
            snapshot
        } else {
            self.storage.lsn()
        };
        self.storage.set_commit_snapshot(commit_snapshot);
        match statement {
            Stmt::With { ctes, statement } => {
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
                    Stmt::Insert(_) | Stmt::Update(_) | Stmt::Delete(_) => {
                        Self::execute_data_modification(
                            &mut self.storage,
                            &mut self.scratch,
                            &self.work,
                            statement,
                            txn,
                            params,
                            guc,
                            responder,
                            None,
                        )
                    }
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
            Stmt::Select(s) => {
                // Data-modifying CTEs run once here (capturing RETURNING) under
                // this statement's command snapshot, before the main query.
                let dml_mats = match self.run_dml_ctes(s.with, txn, arena, params, guc, responder) {
                    Ok(m) => m.unwrap_or(&[]),
                    Err(e) => return Ok(Err(e)),
                };
                // WITH CTEs expand into derived tables before execution; a
                // recursive CTE is materialized to its fixpoint in the work
                // arena (reset per statement, sized for row data).
                let s = match query::expand_ctes_exec(
                    s,
                    &self.storage,
                    txn.txid,
                    &self.work,
                    params,
                    dml_mats,
                ) {
                    Ok(x) => x,
                    Err(e) => return Ok(Err(e)),
                };
                // FOR UPDATE / FOR SHARE row-locking clauses: enforce their
                // analysis-time restrictions (0A000 / 42P01) before executing.
                if let Err(e) = query::validate_locking(s) {
                    return Ok(Err(e));
                }
                // Execution (row materialization) uses the shared work arena;
                // the parsed AST (`s`, `params`) lives in the per-connection
                // arena, which outlives it — so the work arena can be reset
                // per statement while the AST persists across the message.
                let seq = sequence::SeqEval::new(&self.storage, guc.seq_session(), txn.txid);
                if s.from.is_none() {
                    query::constant_select(
                        &self.storage,
                        txn.txid,
                        s,
                        &self.work,
                        params,
                        Some(&seq),
                        responder,
                    )
                } else {
                    query::select_query(
                        &self.storage,
                        txn.txid,
                        s,
                        &self.work,
                        params,
                        Some(&seq),
                        responder,
                    )
                }
            }
            Stmt::SetQuery(q) => {
                query::set_query(&self.storage, txn.txid, q, &self.work, params, responder)
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
                name,
                *or_replace,
                sql,
                guc.search_path().as_str(),
                arena,
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
                unique,
            } => exec::create_index(
                &mut self.storage,
                &mut self.wal,
                txn,
                name,
                table,
                columns,
                *unique,
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
                tables,
                *restart_identity,
                *cascade,
                responder,
            ),
            Stmt::CreateSchema {
                name,
                if_not_exists,
                elements,
            } => {
                let out = exec::create_schema(
                    &mut self.storage,
                    &mut self.wal,
                    txn,
                    name,
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
                    if let Err(e) = self.execute_stmt(
                        requalified,
                        arena,
                        params,
                        txn,
                        sqlprep,
                        cursors,
                        guc,
                        responder,
                    )? {
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
            } => exec::alter_owner(&self.storage, txn, *kind, name, role, *if_exists, responder),
            Stmt::DeclareCursor {
                name,
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
                    let mut capture = Responder::new(cursors.result_buffer(at));
                    capture.set_render(guc.render());
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
                                    None,
                                    &mut capture,
                                )
                            } else {
                                query::select_query(
                                    &self.storage,
                                    txn.txid,
                                    sel,
                                    &self.work,
                                    params,
                                    None,
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
            Stmt::LockTable { tables, nowait: _ } => {
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
                    if let Err(error) = self.storage.lock_table_access_share(txn.txid, slot) {
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
                let mark = self.wal.mark();
                if let Err(e) = txn.savepoint(name, mark) {
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
            Stmt::Vacuum {
                targets,
                analyze: _,
            } => {
                if let Err(error) = self.analyze_targets(targets, txn.txid) {
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
            // Cardinalities are exact live state rather than sampled,
            // periodically stale statistics. ANALYZE still resolves every
            // requested relation/column and walks its visible row state, so it
            // detects inaccessible/corrupt backing data instead of silently
            // accepting and ignoring the command.
            Stmt::Analyze(targets) => {
                if let Err(error) = self.analyze_targets(targets, txn.txid) {
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

/// Rewrites a CREATE SCHEMA element to create inside the new schema. An
/// element that already names that schema passes through; one naming another
/// schema is PostgreSQL's 42P15.
fn requalify_schema_element<'a>(
    element: &'a Stmt<'a>,
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
        Stmt::CreateTable(c) => Stmt::CreateTable(ast::CreateTable {
            name: requalify(c.name)?,
            ..*c
        }),
        Stmt::CreateView {
            name,
            or_replace,
            sql,
        } => Stmt::CreateView {
            name: requalify(*name)?,
            or_replace: *or_replace,
            sql,
        },
        Stmt::CreateIndex {
            name,
            table,
            columns,
            unique,
        } => Stmt::CreateIndex {
            name,
            table: requalify(*table)?,
            columns,
            unique: *unique,
        },
        other => {
            let _ = other;
            return Err(sql_err!(
                sqlstate::FEATURE_NOT_SUPPORTED,
                "unsupported CREATE SCHEMA element"
            ));
        }
    };
    arena
        .alloc(rewritten)
        .map(|r| &*r)
        .map_err(|_| query::arena_full_pub())
}

/// Reapplies one journal record to storage during recovery.
fn apply_wal_op(storage: &mut Storage, lsn: u64, operator: WalOp) -> Result<(), SqlError> {
    match operator {
        WalOp::CreateTable(def) => {
            // A journal written before its schema existed cannot occur going
            // forward (CreateSchema precedes in LSN order), but a pre-schema
            // journal names only public, which always exists.
            storage.create_table(def)?;
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
                storage.alter_sequence(slot, spec, None, owner, generator_for);
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
                base_domain_schema: def.base_domain_schema,
                base: def.base,
                base_type_mod: def.base_type_mod,
                not_null: def.not_null,
                default_expr: def.default_expr,
                checks: def.checks,
                n_checks: def.n_checks,
            };
            if let Some(slot) = storage.domain_slot(def.schema.as_str(), def.name.as_str(), 0) {
                storage.alter_domain(slot, spec);
            } else {
                storage.create_domain(def.schema, def.name, spec, 0)?;
            }
        }
        WalOp::DropDomain { schema, name } => {
            if let Some(slot) = storage.drop_domain(schema, name, 0)? {
                storage.commit_domain_drop(slot);
            }
        }
        WalOp::CreateEnum(def) => {
            // An ALTER ... ADD VALUE replays as a redefinition: redefine in
            // place if the enum exists, else create it committed (txid 0).
            let spec = crate::storage::EnumSpec {
                members: def.members,
                n_members: def.n_members,
            };
            if let Some(slot) = storage.enum_slot(def.schema.as_str(), def.name.as_str(), 0) {
                storage.alter_enum(slot, spec);
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
            storage.rename_enum(slot, crate::storage::SqlName::parse(new_name)?);
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
            n_cols,
            unique,
        } => {
            let slot = storage.create_index(
                crate::storage::IndexDef {
                    schema: crate::storage::SqlName::parse(schema)?,
                    name: crate::storage::SqlName::parse(name)?,
                    table: crate::storage::SqlName::parse(table)?,
                    columns,
                    n_cols,
                    unique,
                    live: true,
                    pending: None,
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
    }
    storage.set_lsn(lsn);
    Ok(())
}

#[cfg(test)]
mod tests;
