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

use crate::checkpoint::{Checkpointer, CheckpointSetupError};
use crate::config::Config;
use crate::mem::arena::Arena;
use crate::mem::budget::{Budget, BudgetError};
use crate::mem::fixed_vec::FixedVec;
use crate::pg::respond::Responder;
use crate::pg::wire::WireFull;
use crate::sql_err;
use crate::stack_format;
use crate::storage::{RowLoc, Storage};
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
    /// S3). Otherwise the upload is drained offset the commit path.
    wal_upload_sync: bool,
    /// Backpressure threshold: once this many bytes of committed WAL await
    /// asynchronous upload, the next commit drains synchronously.
    wal_upload_backpressure: usize,
    /// Scratch buffer for reading committed WAL batches before upload; sized
    /// to hold a full asynchronous accumulation.
    wal_seg_buf: Vec<u8>,
    /// Scratch for materializing scans (ORDER BY, UPDATE, DELETE) and for
    /// sorting SST entries at checkpoint.
    scratch: FixedVec<(u64, RowLoc)>,
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
            + config.table_rows * size_of::<(u64, RowLoc)>()
            + 2 * config.max_tables * config.table_rows * size_of::<(u32, u64, bool, RowLoc)>()
            + config.work_arena_bytes
            + config.wal_upload_buffer_bytes.max(config.wal_buffer_bytes)
            + if config.s3_on {
                Checkpointer::budget_bytes(config)
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
        for i in 0..txn.touched().len() {
            let (table, rowid, _) = txn.touched()[i];
            // A row may be written several times in one transaction; journal
            // its final committed image once.
            if txn.touched()[..i].iter().any(|&(t, r, _)| t == table && r == rowid) {
                continue;
            }
            let t = self.storage.table(table as usize);
            let Some(state) = t.rows.get(&rowid) else {
                continue;
            };
            let Some(p) = state.pending else { continue };
            if p.txid != txn.txid {
                continue;
            }
            let name = t.def.name;
            let lsn = self.storage.lsn() + 1;
            let appended = match p.loc {
                Some(loc) => self.wal.append(
                    lsn,
                    &WalOp::Upsert {
                        table: name.as_str(),
                        rowid,
                        row: self.storage.heap.get(loc),
                    },
                ),
                None => self.wal.append(
                    lsn,
                    &WalOp::Delete {
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
            for c in 0..self.storage.table(i).def.n_columns {
                if !self.storage.table(i).def.columns()[c].auto_increment {
                    continue;
                }
                let last = self.storage.table(i).serial_last[c];
                let lsn = self.storage.lsn() + 1;
                if let Err(e) = self.wal.append(
                    lsn,
                    &WalOp::SequenceSet { table: name.as_str(), column: c as u16, last },
                ) {
                    self.rollback_txn(txn);
                    return Err(e);
                }
                self.storage.set_lsn(lsn);
            }
            self.storage.table_mut(i).serial_dirty = false;
        }
        // One fsync per transaction, before any promotion: this is the
        // durability point. In synchronous mode the batch is also uploaded to
        // the bucket before acking (RPO=0 to S3); otherwise the upload is left
        // for the event loop to drain, and only forced synchronously here when
        // the accumulated batch has grown past the backpressure threshold.
        self.wal.commit();
        if self.wal_upload_sync
            || self.wal.pending_batch_bytes() as usize >= self.wal_upload_backpressure
        {
            self.upload_wal_batch()?;
        }
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
                    self.storage.commit_drop(*slot as usize);
                    // The table's indexes were pending-dropped with it.
                    self.storage.commit_indexes_for(name.as_str(), txn.txid);
                }
                DdlUndo::ViewCreated(slot) => self.storage.commit_view_create(*slot as usize),
                DdlUndo::ViewDropped(slot) => self.storage.commit_view_drop(*slot as usize),
                DdlUndo::IndexCreated(slot) => self.storage.commit_index_create(*slot as usize),
                DdlUndo::IndexDropped(slot) => self.storage.commit_index_drop(*slot as usize),
                // The reset already happened in place; committing keeps it.
                DdlUndo::SequenceReset { .. } => {}
            }
        }
        txn.clear();
        Ok(())
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
                    self.storage.rollback_indexes_for(name.as_str(), txn.txid);
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
                    self.storage.rollback_indexes_for(name.as_str(), txn.txid);
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

    /// Uploads the committed WAL batch awaiting asynchronous upload, offset the
    /// commit path. Returns whether the drain succeeded (or had nothing to do);
    /// a failure is logged, not propagated — the data is already durable on
    /// local disk, so a bucket hiccup must not disturb request processing. The
    /// caller backs offset before retrying so a persistently-down bucket does not
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
    /// the heap. `Ok(false)` = nothing to do.
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
        let lsn = self.storage.lsn();
        let wrote = ckpt.checkpoint(&self.storage, &mut self.scratch)?;
        if wrote {
            self.storage.clear_dirty();
            if self.wal_upload {
                let _ = ckpt.prune_wal_segments(lsn);
            }
            self.wal.reset_after_checkpoint();
            self.storage.compact_heap(&mut self.compact_scratch)?;
        }
        Ok(wrote)
    }

    /// Auto-checkpoint when the heap or journal is filling up. Called after
    /// each query message; failures are reported on stderr and retried on
    /// the next message rather than failing unrelated statements.
    pub fn maybe_checkpoint(&mut self) {
        if self.ckpt.is_none() {
            return;
        }
        let heap_full = self.storage.heap.used() * 100 >= self.storage.heap.capacity() * 65;
        let wal_full = self.wal.used_bytes() * 100 >= self.wal.capacity_bytes() * 50;
        if (heap_full || wal_full)
            && let Err(e) = self.checkpoint() {
                eprintln!(
                    "pos3ql: auto-checkpoint failed ({}): {}",
                    e.sqlstate,
                    e.message.as_str()
                );
            }
    }

    /// Executes a simple-query string (possibly several statements).
    /// SQL errors become ErrorResponses and stop the remainder, as in
    /// PostgreSQL. `Err(WireFull)` means the send buffer overflowed and the
    /// connection must handle it.
    pub fn execute_simple(
        &mut self,
        text: &str,
        arena: &Arena,
        txn: &mut TxnState,
        sqlprep: &mut SqlPreparedPool,
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
                    if let Err(e) = self.execute_stmt(&statement, arena, NO_PARAMS, txn, sqlprep, guc, responder)? {
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
                self.execute_stmt(&statement, arena, params, txn, sqlprep, guc, responder)?
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
    fn column_oid(&self, table: &str, col: &str, txid: u32) -> Option<i32> {
        let slot = self.storage.find_visible(table, txid)?;
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
                let slot = self.storage.find_visible(ins.table, txid);
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
                    if let Some(ty) = self.column_oid(u.table, col, txid) {
                        set(oids, value, ty);
                    }
                }
                if let Some(w) = u.where_clause {
                    self.infer_where_params(u.table, w, txid, oids);
                }
            }
            Stmt::Delete(d) => {
                if let Some(w) = d.where_clause {
                    self.infer_where_params(d.table, w, txid, oids);
                }
            }
            Stmt::Select(s) => {
                // Single-table WHERE comparisons only (joins would need scope
                // resolution; those params stay text).
                if let (Some(from), Some(w)) = (&s.from, s.where_clause)
                    && from.joins.is_empty() && from.base.subquery.is_none() {
                        self.infer_where_params(from.base.table, w, txid, oids);
                    }
            }
            _ => {}
        }
    }

    /// Walks a single-table predicate, typing a `Column OP $n` (or the mirror)
    /// parameter from the column's type.
    fn infer_where_params(
        &self,
        table: &str,
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
    fn execute_stmt(
        &mut self,
        statement: &Stmt,
        arena: &Arena,
        params: &[Datum],
        txn: &mut TxnState,
        sqlprep: &mut SqlPreparedPool,
        guc: &mut GucState,
        responder: &mut Responder,
    ) -> Result<Result<(), SqlError>, WireFull> {
        // Reclaim the shared execution arena from the previous statement: its
        // materialized rows have already been paged to the wire.
        self.work.reset();
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
                arena,
                responder,
            ),
            Stmt::DropView { name, if_exists } => {
                exec::drop_view(&mut self.storage, &mut self.wal, txn, name, *if_exists, responder)
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
            Stmt::DropIndex { name, if_exists } => {
                exec::drop_index(&mut self.storage, &mut self.wal, txn, name, *if_exists, responder)
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
                } else if let Err(e) = self.commit_txn(txn) {
                    return Ok(Err(e));
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
        responder.row_description(&[ColDesc::new(name, types::oid::TEXT, -1)])?;
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

/// Reapplies one journal record to storage during recovery.
fn apply_wal_op(storage: &mut Storage, lsn: u64, operator: WalOp) -> Result<(), SqlError> {
    match operator {
        WalOp::CreateTable(def) => {
            storage.create_table(def)?;
        }
        WalOp::SequenceSet { table, column, last } => {
            let Some(index) = storage.find_table(table) else {
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
        WalOp::DropTable(name) => {
            let Some(index) = storage.find_table(name) else {
                return Err(SqlError {
                    sqlstate: sqlstate::UNDEFINED_TABLE,
                    message: stack_format!(192, "journal drops unknown table \"{}\"", name),
                });
            };
            storage.drop_table(index);
            storage.drop_indexes_for(name, 0);
            storage.commit_indexes_for(name, 0);
        }
        WalOp::Upsert { table, rowid, row } => {
            let Some(index) = storage.find_table(table) else {
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
        WalOp::Delete { table, rowid } => {
            let Some(index) = storage.find_table(table) else {
                return Err(SqlError {
                    sqlstate: sqlstate::UNDEFINED_TABLE,
                    message: stack_format!(192, "journal deletes from unknown table \"{}\"", table),
                });
            };
            storage.table_mut(index).rows.remove(&rowid);
        }
        WalOp::CreateView { name, sql } => {
            // Replay reconstructs committed state: create then promote.
            let mut buffer = crate::util::StackStr::<{ crate::storage::VIEW_SQL_MAX }>::new();
            use core::fmt::Write;
            let _ = write!(buffer, "{sql}");
            let (new_slot, old_slot) =
                storage.create_view(crate::storage::SqlName::parse(name)?, buffer, true, 0)?;
            storage.commit_view_create(new_slot);
            if let Some(old) = old_slot {
                storage.commit_view_drop(old);
            }
        }
        WalOp::DropView(name) => {
            if let Some(slot) = storage.drop_view(name, 0)? {
                storage.commit_view_drop(slot);
            }
        }
        WalOp::CreateIndex { name, table, columns, n_cols, unique } => {
            let slot = storage.create_index(
                crate::storage::IndexDef {
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
        WalOp::DropIndex(name) => {
            if let Some(slot) = storage.drop_index(name, 0)? {
                storage.commit_index_drop(slot);
            }
        }
    }
    storage.set_lsn(lsn);
    Ok(())
}

#[cfg(test)]
mod tests;
