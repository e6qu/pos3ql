//! SQL front end: lexer → parser → execution, and the engine entry point
//! the wire protocol calls.

pub mod array;
pub mod ast;
pub mod catalog;
pub mod eval;
pub mod exec;
pub mod guc;
pub mod json;
pub mod lexer;
pub mod md5;
pub mod numeric;
pub mod parser;
pub mod regex;
pub mod datetime;
pub mod encoding;
pub mod prep;
pub mod sha512;
pub mod query;
pub mod txn;
pub mod types;
pub mod range;
pub mod to_char;
pub mod timezone;
pub mod tzif;
pub mod cursor;

use crate::checkpoint::{CheckpointStep, Checkpointer, CheckpointSetupError};
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

use ast::{Delete, Expr, Insert, Stmt, Update};
use crate::pg::conn::MAX_BIND_PARAMS;
use eval::{eval, sqlstate, NoColumns, SqlError, NO_PARAMS};
use exec::MAX_PROJ;
use parser::{ParseError, Parser};
use guc::GucState;
use prep::SqlPreparedPool;
use txn::{DdlUndo, TxnMode, TxnState};
use types::{ColDesc, ColType, Datum};

#[derive(Debug)]
pub enum EngineSetupError {
    Budget(BudgetError),
    Wal(WalSetupError),
    Checkpoint(CheckpointSetupError),
}

impl std::fmt::Display for EngineSetupError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Budget(e) => write!(f, "{e}"),
            Self::Wal(e) => write!(f, "{e}"),
            Self::Checkpoint(e) => write!(f, "{e}"),
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

/// The query engine: catalog, memtable storage, WAL, object-storage
/// checkpointing, and statement execution.
pub struct Engine {
    storage: Storage,
    wal: Wal,
    ckpt: Option<Checkpointer>,
    wal_upload: bool,
    /// When set, a commit blocks until its WAL batch is uploaded (RPO=0 to
    /// S3). Otherwise the upload is drained off the commit path.
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
    compact_scratch: FixedVec<(u32, u64, bool, RowLoc)>,
    /// Shared execution arena: one query's materialized rows (ORDER BY /
    /// DISTINCT / GROUP BY buffers) live here, separate from the small
    /// per-connection AST arena. Single-threaded execution means one
    /// instance serves every connection; reset at the start of each
    /// statement. This is the `work_mem` analogue.
    work: Arena,
    next_txid: u32,
}

impl Engine {
    /// Bytes drawn beyond the row heap, for the memory plan.
    pub fn extra_budget_bytes(config: &Config) -> usize {
        Storage::extra_budget_bytes(config)
            + config.table_rows * size_of::<(u64, RowHome)>()
            + 2 * config.max_tables * config.table_rows * size_of::<(u32, u64, bool, RowLoc)>()
            + config.work_arena_bytes
            + config.wal_upload_buffer_bytes.max(config.wal_buffer_bytes)
            + if config.s3_on {
                // The checkpointer's fixed parts plus the spilled-row reader's
                // two scratch sets.
                Checkpointer::budget_bytes(config)
                    + 2 * (2 * crate::store::MAX_PAYLOAD + crate::store::MAX_ASSEMBLED)
            } else {
                0
            }
    }

    /// Builds storage, loads the latest checkpoint from object storage
    /// (when enabled), and replays the journal tail on top. Startup only.
    pub fn new(config: &Config, budget: &mut Budget) -> Result<Self, EngineSetupError> {
        let mut storage = Storage::new(config, budget)?;
        let mut ckpt = if config.s3_on {
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
        wal.replay(floor, |lsn, operator| apply_wal_op(&mut storage, lsn, operator))?;
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
        // The upload buffer must hold at least one full WAL batch, plus room
        // to accumulate more before backpressure forces a synchronous drain.
        let upload_buf = config.wal_upload_buffer_bytes.max(config.wal_buffer_bytes);
        let backpressure = upload_buf.saturating_sub(config.wal_buffer_bytes).max(1);
        Ok(Self {
            storage,
            wal,
            ckpt,
            wal_upload: config.wal_upload && config.s3_on,
            wal_upload_sync: config.wal_upload_sync,
            wal_upload_backpressure: backpressure,
            wal_seg_buf: Vec::with_capacity(upload_buf),
            scratch: FixedVec::new(budget, "scan_scratch", config.table_rows)?,
            compact_scratch: FixedVec::new(
                budget,
                "compact_scratch",
                2 * config.max_tables * config.table_rows,
            )?,
            work: Arena::new(budget, "work_arena", config.work_arena_bytes)?,
            next_txid: 0,
        })
    }

    /// Starts a transaction if none is active.
    fn ensure_txn(&mut self, txn: &mut TxnState, mode: TxnMode) {
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
        txn.failed = false;
        txn.wal_mark = self.wal.mark();
    }

    /// Commits: journals every touched row, fsyncs once, then promotes the
    /// in-memory images. On failure the transaction rolls back entirely.
    pub fn commit_txn(&mut self, txn: &mut TxnState) -> Result<(), SqlError> {
        // The next statement starts a fresh transaction clock.
        datetime::end_transaction();
        if !txn.is_active() {
            return Ok(());
        }
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
            if txn.touched()[..i].iter().any(|&(t, r, _)| t == table && r == rowid) {
                continue;
            }
            let Some(state) = self.storage.row_state(table as usize, rowid) else {
                continue;
            };
            let Some(p) = state.pending else { continue };
            let t = self.storage.table(table as usize);
            if p.txid != txn.txid {
                continue;
            }
            let name = t.def.name;
            let schema = t.def.schema;
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
                self.rollback_txn(txn);
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
            let name = self.storage.table(i).def.name;
            let schema = self.storage.table(i).def.schema;
            for c in 0..self.storage.table(i).def.n_columns {
                if !self.storage.table(i).def.columns()[c].auto_increment {
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
                    self.rollback_txn(txn);
                    return Err(e);
                }
                self.storage.set_lsn(lsn);
            }
            self.storage.table_mut(i).serial_dirty = false;
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
        for &(table, rowid, _) in txn.touched() {
            self.storage.commit_row(table as usize, rowid, txn.txid);
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
                DdlUndo::ViewCreated(slot) => self.storage.commit_view_create(*slot as usize),
                DdlUndo::ViewDropped(slot) => self.storage.commit_view_drop(*slot as usize),
                DdlUndo::IndexCreated(slot) => self.storage.commit_index_create(*slot as usize),
                DdlUndo::IndexDropped(slot) => self.storage.commit_index_drop(*slot as usize),
                // The reset already happened in place; committing keeps it.
                DdlUndo::SequenceReset { .. } => {}
                DdlUndo::SchemaCreated(slot) => {
                    self.storage.commit_schema_create(*slot as usize)
                }
                DdlUndo::SchemaDropped(slot) => {
                    self.storage.commit_schema_drop(*slot as usize)
                }
                // The removal already happened in place; committing keeps it.
                DdlUndo::FkDropped { .. } => {}
            }
        }
        txn.clear();
        upload_result
    }

    /// Discards every uncommitted change and journal byte of the
    /// transaction.
    pub fn rollback_txn(&mut self, txn: &mut TxnState) {
        // The next statement starts a fresh transaction clock.
        datetime::end_transaction();
        // Reverse-replay every write to its prior image (newest first), so a
        // row written multiple times unwinds to its pre-transaction state.
        for &(table, rowid, prior) in txn.touched().iter().rev() {
            self.storage.restore_pending(table as usize, rowid, txn.txid, prior);
        }
        for undo in txn.ddl().iter().rev() {
            match *undo {
                DdlUndo::Created(slot) => self.storage.rollback_create(slot as usize),
                DdlUndo::Dropped(slot) => {
                    self.storage.rollback_drop(slot as usize);
                    // The table's indexes were pending-dropped with it; revert.
                    let name = self.storage.table(slot as usize).def.name;
                    let schema = self.storage.table(slot as usize).def.schema;
                    self.storage
                        .rollback_indexes_for(schema.as_str(), name.as_str(), txn.txid);
                }
                DdlUndo::ViewCreated(slot) => self.storage.rollback_view_create(slot as usize),
                DdlUndo::ViewDropped(slot) => {
                    self.storage.rollback_view_drop(slot as usize, txn.txid)
                }
                DdlUndo::IndexCreated(slot) => self.storage.rollback_index_create(slot as usize),
                DdlUndo::IndexDropped(slot) => {
                    self.storage.rollback_index_drop(slot as usize, txn.txid)
                }
                DdlUndo::SequenceReset { table, column, prior } => {
                    let t = self.storage.table_mut(table as usize);
                    t.serial_last[column as usize] = prior;
                    t.serial_dirty = true;
                }
                DdlUndo::SchemaCreated(slot) => {
                    self.storage.rollback_schema_create(slot as usize)
                }
                DdlUndo::SchemaDropped(slot) => {
                    self.storage.rollback_schema_drop(slot as usize)
                }
                DdlUndo::FkDropped { table, fk } => {
                    self.storage.restore_fk(table as usize, fk)
                }
            }
        }
        self.wal.truncate_to_mark(txn.wal_mark);
        txn.clear();
    }

    /// Rolls back to the savepoint at `index`: undoes every row write and DDL
    /// performed after it (reverse-replayed), discards the journal tail, and
    /// restores the pre-savepoint failed state — leaving the transaction (and
    /// the savepoint) open for reuse.
    fn rollback_to_savepoint(&mut self, txn: &mut TxnState, index: usize) {
        let sp = txn.savepoint_at(index);
        for i in (sp.touched_mark..txn.touched().len()).rev() {
            let (table, rowid, prior) = txn.touched()[i];
            self.storage.restore_pending(table as usize, rowid, txn.txid, prior);
        }
        for i in (sp.ddl_mark..txn.ddl().len()).rev() {
            match txn.ddl()[i] {
                DdlUndo::Created(slot) => self.storage.rollback_create(slot as usize),
                DdlUndo::Dropped(slot) => {
                    self.storage.rollback_drop(slot as usize);
                    let name = self.storage.table(slot as usize).def.name;
                    let schema = self.storage.table(slot as usize).def.schema;
                    self.storage
                        .rollback_indexes_for(schema.as_str(), name.as_str(), txn.txid);
                }
                DdlUndo::ViewCreated(slot) => self.storage.rollback_view_create(slot as usize),
                DdlUndo::ViewDropped(slot) => {
                    self.storage.rollback_view_drop(slot as usize, txn.txid)
                }
                DdlUndo::IndexCreated(slot) => self.storage.rollback_index_create(slot as usize),
                DdlUndo::IndexDropped(slot) => {
                    self.storage.rollback_index_drop(slot as usize, txn.txid)
                }
                DdlUndo::SequenceReset { table, column, prior } => {
                    let t = self.storage.table_mut(table as usize);
                    t.serial_last[column as usize] = prior;
                    t.serial_dirty = true;
                }
                DdlUndo::SchemaCreated(slot) => {
                    self.storage.rollback_schema_create(slot as usize)
                }
                DdlUndo::SchemaDropped(slot) => {
                    self.storage.rollback_schema_drop(slot as usize)
                }
                DdlUndo::FkDropped { table, fk } => {
                    self.storage.restore_fk(table as usize, fk)
                }
            }
        }
        txn.rewind_touched(sp.touched_mark);
        txn.rewind_ddl(sp.ddl_mark);
        txn.rollback_savepoints_after(index);
        self.wal.truncate_to_mark(sp.wal_mark);
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
    /// this to drain uploads between requests without adding S3 latency to any
    /// commit.
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
    pub fn checkpoint(&mut self) -> Result<bool, SqlError> {
        let Some(ckpt) = self.ckpt.as_mut() else {
            return Err(SqlError {
                sqlstate: sqlstate::FEATURE_NOT_SUPPORTED,
                message: stack_format!(192, "no object storage configured (s3 = off)"),
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
        if self.wal_upload
            && let Some(ckpt) = self.ckpt.as_mut() {
                let _ = ckpt.prune_wal_segments(lsn);
            }
        self.wal.reset_after_checkpoint();
        // The checkpoint installed each table's spill-SST list as it
        // wrote (full rewrites collapse a list, deltas append).
        self.storage.compact_heap(&mut self.compact_scratch)?;
        // Under memory pressure, committed bytes leave the heap: the map
        // entries flip to spilled and a second compaction drops the
        // bytes. Reads fetch them back through the cache tiers. Below the
        // threshold nothing spills and reads stay heap-fast.
        if self.storage.spill_attached()
            && self.storage.heap.used() * 100 >= self.storage.heap.capacity() * 50
        {
            self.storage.evict_committed();
            self.storage.compact_heap(&mut self.compact_scratch)?;
        }
        Ok(())
    }

    /// Whether checkpoint or compaction work is pending — an active sweep,
    /// a paced merge (mid-flight, finished-awaiting-publish, or a list at
    /// the trigger). The event loop keeps beating pending work between
    /// events, so an idle server still finishes what a trigger started and
    /// compacts what its lists owe.
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
        if !(ckpt.sweep_active()
            || ckpt.merge_work_pending(&self.storage)
            || heap_full
            || wal_full)
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
    ) -> Result<(), WireFull> {
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
        self.ensure_txn(txn, TxnMode::Implicit);
        let mut executed_any = false;
        loop {
            match parser.next_stmt() {
                Ok(Some(statement)) => {
                    executed_any = true;
                    emit_parse_warnings(&mut parser, responder)?;
                    if let Err(e) = self.execute_stmt(&statement, arena, NO_PARAMS, txn, sqlprep, cursors, guc, responder)? {
                        if txn.is_explicit() {
                            txn.failed = true;
                        } else {
                            self.rollback_txn(txn);
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
                        self.rollback_txn(txn);
                    }
                    return report_parse_error(responder, &e);
                }
            }
        }
        if !executed_any {
            responder.empty_query_response()?;
        }
        // Implicit transactions commit at end of message.
        if txn.mode == TxnMode::Implicit
            && let Err(e) = self.commit_txn(txn) {
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
    ) -> Result<bool, WireFull> {
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
        self.ensure_txn(txn, TxnMode::Implicit);
        let outcome = match parser.next_stmt() {
            Ok(Some(statement)) => {
                emit_parse_warnings(&mut parser, responder)?;
                self.execute_stmt(&statement, arena, params, txn, sqlprep, cursors, guc, responder)?
            }
            Ok(None) => {
                responder.empty_query_response()?;
                Ok(())
            }
            Err(e) => {
                if txn.is_explicit() {
                    txn.failed = true;
                } else {
                    self.rollback_txn(txn);
                }
                report_parse_error(responder, &e)?;
                return Ok(false);
            }
        };
        match outcome {
            Ok(()) => {
                if txn.mode == TxnMode::Implicit
                    && let Err(e) = self.commit_txn(txn) {
                        responder.error(e.sqlstate, e.message.as_str())?;
                        return Ok(false);
                    }
                Ok(true)
            }
            Err(e) => {
                if txn.is_explicit() {
                    txn.failed = true;
                } else {
                    self.rollback_txn(txn);
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
        let slot = match self.storage.resolve_relation(table.schema, table.name, txid) {
            Some(crate::storage::ResolvedRelation::Table(slot)) => slot,
            _ => return None,
        };
        let def = &self.storage.table(slot).def;
        let index = def.column_index(col)?;
        Some(def.columns()[index].ctype.oid())
    }

    fn infer_stmt_params(&self, statement: &Stmt, txid: u32, oids: &mut [i32; MAX_BIND_PARAMS]) {
        let set = |oids: &mut [i32; MAX_BIND_PARAMS], e: &Expr, ty: i32| {
            if let Expr::Param(n) = e
                && *n >= 1 && (*n as usize) <= MAX_BIND_PARAMS {
                    oids[*n as usize - 1] = ty;
                }
        };
        match statement {
            Stmt::Insert(ins) => {
                let slot = match self.storage.resolve_relation(ins.table.schema, ins.table.name, txid)
                {
                    Some(crate::storage::ResolvedRelation::Table(slot)) => Some(slot),
                    _ => None,
                };
                let def = slot.map(|s| &self.storage.table(s).def);
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
                    && from.joins.is_empty() && from.base.subquery.is_none() {
                        let table =
                            ast::QualName { schema: from.base.schema, name: from.base.table };
                        self.infer_where_params(&table, w, txid, oids);
                    }
            }
            _ => {}
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
        if let Expr::Binary { operator, left, right } = expression {
            match operator {
                And | Or => {
                    self.infer_where_params(table, left, txid, oids);
                    self.infer_where_params(table, right, txid, oids);
                }
                Eq | NotEq | Lt | LtEq | Gt | GtEq => {
                    let mut pair = |c: &Expr, p: &Expr| {
                        if let (Expr::Column { name, .. }, Expr::Param(n)) = (c, p)
                            && *n >= 1 && (*n as usize) <= MAX_BIND_PARAMS
                                && let Some(ty) = self.column_oid(table, name, txid) {
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
                        match query::QueryScope::resolve_schema(&self.storage, from, txn.txid, arena) {
                            Ok(scope) => query::describe_scope_items(s.items, &scope, &mut columns),
                            Err(e) => {
                                responder.error(e.sqlstate, e.message.as_str())?;
                                return Ok(false);
                            }
                        }
                    }
                    None => exec::describe_items(s.items, None, &mut columns),
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

    /// Outer Result: wire-level trouble. Inner Result: SQL-level error.
    #[allow(clippy::too_many_arguments)]
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
        // Reclaim the shared execution arena from the previous statement: its
        // materialized rows have already been paged to the wire.
        self.work.reset();
        // Drop any diagnostic detail a swallowed error left behind, and
        // install this session's effective search path for the statement:
        // every name resolution below reads it from storage.
        let _ = eval::take_diagnostic();
        exec::reset_record_shapes();
        eval::funcs::system::set_session_user(guc.session_user());
        let path =
            self.storage.compute_path(guc.search_path(), guc.session_user(), txn.txid);
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
            && !matches!(statement, Stmt::Commit | Stmt::Rollback | Stmt::RollbackToSavepoint(_))
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
                message: stack_format!(
                    192,
                    "CHECKPOINT cannot run inside a transaction block"
                ),
            }));
        }
        match statement {
            Stmt::Select(s) => {
                // WITH CTEs expand into derived tables before execution; a
                // recursive CTE is materialized to its fixpoint in the work
                // arena (reset per statement, sized for row data).
                let s = match query::expand_ctes_exec(s, &self.storage, txn.txid, &self.work, params) {
                    Ok(x) => x,
                    Err(e) => return Ok(Err(e)),
                };
                // Execution (row materialization) uses the shared work arena;
                // the parsed AST (`s`, `params`) lives in the per-connection
                // arena, which outlives it — so the work arena can be reset
                // per statement while the AST persists across the message.
                if s.from.is_none() {
                    query::constant_select(&self.storage, txn.txid, s, &self.work, params, responder)
                } else {
                    query::select_query(&self.storage, txn.txid, s, &self.work, params, responder)
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
            Stmt::CreateView { name, or_replace, sql } => exec::create_view(
                &mut self.storage,
                &mut self.wal,
                txn,
                name,
                *or_replace,
                sql,
                guc.search_path(),
                arena,
                responder,
            ),
            Stmt::DropView { names, if_exists } => {
                exec::drop_view(&mut self.storage, &mut self.wal, txn, names, *if_exists, responder)
            }
            Stmt::CreateIndex { name, table, columns, unique } => exec::create_index(
                &mut self.storage,
                &mut self.wal,
                txn,
                name,
                table,
                columns,
                *unique,
                responder,
            ),
            Stmt::DropIndex { names, if_exists } => {
                exec::drop_index(&mut self.storage, &mut self.wal, txn, names, *if_exists, responder)
            }
            Stmt::Insert(i) => {
                // DML on an auto-updatable view rewrites to its base table.
                let i = match query::resolve_view_for_dml(&self.storage, i.table, txn.txid, arena) {
                    Ok(Some(uv)) => {
                        // Empty target columns default to the view's exposed
                        // columns, so a base column the view hides is untouched.
                        let columns = if i.columns.is_empty() { uv.columns } else { i.columns };
                        match arena.alloc(Insert {
                            table: uv.base,
                            columns,
                            rows: i.rows,
                            select: i.select,
                            on_conflict: i.on_conflict,
                            returning: i.returning,
                        }) {
                            Ok(ni) => &*ni,
                            Err(_) => return Ok(Err(query::arena_full_pub())),
                        }
                    }
                    Ok(None) => i,
                    Err(e) => return Ok(Err(e)),
                };
                exec::insert(&mut self.storage, txn, i, arena, params, responder)
            }
            Stmt::Update(u) => {
                let u = match query::resolve_view_for_dml(&self.storage, u.table, txn.txid, arena) {
                    Ok(Some(uv)) => {
                        let where_clause =
                            match query::and_where(uv.where_clause, u.where_clause, arena) {
                                Ok(w) => w,
                                Err(e) => return Ok(Err(e)),
                            };
                        match arena.alloc(Update {
                            table: uv.base,
                            assignments: u.assignments,
                            from: u.from,
                            where_clause,
                            returning: u.returning,
                        }) {
                            Ok(nu) => &*nu,
                            Err(_) => return Ok(Err(query::arena_full_pub())),
                        }
                    }
                    Ok(None) => u,
                    Err(e) => return Ok(Err(e)),
                };
                exec::update(&mut self.storage, txn, &mut self.scratch, u, arena, params, responder)
            }
            Stmt::Delete(d) => {
                let d = match query::resolve_view_for_dml(&self.storage, d.table, txn.txid, arena) {
                    Ok(Some(uv)) => {
                        let where_clause =
                            match query::and_where(uv.where_clause, d.where_clause, arena) {
                                Ok(w) => w,
                                Err(e) => return Ok(Err(e)),
                            };
                        match arena.alloc(Delete {
                            table: uv.base,
                            using: d.using,
                            where_clause,
                            returning: d.returning,
                        }) {
                            Ok(nd) => &*nd,
                            Err(_) => return Ok(Err(query::arena_full_pub())),
                        }
                    }
                    Ok(None) => d,
                    Err(e) => return Ok(Err(e)),
                };
                exec::delete(&mut self.storage, txn, &mut self.scratch, d, arena, params, responder)
            }
            Stmt::Truncate { tables, restart_identity, cascade } => {
                exec::truncate(&mut self.storage, txn, tables, *restart_identity, *cascade, responder)
            }
            Stmt::CreateSchema { name, if_not_exists, elements } => {
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
                    if let Err(e) = self
                        .execute_stmt(requalified, arena, params, txn, sqlprep, cursors, guc, responder)?
                    {
                        return Ok(Err(e));
                    }
                }
                Ok(Ok(()))
            }
            Stmt::DropSchema { names, if_exists, cascade } => exec::drop_schema(
                &mut self.storage,
                &mut self.wal,
                txn,
                names,
                *if_exists,
                *cascade,
                responder,
            ),
            Stmt::DeclareCursor { name, scroll, hold, sql } => {
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
                                sel, &self.storage, txn.txid, &self.work, params,
                            ) {
                                Ok(x) => x,
                                Err(e) => {
                                    cursors.abandon(at);
                                    return Ok(Err(e));
                                }
                            };
                            if sel.from.is_none() {
                                query::constant_select(
                                    &self.storage, txn.txid, sel, &self.work, params,
                                    &mut capture,
                                )
                            } else {
                                query::select_query(
                                    &self.storage, txn.txid, sel, &self.work, params,
                                    &mut capture,
                                )
                            }
                        }
                        Stmt::SetQuery(q) => query::set_query(
                            &self.storage, txn.txid, q, &self.work, params, &mut capture,
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
            Stmt::FetchCursor { name, motion, move_only } => {
                let count = match cursors.fetch(name, *motion) {
                    Ok(c) => c,
                    Err(e) => return Ok(Err(e)),
                };
                if !*move_only {
                    let (description, rows) =
                        cursors.wire_parts(name).expect("fetch found it");
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
            Stmt::Begin => {
                if txn.is_explicit() {
                    // PostgreSQL warns and continues.
                    responder.warning(
                        crate::sql::eval::sqlstate::ACTIVE_SQL_TRANSACTION,
                        "there is already a transaction in progress",
                    )?;
                }
                self.ensure_txn(txn, TxnMode::Explicit);
                responder.command_complete("BEGIN")?;
                Ok(Ok(()))
            }
            Stmt::Commit => {
                if !txn.is_explicit() {
                    responder.warning("25P01", "there is no transaction in progress")?;
                }
                let tag = if txn.failed { "ROLLBACK" } else { "COMMIT" };
                if txn.failed {
                    self.rollback_txn(txn);
                    cursors.on_rollback();
                } else {
                    if let Err(e) = self.commit_txn(txn) {
                        return Ok(Err(e));
                    }
                    cursors.on_commit();
                }
                responder.command_complete(tag)?;
                // Later statements in this message get a fresh implicit txn.
                // Freeze this statement's clock before anything anchors a
                // transaction to it.
                datetime::begin_statement();
        self.ensure_txn(txn, TxnMode::Implicit);
                Ok(Ok(()))
            }
            Stmt::Rollback => {
                if !txn.is_explicit() {
                    responder.warning("25P01", "there is no transaction in progress")?;
                }
                self.rollback_txn(txn);
                cursors.on_rollback();
                responder.command_complete("ROLLBACK")?;
                // Freeze this statement's clock before anything anchors a
                // transaction to it.
                datetime::begin_statement();
        self.ensure_txn(txn, TxnMode::Implicit);
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
                self.rollback_to_savepoint(txn, index);
                responder.command_complete("ROLLBACK")?;
                Ok(Ok(()))
            }
            Stmt::Set { name, value } => match guc.set(name, value) {
                Ok(()) => {
                    responder.command_complete("SET")?;
                    Ok(Ok(()))
                }
                Err(e) => Ok(Err(e)),
            },
            Stmt::SetTransaction => {
                responder.command_complete("SET")?;
                Ok(Ok(()))
            }
            Stmt::Show(name) => self.show(name, guc, responder),
            Stmt::ShowAll => self.show_all(guc, responder),
            Stmt::Checkpoint => match self.checkpoint() {
                Ok(_) => {
                    responder.command_complete("CHECKPOINT")?;
                    Ok(Ok(()))
                }
                Err(e) => Ok(Err(e)),
            },
            Stmt::AlterTable(a) => {
                if txn.is_explicit() {
                    return Ok(Err(SqlError {
                        sqlstate: sqlstate::FEATURE_NOT_SUPPORTED,
                        message: stack_format!(
                            192,
                            "ALTER TABLE cannot run inside a transaction block yet"
                        ),
                    }));
                }
                // ALTER acts as an autocommit barrier: prior implicit work
                // commits, the rewrite runs and commits by itself.
                if let Err(e) = self.commit_txn(txn) {
                    return Ok(Err(e));
                }
                // Freeze this statement's clock before anything anchors a
                // transaction to it.
                datetime::begin_statement();
        self.ensure_txn(txn, TxnMode::Implicit);
                let out = exec::alter_table(
                    &mut self.storage,
                    &mut self.wal,
                    &mut self.scratch,
                    a,
                    arena,
                    responder,
                )?;
                match out {
                    Ok(()) => {
                        if let Err(e) = self.commit_txn(txn) {
                            return Ok(Err(e));
                        }
                        // Freeze this statement's clock before anything anchors a
                // transaction to it.
                datetime::begin_statement();
        self.ensure_txn(txn, TxnMode::Implicit);
                        Ok(Ok(()))
                    }
                    Err(e) => {
                        self.rollback_txn(txn);
                        // Freeze this statement's clock before anything anchors a
                // transaction to it.
                datetime::begin_statement();
        self.ensure_txn(txn, TxnMode::Implicit);
                        Ok(Err(e))
                    }
                }
            }
            Stmt::Prepare { name, sql, param_types } => {
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
                            }))
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
                        }))
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
                        }))
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
        let value = match fixed_setting(name).or_else(|| guc.get(name)) {
            Some(v) => v,
            None => {
                return Ok(Err(SqlError {
                    sqlstate: sqlstate::UNDEFINED_OBJECT,
                    message: stack_format!(
                        192,
                        "unrecognized configuration parameter \"{}\"",
                        name
                    ),
                }))
            }
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
        const NAMES: &[&str] = &[
            "application_name",
            "bytea_output",
            "client_encoding",
            "client_min_messages",
            "DateStyle",
            "extra_float_digits",
            "idle_in_transaction_session_timeout",
            "integer_datetimes",
            "is_superuser",
            "lock_timeout",
            "row_security",
            "search_path",
            "server_encoding",
            "server_version",
            "standard_conforming_strings",
            "statement_timeout",
            "TimeZone",
            "transaction_isolation",
        ];
        responder.row_description(&[
            ColDesc::new("name", types::oid::TEXT, -1),
            ColDesc::new("setting", types::oid::TEXT, -1),
            ColDesc::new("description", types::oid::TEXT, -1),
        ])?;
        for &name in NAMES {
            if let Some(v) = fixed_setting(name).or_else(|| guc.get(name)) {
                responder.data_row(&[Datum::Text(name), Datum::Text(v), Datum::Text("")])?;
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
        "server_encoding" => Some("UTF8"),
        "standard_conforming_strings" => Some("on"),
        "integer_datetimes" => Some("on"),
        "transaction_isolation" => Some("read committed"),
        "is_superuser" => Some("on"),
        _ => None,
    }
}

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
            None => Ok(ast::QualName { schema: Some(schema), name: name.name }),
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
        Stmt::CreateTable(c) => {
            Stmt::CreateTable(ast::CreateTable { name: requalify(c.name)?, ..*c })
        }
        Stmt::CreateView { name, or_replace, sql } => Stmt::CreateView {
            name: requalify(*name)?,
            or_replace: *or_replace,
            sql,
        },
        Stmt::CreateIndex { name, table, columns, unique } => Stmt::CreateIndex {
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
        WalOp::SequenceSet { schema, table, column, last } => {
            let Some(index) = storage.find_table(schema, table) else {
                return Err(SqlError {
                    sqlstate: sqlstate::UNDEFINED_TABLE,
                    message: stack_format!(192, "journal sets a sequence of unknown table \"{}\"", table),
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
        WalOp::Upsert { schema, table, rowid, row } => {
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
                .insert(rowid, crate::storage::RowState::committed_only(loc))
                .map_err(|e| SqlError {
                    sqlstate: sqlstate::PROGRAM_LIMIT_EXCEEDED,
                    message: stack_format!(192, "journal replay overflows {}", e.what),
                })?;
        }
        WalOp::Delete { schema, table, rowid } => {
            let Some(index) = storage.find_table(schema, table) else {
                return Err(SqlError {
                    sqlstate: sqlstate::UNDEFINED_TABLE,
                    message: stack_format!(192, "journal deletes from unknown table \"{}\"", table),
                });
            };
            storage.remove_committed(index, rowid);
        }
        WalOp::CreateView { schema, name, sql, path } => {
            // Replay reconstructs committed state: create then promote.
            let mut buffer = crate::util::StackStr::<{ crate::storage::VIEW_SQL_MAX }>::new();
            use core::fmt::Write;
            let _ = write!(buffer, "{sql}");
            let mut creation_path = crate::util::StackStr::<128>::new();
            let _ = write!(creation_path, "{path}");
            let (new_slot, old_slot) = storage.create_view(
                crate::storage::SqlName::parse(schema)?,
                crate::storage::SqlName::parse(name)?,
                buffer,
                creation_path,
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
        WalOp::CreateIndex { schema, name, table, columns, n_cols, unique } => {
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
        WalOp::SetTableSchema { schema, name, new_schema } => {
            let Some(index) = storage.find_table(schema, name) else {
                return Err(SqlError {
                    sqlstate: sqlstate::UNDEFINED_TABLE,
                    message: stack_format!(192, "journal moves unknown table \"{}\"", name),
                });
            };
            storage.move_table_schema(index, crate::storage::SqlName::parse(new_schema)?);
        }
        WalOp::DropTableFk { schema, table, fk_name } => {
            let Some(index) = storage.find_table(schema, table) else {
                return Err(SqlError {
                    sqlstate: sqlstate::UNDEFINED_TABLE,
                    message: stack_format!(192, "journal severs a key of unknown table \"{}\"", table),
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
