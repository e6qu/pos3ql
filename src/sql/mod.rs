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
pub mod prep;
pub mod query;
pub mod txn;
pub mod types;
pub mod range;
pub mod to_char;
pub mod tz;

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
        wal.replay(floor, |lsn, op| apply_wal_op(&mut storage, lsn, op))?;
        // RPO=0: replay any WAL segments in the bucket newer than what the
        // local journal (possibly empty after disk loss) already covered.
        if let Some(c) = ckpt.as_mut() {
            let seg_floor = storage.lsn().max(floor);
            let applied_to = c
                .replay_wal_segments(seg_floor, |lsn, record| {
                    match crate::wal::decode_record(record) {
                        Some(op) => apply_wal_op(&mut storage, lsn, op),
                        None => Err(SqlError {
                            sqlstate: "XX000",
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
        txn.failed = false;
        txn.wal_mark = self.wal.mark();
    }

    /// Commits: journals every touched row, fsyncs once, then promotes the
    /// in-memory images. On failure the transaction rolls back entirely.
    pub fn commit_txn(&mut self, txn: &mut TxnState) -> Result<(), SqlError> {
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
                // Promote a transaction's uncommitted table DDL into the
                // committed catalog now that the journal is durable.
                DdlUndo::Created(slot) => self.storage.commit_create(*slot as usize),
                DdlUndo::Dropped(slot) => self.storage.commit_drop(*slot as usize),
                // Views and indexes are applied to the committed catalog at
                // execution time; nothing to promote here.
                DdlUndo::ViewCreated(_)
                | DdlUndo::ViewDropped(_)
                | DdlUndo::IndexCreated(_)
                | DdlUndo::IndexDropped(_) => {}
            }
        }
        txn.clear();
        Ok(())
    }

    /// Discards every uncommitted change and journal byte of the
    /// transaction.
    pub fn rollback_txn(&mut self, txn: &mut TxnState) {
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
                    // The table's indexes were dropped with it; revive them too.
                    let name = self.storage.table(slot as usize).def.name;
                    self.storage.revive_indexes_for(name.as_str());
                }
                DdlUndo::ViewCreated(slot) => self.storage.drop_view_slot(slot as usize),
                DdlUndo::ViewDropped(slot) => self.storage.revive_view(slot as usize),
                DdlUndo::IndexCreated(slot) => self.storage.drop_index_slot(slot as usize),
                DdlUndo::IndexDropped(slot) => self.storage.revive_index(slot as usize),
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
                    self.storage.revive_indexes_for(name.as_str());
                }
                DdlUndo::ViewCreated(slot) => self.storage.drop_view_slot(slot as usize),
                DdlUndo::ViewDropped(slot) => self.storage.revive_view(slot as usize),
                DdlUndo::IndexCreated(slot) => self.storage.drop_index_slot(slot as usize),
                DdlUndo::IndexDropped(slot) => self.storage.revive_index(slot as usize),
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
                sqlstate: "58030",
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
        resp: &mut Responder,
    ) -> Result<(), WireFull> {
        let mut parser = match Parser::new(text, arena) {
            Ok(p) => p,
            Err(e) => return report_parse_error(resp, &e),
        };
        // The whole message runs in one implicit transaction unless an
        // explicit block is open — an error undoes the entire message,
        // matching PostgreSQL's implicit-transaction rule.
        self.ensure_txn(txn, TxnMode::Implicit);
        let mut executed_any = false;
        loop {
            match parser.next_stmt() {
                Ok(Some(statement)) => {
                    executed_any = true;
                    if let Err(e) = self.execute_stmt(&statement, arena, NO_PARAMS, txn, sqlprep, guc, resp)? {
                        if txn.is_explicit() {
                            txn.failed = true;
                        } else {
                            self.rollback_txn(txn);
                        }
                        resp.error(e.sqlstate, e.message.as_str())?;
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
                    return report_parse_error(resp, &e);
                }
            }
        }
        if !executed_any {
            resp.empty_query_response()?;
        }
        // Implicit transactions commit at end of message.
        if txn.mode == TxnMode::Implicit
            && let Err(e) = self.commit_txn(txn) {
                resp.error(e.sqlstate, e.message.as_str())?;
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
        resp: &mut Responder,
    ) -> Result<bool, WireFull> {
        let mut parser = match Parser::new(text, arena) {
            Ok(p) => p,
            Err(e) => {
                report_parse_error(resp, &e)?;
                return Ok(false);
            }
        };
        self.ensure_txn(txn, TxnMode::Implicit);
        let outcome = match parser.next_stmt() {
            Ok(Some(statement)) => self.execute_stmt(&statement, arena, params, txn, sqlprep, guc, resp)?,
            Ok(None) => {
                resp.empty_query_response()?;
                Ok(())
            }
            Err(e) => {
                if txn.is_explicit() {
                    txn.failed = true;
                } else {
                    self.rollback_txn(txn);
                }
                report_parse_error(resp, &e)?;
                return Ok(false);
            }
        };
        match outcome {
            Ok(()) => {
                if txn.mode == TxnMode::Implicit
                    && let Err(e) = self.commit_txn(txn) {
                        resp.error(e.sqlstate, e.message.as_str())?;
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
                resp.error(e.sqlstate, e.message.as_str())?;
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
                    for (i, val) in row.iter().enumerate() {
                        let ty = def.and_then(|d| {
                            let ci = if ins.columns.is_empty() {
                                (i < d.n_columns).then_some(i)
                            } else {
                                ins.columns.get(i).and_then(|c| d.column_index(c))
                            };
                            ci.map(|ci| d.columns()[ci].ctype.oid())
                        });
                        if let Some(ty) = ty {
                            set(oids, val, ty);
                        }
                    }
                }
            }
            Stmt::Update(u) => {
                for (col, val) in u.assignments {
                    if let Some(ty) = self.column_oid(u.table, col, txid) {
                        set(oids, val, ty);
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
        if let Expr::Binary { op, left, right } = expression {
            match op {
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
        resp: &mut Responder,
    ) -> Result<bool, WireFull> {
        // resp already carries the portal's result-format flag when this is
        // a portal Describe (set by the caller).
        let mut parser = match Parser::new(text, arena) {
            Ok(p) => p,
            Err(e) => {
                report_parse_error(resp, &e)?;
                return Ok(false);
            }
        };
        let statement = match parser.next_stmt() {
            Ok(Some(statement)) => statement,
            Ok(None) => {
                resp.no_data()?;
                return Ok(true);
            }
            Err(e) => {
                report_parse_error(resp, &e)?;
                return Ok(false);
            }
        };
        match &statement {
            Stmt::Select(s) => {
                // Describe the CTE-expanded query so derived columns resolve.
                let s = match query::expand_ctes(s, &self.storage, arena) {
                    Ok(x) => x,
                    Err(e) => {
                        resp.error(e.sqlstate, e.message.as_str())?;
                        return Ok(false);
                    }
                };
                let mut cols = [ColDesc::new("", 0, 0); MAX_PROJ];
                let described = match &s.from {
                    Some(from) => {
                        match query::QueryScope::resolve_schema(&self.storage, from, txn.txid, arena) {
                            Ok(scope) => query::describe_scope_items(s.items, &scope, &mut cols),
                            Err(e) => {
                                resp.error(e.sqlstate, e.message.as_str())?;
                                return Ok(false);
                            }
                        }
                    }
                    None => exec::describe_items(s.items, None, &mut cols),
                };
                match described {
                    Ok(n) => {
                        resp.row_description(&cols[..n])?;
                        Ok(true)
                    }
                    Err(e) => {
                        resp.error(e.sqlstate, e.message.as_str())?;
                        Ok(false)
                    }
                }
            }
            Stmt::Show(name) => {
                resp.row_description(&[ColDesc::new(name, types::oid::TEXT, -1)])?;
                Ok(true)
            }
            _ => {
                resp.no_data()?;
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
        resp: &mut Responder,
    ) -> Result<Result<(), SqlError>, WireFull> {
        // Reclaim the shared execution arena from the previous statement: its
        // materialized rows have already been paged to the wire.
        self.work.reset();
        // Arm this statement's `statement_timeout` deadline (0 clears it); each
        // statement re-arms, so no explicit disarm is needed.
        query::arm_timeout(guc.statement_timeout_ms());
        // Render output with the current session settings (a SET earlier in the
        // same batch takes effect here).
        resp.set_render(guc.render());
        // Inside a failed explicit block only COMMIT/ROLLBACK (and ROLLBACK TO
        // SAVEPOINT, which recovers the block) act.
        if txn.failed
            && !matches!(statement, Stmt::Commit | Stmt::Rollback | Stmt::RollbackToSavepoint(_))
        {
            return Ok(Err(SqlError {
                sqlstate: "25P02",
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
                // WITH CTEs expand into derived tables before execution.
                let s = match query::expand_ctes(s, &self.storage, arena) {
                    Ok(x) => x,
                    Err(e) => return Ok(Err(e)),
                };
                // Execution (row materialization) uses the shared work arena;
                // the parsed AST (`s`, `params`) lives in the per-connection
                // arena, which outlives it — so the work arena can be reset
                // per statement while the AST persists across the message.
                if s.from.is_none() {
                    query::constant_select(&self.storage, txn.txid, s, &self.work, params, resp)
                } else {
                    query::select_query(&self.storage, txn.txid, s, &self.work, params, resp)
                }
            }
            Stmt::SetQuery(q) => {
                query::set_query(&self.storage, txn.txid, q, &self.work, params, resp)
            }
            Stmt::CreateTable(c) => {
                exec::create_table(&mut self.storage, &mut self.wal, txn, c, arena, resp)
            }
            Stmt::DropTable(d) => {
                exec::drop_table(&mut self.storage, &mut self.wal, txn, d, resp)
            }
            Stmt::CreateView { name, or_replace, sql } => exec::create_view(
                &mut self.storage,
                &mut self.wal,
                txn,
                name,
                *or_replace,
                sql,
                arena,
                resp,
            ),
            Stmt::DropView { name, if_exists } => {
                exec::drop_view(&mut self.storage, &mut self.wal, txn, name, *if_exists, resp)
            }
            Stmt::CreateIndex { name, table, columns, unique } => exec::create_index(
                &mut self.storage,
                &mut self.wal,
                txn,
                name,
                table,
                columns,
                *unique,
                resp,
            ),
            Stmt::DropIndex { name, if_exists } => {
                exec::drop_index(&mut self.storage, &mut self.wal, txn, name, *if_exists, resp)
            }
            Stmt::Insert(i) => {
                // DML on an auto-updatable view rewrites to its base table.
                let i = match query::resolve_view_for_dml(&self.storage, i.table, arena) {
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
                exec::insert(&mut self.storage, txn, i, arena, params, resp)
            }
            Stmt::Update(u) => {
                let u = match query::resolve_view_for_dml(&self.storage, u.table, arena) {
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
                exec::update(&mut self.storage, txn, &mut self.scratch, u, arena, params, resp)
            }
            Stmt::Delete(d) => {
                let d = match query::resolve_view_for_dml(&self.storage, d.table, arena) {
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
                exec::delete(&mut self.storage, txn, &mut self.scratch, d, arena, params, resp)
            }
            Stmt::Begin => {
                if txn.is_explicit() {
                    // PostgreSQL warns and continues.
                    resp.warning(
                        "25001",
                        "there is already a transaction in progress",
                    )?;
                }
                self.ensure_txn(txn, TxnMode::Explicit);
                resp.command_complete("BEGIN")?;
                Ok(Ok(()))
            }
            Stmt::Commit => {
                if !txn.is_explicit() {
                    resp.warning("25P01", "there is no transaction in progress")?;
                }
                let tag = if txn.failed { "ROLLBACK" } else { "COMMIT" };
                if txn.failed {
                    self.rollback_txn(txn);
                } else if let Err(e) = self.commit_txn(txn) {
                    return Ok(Err(e));
                }
                resp.command_complete(tag)?;
                // Later statements in this message get a fresh implicit txn.
                self.ensure_txn(txn, TxnMode::Implicit);
                Ok(Ok(()))
            }
            Stmt::Rollback => {
                if !txn.is_explicit() {
                    resp.warning("25P01", "there is no transaction in progress")?;
                }
                self.rollback_txn(txn);
                resp.command_complete("ROLLBACK")?;
                self.ensure_txn(txn, TxnMode::Implicit);
                Ok(Ok(()))
            }
            Stmt::Savepoint(name) => {
                if !txn.is_explicit() {
                    return Ok(Err(sql_err!(
                        "25P01",
                        "SAVEPOINT can only be used in transaction blocks"
                    )));
                }
                let mark = self.wal.mark();
                if let Err(e) = txn.savepoint(name, mark) {
                    return Ok(Err(e));
                }
                resp.command_complete("SAVEPOINT")?;
                Ok(Ok(()))
            }
            Stmt::ReleaseSavepoint(name) => {
                if !txn.is_explicit() {
                    return Ok(Err(sql_err!(
                        "25P01",
                        "RELEASE SAVEPOINT can only be used in transaction blocks"
                    )));
                }
                match txn.savepoint_index(name) {
                    Some(index) => {
                        txn.release_savepoints_from(index);
                        resp.command_complete("RELEASE")?;
                        Ok(Ok(()))
                    }
                    None => Ok(Err(sql_err!(
                        "3B001",
                        "savepoint \"{}\" does not exist",
                        name
                    ))),
                }
            }
            Stmt::RollbackToSavepoint(name) => {
                if !txn.is_explicit() {
                    return Ok(Err(sql_err!(
                        "25P01",
                        "ROLLBACK TO SAVEPOINT can only be used in transaction blocks"
                    )));
                }
                let Some(index) = txn.savepoint_index(name) else {
                    return Ok(Err(sql_err!(
                        "3B001",
                        "savepoint \"{}\" does not exist",
                        name
                    )));
                };
                self.rollback_to_savepoint(txn, index);
                resp.command_complete("ROLLBACK")?;
                Ok(Ok(()))
            }
            Stmt::Set { name, value } => match guc.set(name, value) {
                Ok(()) => {
                    resp.command_complete("SET")?;
                    Ok(Ok(()))
                }
                Err(e) => Ok(Err(e)),
            },
            Stmt::SetTransaction => {
                resp.command_complete("SET")?;
                Ok(Ok(()))
            }
            Stmt::Show(name) => self.show(name, guc, resp),
            Stmt::ShowAll => self.show_all(guc, resp),
            Stmt::Checkpoint => match self.checkpoint() {
                Ok(_) => {
                    resp.command_complete("CHECKPOINT")?;
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
                self.ensure_txn(txn, TxnMode::Implicit);
                let out = exec::alter_table(
                    &mut self.storage,
                    &mut self.wal,
                    &mut self.scratch,
                    a,
                    arena,
                    resp,
                )?;
                match out {
                    Ok(()) => {
                        if let Err(e) = self.commit_txn(txn) {
                            return Ok(Err(e));
                        }
                        self.ensure_txn(txn, TxnMode::Implicit);
                        Ok(Ok(()))
                    }
                    Err(e) => {
                        self.rollback_txn(txn);
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
                                sqlstate: "42704",
                                message: stack_format!(192, "type \"{}\" does not exist", tn),
                            }))
                        }
                    }
                }
                match sqlprep.store(name, sql, &types[..param_types.len()]) {
                    Ok(()) => {
                        resp.command_complete("PREPARE")?;
                        Ok(Ok(()))
                    }
                    Err(e) => Ok(Err(e)),
                }
            }
            Stmt::ExecutePrepared { name, args } => {
                let Some(text) = sqlprep.get(name) else {
                    return Ok(Err(SqlError {
                        sqlstate: "26000",
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
                        sqlstate: "08P01",
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
                        resp,
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
                                sqlstate: "26000",
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
                resp.command_complete("DEALLOCATE")?;
                Ok(Ok(()))
            }
        }
    }

    fn show(
        &mut self,
        name: &str,
        guc: &GucState,
        resp: &mut Responder,
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
        resp.row_description(&[ColDesc::new(name, types::oid::TEXT, -1)])?;
        resp.data_row(&[Datum::Text(value)])?;
        resp.command_complete("SHOW")?;
        Ok(Ok(()))
    }

    /// SHOW ALL: every readable setting as (name, setting, description). Tools
    /// read name/setting; descriptions are left empty.
    fn show_all(
        &mut self,
        guc: &GucState,
        resp: &mut Responder,
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
        resp.row_description(&[
            ColDesc::new("name", types::oid::TEXT, -1),
            ColDesc::new("setting", types::oid::TEXT, -1),
            ColDesc::new("description", types::oid::TEXT, -1),
        ])?;
        for &name in NAMES {
            if let Some(v) = fixed_setting(name).or_else(|| guc.get(name)) {
                resp.data_row(&[Datum::Text(name), Datum::Text(v), Datum::Text("")])?;
            }
        }
        resp.command_complete("SHOW")?;
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

fn report_parse_error(resp: &mut Responder, e: &ParseError) -> Result<(), WireFull> {
    resp.error(sqlstate::SYNTAX_ERROR, e.message.as_str())
}

/// Reapplies one journal record to storage during recovery.
fn apply_wal_op(storage: &mut Storage, lsn: u64, op: WalOp) -> Result<(), SqlError> {
    match op {
        WalOp::CreateTable(def) => {
            storage.create_table(def)?;
        }
        WalOp::DropTable(name) => {
            let Some(index) = storage.find_table(name) else {
                return Err(SqlError {
                    sqlstate: sqlstate::UNDEFINED_TABLE,
                    message: stack_format!(192, "journal drops unknown table \"{}\"", name),
                });
            };
            storage.drop_table(index);
            storage.drop_indexes_for(name);
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
            let mut buffer = crate::util::StackStr::<{ crate::storage::VIEW_SQL_MAX }>::new();
            use core::fmt::Write;
            let _ = write!(buffer, "{sql}");
            storage.create_view(crate::storage::SqlName::parse(name)?, buffer, true)?;
        }
        WalOp::DropView(name) => {
            storage.drop_view(name);
        }
        WalOp::CreateIndex { name, table, cols, n_cols, unique } => {
            storage.create_index(crate::storage::IndexDef {
                name: crate::storage::SqlName::parse(name)?,
                table: crate::storage::SqlName::parse(table)?,
                cols,
                n_cols,
                unique,
                live: true,
            })?;
        }
        WalOp::DropIndex(name) => {
            storage.drop_index(name);
        }
    }
    storage.set_lsn(lsn);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_config(name: &str) -> Config {
        let dir = std::env::temp_dir().join(format!(
            "pos3ql-engine-{}-{}",
            std::process::id(),
            name
        ));
        let _ = std::fs::remove_dir_all(&dir);
        let mut config = Config::default_dev();
        config.data_dir = dir.to_str().unwrap().to_string();
        config.memtable_bytes = 1 << 20;
        config.max_tables = 8;
        config.table_rows = 1024;
        config.wal_bytes = 1 << 20;
        config.wal_buffer_bytes = 1 << 14;
        config.work_arena_bytes = 1 << 21;
        config
    }

    fn test_engine() -> (Engine, Budget) {
        // Each test gets its own journal; the caller's function name is not
        // available, so a counter differentiates them.
        use core::sync::atomic::{AtomicU32, Ordering};
        static N: AtomicU32 = AtomicU32::new(0);
        let n = N.fetch_add(1, Ordering::SeqCst);
        let name = format!("t{n}");
        let config = test_config(&name);
        let mut budget = Budget::new(1 << 25);
        let engine = Engine::new(&config, &mut budget).unwrap();
        (engine, budget)
    }

    fn run_with(engine: &mut Engine, budget: &mut Budget, sql_text: &str) -> Vec<u8> {
        let mut buffer = crate::mem::FixedBuf::new(budget, "send", 1 << 18).unwrap();
        let arena = Arena::new(budget, "sql", 1 << 18).unwrap();
        let mut txn = TxnState::new(budget, 1024).unwrap();
        let mut pool = test_pool(budget);
        let mut guc = GucState::new();
        let mut resp = Responder::new(&mut buffer);
        engine
            .execute_simple(sql_text, &arena, &mut txn, &mut pool, &mut guc, &mut resp)
            .unwrap();
        buffer.readable().to_vec()
    }

    fn test_pool(budget: &mut Budget) -> SqlPreparedPool {
        let mut c = Config::default_dev();
        c.max_prepared = 4;
        c.prepared_bytes = 1024;
        SqlPreparedPool::new(&c, budget).unwrap()
    }

    fn message_types(bytes: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        let mut i = 0;
        while i < bytes.len() {
            out.push(bytes[i]);
            let len = i32::from_be_bytes(bytes[i + 1..i + 5].try_into().unwrap()) as usize;
            i += 1 + len;
        }
        out
    }

    /// Extracts text values from DataRow messages, '|'-joined per row.
    fn data_rows(bytes: &[u8]) -> Vec<String> {
        let mut out = Vec::new();
        let mut i = 0;
        while i < bytes.len() {
            let t = bytes[i];
            let len = i32::from_be_bytes(bytes[i + 1..i + 5].try_into().unwrap()) as usize;
            if t == b'D' {
                let mut row = String::new();
                let payload = &bytes[i + 5..i + 1 + len];
                let ncols = i16::from_be_bytes(payload[..2].try_into().unwrap()) as usize;
                let mut at = 2;
                for c in 0..ncols {
                    if c > 0 {
                        row.push('|');
                    }
                    let vlen =
                        i32::from_be_bytes(payload[at..at + 4].try_into().unwrap());
                    at += 4;
                    if vlen < 0 {
                        row.push_str("NULL");
                    } else {
                        row.push_str(
                            core::str::from_utf8(&payload[at..at + vlen as usize]).unwrap(),
                        );
                        at += vlen as usize;
                    }
                }
                out.push(row);
            }
            i += 1 + len;
        }
        out
    }

    #[test]
    fn create_insert_select_roundtrip() {
        let (mut e, mut b) = test_engine();
        run_with(&mut e, &mut b, "CREATE TABLE t (id int NOT NULL, name text, score float8)");
        let bytes = run_with(
            &mut e,
            &mut b,
            "INSERT INTO t VALUES (1, 'alpha', 1.5), (2, 'beta', NULL), (3, NULL, 2.5)",
        );
        assert_eq!(message_types(&bytes), [b'C']);
        let bytes = run_with(&mut e, &mut b, "SELECT * FROM t ORDER BY id");
        assert_eq!(
            data_rows(&bytes),
            ["1|alpha|1.5", "2|beta|NULL", "3|NULL|2.5"]
        );
        let bytes = run_with(
            &mut e,
            &mut b,
            "SELECT name, score * 2 AS double_score FROM t WHERE id <= 2 ORDER BY id DESC",
        );
        assert_eq!(data_rows(&bytes), ["beta|NULL", "alpha|3"]);
    }

    #[test]
    fn update_and_delete() {
        let (mut e, mut b) = test_engine();
        run_with(&mut e, &mut b, "CREATE TABLE t (id int, v text)");
        run_with(&mut e, &mut b, "INSERT INTO t VALUES (1,'a'),(2,'b'),(3,'c')");
        let bytes = run_with(&mut e, &mut b, "UPDATE t SET v = v || '!' WHERE id > 1");
        let types = message_types(&bytes);
        assert_eq!(types, [b'C']);
        let bytes = run_with(&mut e, &mut b, "SELECT v FROM t ORDER BY id");
        assert_eq!(data_rows(&bytes), ["a", "b!", "c!"]);
        run_with(&mut e, &mut b, "DELETE FROM t WHERE id = 2");
        let bytes = run_with(&mut e, &mut b, "SELECT id FROM t ORDER BY id");
        assert_eq!(data_rows(&bytes), ["1", "3"]);
    }

    #[test]
    fn constraint_and_type_errors() {
        let (mut e, mut b) = test_engine();
        run_with(&mut e, &mut b, "CREATE TABLE t (id int NOT NULL, v text)");
        let bytes = run_with(&mut e, &mut b, "INSERT INTO t VALUES (NULL, 'x')");
        assert_eq!(message_types(&bytes), [b'E']);
        let text = String::from_utf8_lossy(&bytes).to_string();
        assert!(text.contains("23502"), "{text}");
        let bytes = run_with(&mut e, &mut b, "SELECT * FROM missing");
        let text = String::from_utf8_lossy(&bytes).to_string();
        assert!(text.contains("42P01"), "{text}");
        let bytes = run_with(&mut e, &mut b, "CREATE TABLE t (id int)");
        let text = String::from_utf8_lossy(&bytes).to_string();
        assert!(text.contains("42P07"), "{text}");
        let bytes = run_with(&mut e, &mut b, "CREATE TABLE IF NOT EXISTS t (id int)");
        // NoticeResponse then CommandComplete, as in PostgreSQL.
        assert_eq!(message_types(&bytes), [b'N', b'C']);
    }

    #[test]
    fn order_by_nulls_last_and_limit() {
        let (mut e, mut b) = test_engine();
        run_with(&mut e, &mut b, "CREATE TABLE t (v int)");
        run_with(&mut e, &mut b, "INSERT INTO t VALUES (3),(NULL),(1),(2)");
        let bytes = run_with(&mut e, &mut b, "SELECT v FROM t ORDER BY v");
        assert_eq!(data_rows(&bytes), ["1", "2", "3", "NULL"]);
        let bytes = run_with(&mut e, &mut b, "SELECT v FROM t ORDER BY v DESC LIMIT 2");
        assert_eq!(data_rows(&bytes), ["NULL", "3"]);
    }

    #[test]
    fn large_sort_materializes_in_shared_work_arena() {
        // A sort whose materialized rows exceed the per-connection AST arena
        // (256 KiB in run_with) must still succeed by buffering in the larger
        // shared work arena — matching PostgreSQL's in-memory sort. LIMIT keeps
        // the wire output small, so this isolates the sort buffer from the send
        // buffer: only the full materialization can overflow.
        let (mut e, mut b) = test_engine();
        run_with(&mut e, &mut b, "CREATE TABLE t (id int, pad text)");
        let pad = "x".repeat(300);
        // 900 rows x ~320 bytes materialized ~= 288 KiB, above the 256 KiB AST
        // arena but well within the 2 MiB test work arena.
        for base in 0..30 {
            let mut sql = String::from("INSERT INTO t VALUES ");
            for i in 0..30 {
                if i > 0 {
                    sql.push(',');
                }
                let id = base * 30 + i;
                sql.push_str(&format!("({id},'{pad}')"));
            }
            let bytes = run_with(&mut e, &mut b, &sql);
            assert!(
                message_types(&bytes).contains(&b'C'),
                "insert failed: {}",
                String::from_utf8_lossy(&bytes)
            );
        }
        // Materialize all 900 wide rows to sort, emit only the top 3.
        let bytes = run_with(
            &mut e,
            &mut b,
            "SELECT id, pad FROM t ORDER BY id LIMIT 3",
        );
        assert!(
            !message_types(&bytes).contains(&b'E'),
            "large sort errored: {}",
            String::from_utf8_lossy(&bytes)
        );
        let rows = data_rows(&bytes);
        assert_eq!(rows.len(), 3);
        assert_eq!(rows[0], format!("0|{pad}"));
        assert_eq!(rows[1], format!("1|{pad}"));
        assert_eq!(rows[2], format!("2|{pad}"));
    }

    #[test]
    fn text_coercion_on_insert() {
        let (mut e, mut b) = test_engine();
        run_with(&mut e, &mut b, "CREATE TABLE t (id int, flag bool)");
        run_with(&mut e, &mut b, "INSERT INTO t VALUES ('42', 'true')");
        let bytes = run_with(&mut e, &mut b, "SELECT id, flag FROM t");
        assert_eq!(data_rows(&bytes), ["42|t"]);
        let bytes = run_with(&mut e, &mut b, "INSERT INTO t VALUES ('zap', 'true')");
        let text = String::from_utf8_lossy(&bytes).to_string();
        // Bad text for an integer column is a data error (22P02), matching
        // PostgreSQL, not a generic type mismatch.
        assert!(text.contains("22P02"), "{text}");
    }

    #[test]
    fn select_one_still_works() {
        let (mut e, mut b) = test_engine();
        let bytes = run_with(&mut e, &mut b, "SELECT 1");
        assert_eq!(message_types(&bytes), [b'T', b'D', b'C']);
    }

    /// Like run_with but with a caller-owned TxnState, so explicit
    /// transactions span calls (one call ≈ one wire message).
    fn run_txn(
        engine: &mut Engine,
        budget: &mut Budget,
        txn: &mut TxnState,
        sql_text: &str,
    ) -> String {
        let mut buffer = crate::mem::FixedBuf::new(budget, "send", 1 << 18).unwrap();
        let arena = Arena::new(budget, "sql", 1 << 18).unwrap();
        let mut pool = test_pool(budget);
        let mut guc = GucState::new();
        let mut resp = Responder::new(&mut buffer);
        engine
            .execute_simple(sql_text, &arena, txn, &mut pool, &mut guc, &mut resp)
            .unwrap();
        String::from_utf8_lossy(buffer.readable()).to_string()
    }

    #[test]
    fn explicit_rollback_discards_writes() {
        let (mut e, mut b) = test_engine();
        let mut t = TxnState::new(&mut b, 256).unwrap();
        run_txn(&mut e, &mut b, &mut t, "CREATE TABLE t (id int, v text)");
        run_txn(&mut e, &mut b, &mut t, "INSERT INTO t VALUES (1,'keep')");
        run_txn(&mut e, &mut b, &mut t, "BEGIN");
        assert_eq!(t.status_byte(), b'T');
        run_txn(&mut e, &mut b, &mut t, "INSERT INTO t VALUES (2,'discard')");
        run_txn(&mut e, &mut b, &mut t, "UPDATE t SET v = 'changed' WHERE id = 1");
        run_txn(&mut e, &mut b, &mut t, "DELETE FROM t WHERE id = 1");
        // Inside the txn, the changes are visible to itself.
        let out = run_txn(&mut e, &mut b, &mut t, "SELECT count(*) FROM t");
        assert!(out.contains('1'), "{out}");
        run_txn(&mut e, &mut b, &mut t, "ROLLBACK");
        assert_eq!(t.status_byte(), b'I');
        let out = run_txn(&mut e, &mut b, &mut t, "SELECT id, v FROM t ORDER BY id");
        assert!(out.contains("keep") && !out.contains("discard") && !out.contains("changed"), "{out}");
    }

    #[test]
    fn uncommitted_create_is_invisible_to_other_sessions() {
        let (mut e, mut b) = test_engine();
        let mut a = TxnState::new(&mut b, 256).unwrap();
        let mut s = TxnState::new(&mut b, 256).unwrap();
        run_txn(&mut e, &mut b, &mut a, "BEGIN");
        run_txn(&mut e, &mut b, &mut a, "CREATE TABLE t (id int)");
        run_txn(&mut e, &mut b, &mut a, "INSERT INTO t VALUES (1)");
        // The creator sees its own uncommitted table.
        let own = run_txn(&mut e, &mut b, &mut a, "SELECT id FROM t");
        assert!(own.contains("SELECT 1"), "creator sees its own table: {own}");
        // Another session does not.
        let other = run_txn(&mut e, &mut b, &mut s, "SELECT id FROM t");
        assert!(other.contains("does not exist"), "other must not see it: {other}");
        // Nor can it create the same name concurrently.
        let conflict = run_txn(&mut e, &mut b, &mut s, "CREATE TABLE t (x int)");
        assert!(conflict.contains("40001"), "concurrent create conflicts: {conflict}");
        // After commit it becomes visible to everyone.
        run_txn(&mut e, &mut b, &mut a, "COMMIT");
        let now = run_txn(&mut e, &mut b, &mut s, "SELECT id FROM t");
        assert!(now.contains("SELECT 1"), "visible after commit: {now}");
    }

    #[test]
    fn uncommitted_drop_stays_visible_to_other_sessions() {
        let (mut e, mut b) = test_engine();
        let mut a = TxnState::new(&mut b, 256).unwrap();
        let mut s = TxnState::new(&mut b, 256).unwrap();
        run_txn(&mut e, &mut b, &mut a, "CREATE TABLE t (id int)");
        run_txn(&mut e, &mut b, &mut a, "INSERT INTO t VALUES (7)");
        run_txn(&mut e, &mut b, &mut a, "BEGIN");
        let dropped = run_txn(&mut e, &mut b, &mut a, "DROP TABLE t");
        assert!(dropped.contains("DROP TABLE"), "drop succeeds: {dropped}");
        // Another session still sees the committed table and its rows (the
        // drop is not visible until it commits).
        let other = run_txn(&mut e, &mut b, &mut s, "SELECT id FROM t");
        assert!(other.contains("SELECT 1") && other.contains('7'), "other still sees it: {other}");
        run_txn(&mut e, &mut b, &mut a, "COMMIT");
        let after = run_txn(&mut e, &mut b, &mut s, "SELECT id FROM t");
        assert!(after.contains("does not exist"), "gone after commit: {after}");
    }

    #[test]
    fn dropper_does_not_see_its_own_dropped_table() {
        let (mut e, mut b) = test_engine();
        let mut a = TxnState::new(&mut b, 256).unwrap();
        run_txn(&mut e, &mut b, &mut a, "CREATE TABLE t (id int)");
        run_txn(&mut e, &mut b, &mut a, "BEGIN");
        run_txn(&mut e, &mut b, &mut a, "DROP TABLE t");
        // Referencing the just-dropped table errors and, as in PostgreSQL,
        // aborts the transaction (so a later COMMIT rolls back).
        let own = run_txn(&mut e, &mut b, &mut a, "SELECT id FROM t");
        assert!(own.contains("does not exist"), "dropper does not see it: {own}");
        assert_eq!(a.status_byte(), b'E', "the failed reference aborts the txn");
        run_txn(&mut e, &mut b, &mut a, "ROLLBACK");
    }

    #[test]
    fn rolled_back_create_never_appears_and_frees_the_slot() {
        let (mut e, mut b) = test_engine();
        let mut a = TxnState::new(&mut b, 256).unwrap();
        let mut s = TxnState::new(&mut b, 256).unwrap();
        run_txn(&mut e, &mut b, &mut a, "BEGIN");
        run_txn(&mut e, &mut b, &mut a, "CREATE TABLE r (id int)");
        run_txn(&mut e, &mut b, &mut a, "ROLLBACK");
        let gone = run_txn(&mut e, &mut b, &mut a, "SELECT id FROM r");
        assert!(gone.contains("does not exist"), "rolled-back create is gone: {gone}");
        // The freed slot is reusable by a fresh create of the same name.
        let recreate = run_txn(&mut e, &mut b, &mut s, "CREATE TABLE r (x int)");
        assert!(recreate.contains("CREATE TABLE"), "slot reusable: {recreate}");
    }

    #[test]
    fn rolled_back_drop_keeps_the_table() {
        let (mut e, mut b) = test_engine();
        let mut a = TxnState::new(&mut b, 256).unwrap();
        run_txn(&mut e, &mut b, &mut a, "CREATE TABLE t (id int)");
        run_txn(&mut e, &mut b, &mut a, "INSERT INTO t VALUES (5)");
        run_txn(&mut e, &mut b, &mut a, "BEGIN");
        run_txn(&mut e, &mut b, &mut a, "DROP TABLE t");
        run_txn(&mut e, &mut b, &mut a, "ROLLBACK");
        let out = run_txn(&mut e, &mut b, &mut a, "SELECT id FROM t");
        assert!(out.contains("SELECT 1") && out.contains('5'), "table survives rolled-back drop: {out}");
    }

    #[test]
    fn client_min_messages_filters_by_severity() {
        let (mut e, mut b) = test_engine();
        let mut t = TxnState::new(&mut b, 256).unwrap();
        // Default (notice): a DROP IF EXISTS on a missing table emits a NOTICE.
        let out = run_txn(&mut e, &mut b, &mut t, "DROP TABLE IF EXISTS nope");
        assert!(out.contains("NOTICE") && out.contains("does not exist"), "{out}");
        // At `warning`, the NOTICE is suppressed but a WARNING survives.
        let out = run_txn(
            &mut e,
            &mut b,
            &mut t,
            "SET client_min_messages = warning; DROP TABLE IF EXISTS nope; ROLLBACK",
        );
        assert!(!out.contains("does not exist"), "NOTICE must be filtered: {out}");
        assert!(
            out.contains("WARNING") && out.contains("no transaction in progress"),
            "WARNING must survive: {out}"
        );
        // Unknown level errors like PostgreSQL (22023); a valid level shows back.
        let out = run_txn(&mut e, &mut b, &mut t, "SET client_min_messages = bogus");
        assert!(out.contains("22023"), "{out}");
        let out = run_txn(&mut e, &mut b, &mut t, "SHOW client_min_messages");
        assert!(out.contains("notice"), "{out}");
    }

    #[test]
    fn session_gucs_honored_or_rejected_faithfully() {
        let (mut e, mut b) = test_engine();
        let mut t = TxnState::new(&mut b, 256).unwrap();
        let mut run = |sql: &str| run_txn(&mut e, &mut b, &mut t, sql);
        // Honored (the driver/tool session-setup set): each acknowledges SET.
        for s in [
            "SET extra_float_digits = 3",
            "SET lock_timeout = 5000",
            "SET statement_timeout = 0",
            "SET idle_in_transaction_session_timeout = 0",
            "SET bytea_output = 'hex'",
            "SET row_security = off",
        ] {
            assert!(run(s).contains("SET"), "should accept: {s}");
        }
        // SET then SHOW within one message (GUC state is per session/message).
        assert!(run("SET extra_float_digits = 2; SHOW extra_float_digits").contains('2'));
        assert!(run("SET lock_timeout = 5000; SHOW lock_timeout").contains("5000"));
        assert!(run("SET row_security = off; SHOW row_security").contains("off"));
        // Rejected loudly — never accepted-and-ignored.
        assert!(run("SET extra_float_digits = 9").contains("22023"), "out of range");
        // statement_timeout is now accepted (enforced at scan boundaries); a
        // malformed value is still rejected loudly.
        assert!(run("SET statement_timeout = 5000; SHOW statement_timeout").contains("5000"));
        assert!(run("SET statement_timeout = 'bogus'").contains("22023"), "bad timeout");
        assert!(run("SET bytea_output = 'escape'").contains("0A000"), "unsupported format");
    }

    #[test]
    fn cast_with_type_modifier() {
        let (mut e, mut b) = test_engine();
        let mut t = TxnState::new(&mut b, 256).unwrap();
        let mut run = |sql: &str| run_txn(&mut e, &mut b, &mut t, sql);
        // numeric cast rounds to scale
        assert!(run("SELECT 12.345::numeric(5,1)").contains("12.3"), "numeric scale");
        // varchar cast TRUNCATES (not error), unlike column assignment — matches PG
        assert!(run("SELECT 'hello'::varchar(3)").contains("hel"), "varchar truncate");
        // SQL-standard CAST(x AS type(mod)) form
        assert!(run("SELECT CAST(1.5 AS numeric(10,2))").contains("1.50"), "CAST form");
        // numeric precision overflow errors (22003)
        assert!(run("SELECT 123.45::numeric(3,1)").contains("22003"), "overflow");
        // a cast without a modifier still parses
        assert!(run("SELECT 5::int8").contains('5'), "plain cast");
    }

    #[test]
    fn set_show_transaction_and_show_all() {
        let (mut e, mut b) = test_engine();
        let mut t = TxnState::new(&mut b, 256).unwrap();
        let mut run = |sql: &str| run_txn(&mut e, &mut b, &mut t, sql);
        // Transaction-control SET forms that JDBC/tools send (one isolation
        // level, as BEGIN provides — the clause is acknowledged).
        assert!(run("SET TRANSACTION ISOLATION LEVEL SERIALIZABLE").contains("SET"));
        assert!(run("SET SESSION CHARACTERISTICS AS TRANSACTION ISOLATION LEVEL READ COMMITTED")
            .contains("SET"));
        assert!(run("SET TRANSACTION READ ONLY").contains("SET"));
        // SQL-standard multi-word SHOW forms.
        assert!(run("SHOW TRANSACTION ISOLATION LEVEL").contains("read committed"));
        assert!(run("SHOW ALL").contains("client_encoding"));
    }

    #[test]
    fn smallint_varchar_char_type_fidelity() {
        let (mut e, mut b) = test_engine();
        let mut t = TxnState::new(&mut b, 256).unwrap();
        let mut run = |sql: &str| run_txn(&mut e, &mut b, &mut t, sql);
        run("CREATE TABLE ty (s smallint, v varchar(3), c char(5))");
        // smallint enforces ±32767 — the previously-silent out-of-range case.
        assert!(run("INSERT INTO ty(s) VALUES (40000)").contains("smallint out of range"));
        assert!(run("INSERT INTO ty(s) VALUES (32767)").contains("INSERT"));
        assert!(run("SELECT s FROM ty WHERE s = 32767").contains("32767"), "round-trips");
        // varchar length errors; char(n) blank-pads to n.
        assert!(run("INSERT INTO ty(v) VALUES ('toolong')").contains("22001"));
        assert!(
            run("INSERT INTO ty(c) VALUES ('hi'); SELECT '[' || c || ']' FROM ty WHERE c IS NOT NULL")
                .contains("[hi   ]"),
            "char(5) pads to 5"
        );
    }

    #[test]
    fn join_using_clause() {
        let (mut e, mut b) = test_engine();
        let mut t = TxnState::new(&mut b, 256).unwrap();
        let mut run = |sql: &str| run_txn(&mut e, &mut b, &mut t, sql);
        run("CREATE TABLE a (id int, x text)");
        run("CREATE TABLE bb (id int, y text)");
        run("INSERT INTO a VALUES (1,'a1'),(2,'a2')");
        run("INSERT INTO bb VALUES (1,'b1'),(3,'b3')");
        // JOIN ... USING (id) is desugared to ON a.id = bb.id.
        let out = run("SELECT a.x, bb.y FROM a JOIN bb USING (id)");
        assert!(out.contains("a1") && out.contains("b1"), "match: {out}");
        assert!(!out.contains("a2") && !out.contains("b3"), "non-match dropped: {out}");
    }

    #[test]
    fn serial_auto_increment() {
        let (mut e, mut b) = test_engine();
        let mut t = TxnState::new(&mut b, 256).unwrap();
        let mut run = |sql: &str| run_txn(&mut e, &mut b, &mut t, sql);
        run("CREATE TABLE u (id serial PRIMARY KEY, name text)");
        // An omitted serial column is auto-assigned; RETURNING sees it.
        assert!(run("INSERT INTO u(name) VALUES ('a') RETURNING id").contains('1'));
        // A multi-row insert assigns increasing ids.
        let out = run("INSERT INTO u(name) VALUES ('b'),('c') RETURNING id");
        assert!(out.contains('2') && out.contains('3'), "sequential: {out}");
        // An explicit value is accounted for by the next auto value.
        run("INSERT INTO u VALUES (100, 'd')");
        assert!(run("INSERT INTO u(name) VALUES ('e') RETURNING id").contains("101"));
        assert!(run("SELECT count(*) FROM u").contains('5'));
    }

    #[test]
    fn on_conflict_do_nothing() {
        let (mut e, mut b) = test_engine();
        let mut t = TxnState::new(&mut b, 256).unwrap();
        let mut run = |sql: &str| run_txn(&mut e, &mut b, &mut t, sql);
        run("CREATE TABLE kv (k int PRIMARY KEY, v text)");
        run("INSERT INTO kv VALUES (1,'a'),(2,'b')");
        // The conflicting row is skipped, the new one inserted; the count
        // excludes skips (INSERT 0 1), matching PostgreSQL.
        assert!(run("INSERT INTO kv VALUES (1,'x'),(3,'c') ON CONFLICT DO NOTHING")
            .contains("INSERT 0 1"));
        let out = run("SELECT k, v FROM kv ORDER BY k");
        // k=1 keeps its original 'a' (the conflicting 'x' was skipped); k=3 added.
        assert!(out.contains("SELECT 3"), "three rows: {out}");
        assert!(out.contains('a') && out.contains('c') && !out.contains('x'), "kept original: {out}");
        // A fully-conflicting insert stores nothing.
        assert!(run("INSERT INTO kv VALUES (2,'y') ON CONFLICT (k) DO NOTHING")
            .contains("INSERT 0 0"));
        // DO UPDATE is a real upsert; assignments can reference the existing
        // row and excluded.<col> (the proposed row).
        run("INSERT INTO kv VALUES (1,'z') ON CONFLICT (k) DO UPDATE SET v = excluded.v");
        assert!(run("SELECT v FROM kv WHERE k = 1").contains('z'), "upserted");
        // DO UPDATE ... WHERE can veto the update.
        run("INSERT INTO kv VALUES (1,'q') ON CONFLICT (k) DO UPDATE SET v = 'q' WHERE FALSE");
        assert!(!run("SELECT v FROM kv WHERE k = 1").contains('q'), "WHERE vetoed");
    }

    #[test]
    fn multi_column_unique_and_primary_key() {
        // SQLSTATEs verified against PostgreSQL 18.4: duplicate multi-column key
        // is 23505; a NULL member makes the tuple distinct (no conflict).
        let (mut e, mut b) = test_engine();
        let mut t = TxnState::new(&mut b, 256).unwrap();
        let mut run = |sql: &str| run_txn(&mut e, &mut b, &mut t, sql);
        run("CREATE TABLE t (a int, b int, c text, PRIMARY KEY (a, b))");
        assert!(run("INSERT INTO t VALUES (1, 2, 'x')").contains("INSERT 0 1"));
        // Same (a,b) tuple conflicts; a different tuple is fine.
        assert!(run("INSERT INTO t VALUES (1, 2, 'y')").contains("23505"), "dup PK");
        assert!(run("INSERT INTO t VALUES (1, 3, 'y')").contains("INSERT 0 1"), "distinct");
        // A PRIMARY KEY column is NOT NULL.
        assert!(run("INSERT INTO t VALUES (NULL, 4, 'z')").contains("23502"), "PK not null");
        // Multi-column UNIQUE allows NULLs (distinct), rejects full duplicates.
        run("CREATE TABLE u (a int, b int, UNIQUE (a, b))");
        assert!(run("INSERT INTO u VALUES (1, NULL)").contains("INSERT 0 1"));
        assert!(run("INSERT INTO u VALUES (1, NULL)").contains("INSERT 0 1"), "NULL distinct");
        assert!(run("INSERT INTO u VALUES (5, 6)").contains("INSERT 0 1"));
        assert!(run("INSERT INTO u VALUES (5, 6)").contains("23505"), "dup UNIQUE");
    }

    #[test]
    fn check_constraints_enforced() {
        // 23514 on violation; NULL passes (three-valued logic) — matches PG 18.4.
        let (mut e, mut b) = test_engine();
        let mut t = TxnState::new(&mut b, 256).unwrap();
        let mut run = |sql: &str| run_txn(&mut e, &mut b, &mut t, sql);
        run("CREATE TABLE c (x int CHECK (x > 0), y int, CHECK (y < 100))");
        assert!(run("INSERT INTO c VALUES (5, 10)").contains("INSERT 0 1"));
        assert!(run("INSERT INTO c VALUES (-1, 10)").contains("23514"), "x>0 violated");
        assert!(run("INSERT INTO c VALUES (5, 200)").contains("23514"), "y<100 violated");
        // A NULL makes the predicate NULL, which passes.
        assert!(run("INSERT INTO c VALUES (NULL, 10)").contains("INSERT 0 1"), "null passes");
        // UPDATE is checked too.
        assert!(run("UPDATE c SET x = -5 WHERE x = 5").contains("23514"), "update checked");
        // A CHECK referencing an unknown column is rejected at creation (42703).
        assert!(run("CREATE TABLE bad (x int CHECK (nope > 0))").contains("42703"));
    }

    #[test]
    fn foreign_key_referential_integrity() {
        // All SQLSTATEs verified against PostgreSQL 18.4.
        let (mut e, mut b) = test_engine();
        let mut t = TxnState::new(&mut b, 256).unwrap();
        let mut run = |sql: &str| run_txn(&mut e, &mut b, &mut t, sql);
        run("CREATE TABLE p (id int PRIMARY KEY, name text)");
        run("CREATE TABLE ch (pid int REFERENCES p(id), note text)");
        // A referencing row with no parent is rejected (23503).
        assert!(run("INSERT INTO ch VALUES (5, 'orphan')").contains("23503"), "missing parent");
        // A NULL foreign key passes (MATCH SIMPLE).
        assert!(run("INSERT INTO ch VALUES (NULL, 'ok')").contains("INSERT 0 1"), "null fk");
        // With the parent present, the child inserts.
        run("INSERT INTO p VALUES (1, 'a')");
        assert!(run("INSERT INTO ch VALUES (1, 'child')").contains("INSERT 0 1"));
        // Deleting a referenced parent row is blocked (23503).
        assert!(run("DELETE FROM p WHERE id = 1").contains("23503"), "delete blocked");
        // Changing the referenced key of a referenced parent is blocked.
        assert!(run("UPDATE p SET id = 2 WHERE id = 1").contains("23503"), "key change blocked");
        // An unreferenced parent row can be deleted.
        run("INSERT INTO p VALUES (9, 'free')");
        assert!(run("DELETE FROM p WHERE id = 9").contains("DELETE 1"), "free delete");
        // A foreign key must reference a unique/PK column set (42830).
        run("CREATE TABLE nu (a int)");
        assert!(run("CREATE TABLE nc (a int REFERENCES nu(a))").contains("42830"), "non-unique");
        // Referencing a missing table is 42P01.
        assert!(run("CREATE TABLE nt (a int REFERENCES nope(a))").contains("42P01"), "missing tbl");
        // A cascade/set-null action is rejected loudly for now (0A000).
        assert!(run("CREATE TABLE cc (pid int REFERENCES p(id) ON DELETE CASCADE)")
            .contains("0A000"), "cascade rejected");
    }

    #[test]
    fn right_and_full_outer_joins() {
        // Expected rows verified against PostgreSQL 18.4.
        let (mut e, mut b) = test_engine();
        let mut t = TxnState::new(&mut b, 256).unwrap();
        run_txn(&mut e, &mut b, &mut t, "CREATE TABLE a (id int, x text)");
        run_txn(&mut e, &mut b, &mut t, "CREATE TABLE bt (id int, y text)");
        run_txn(&mut e, &mut b, &mut t, "INSERT INTO a VALUES (1,'a1'),(2,'a2'),(3,'a3')");
        run_txn(&mut e, &mut b, &mut t, "INSERT INTO bt VALUES (2,'b2'),(3,'b3'),(4,'b4')");
        // RIGHT JOIN preserves the right side; the unmatched b4 nulls a.x.
        let rows = data_rows(&run_with_txn_bytes(
            &mut e, &mut b, &mut t,
            "SELECT a.x FROM a RIGHT JOIN bt ON a.id=bt.id ORDER BY bt.id",
        ));
        assert_eq!(rows, ["a2", "a3", "NULL"], "right unmatched nulls left: {rows:?}");
        // FULL JOIN preserves both: unmatched a1 (left) and unmatched b4 (right).
        let full = data_rows(&run_with_txn_bytes(
            &mut e, &mut b, &mut t,
            "SELECT coalesce(a.x,'-'), coalesce(bt.y,'-') FROM a FULL JOIN bt ON a.id=bt.id ORDER BY a.id NULLS LAST, bt.id",
        ));
        assert_eq!(full, ["a1|-", "a2|b2", "a3|b3", "-|b4"], "full: {full:?}");
    }

    #[test]
    fn time_type() {
        // Output verified against PostgreSQL 18.4.
        let (mut e, mut b) = test_engine();
        let mut t = TxnState::new(&mut b, 256).unwrap();
        run_txn(&mut e, &mut b, &mut t, "CREATE TABLE t (id int, tm time)");
        run_txn(&mut e, &mut b, &mut t, "INSERT INTO t VALUES (1,'12:34:56'),(2,'09:00:00'),(3,'23:59:59.5')");
        let rows = data_rows(&run_with_txn_bytes(&mut e, &mut b, &mut t, "SELECT id, tm FROM t ORDER BY tm"));
        assert_eq!(rows, ["2|09:00:00", "1|12:34:56", "3|23:59:59.5"], "ordered: {rows:?}");
        // Casts: text -> time, and the time-of-day of a timestamp.
        assert!(run_txn(&mut e, &mut b, &mut t, "SELECT '08:30'::time").contains("08:30:00"));
        assert!(run_txn(&mut e, &mut b, &mut t,
            "SELECT '2024-01-15 14:30:00'::timestamp::time").contains("14:30:00"));
    }

    #[test]
    fn array_type() {
        // Output/operators verified against PostgreSQL 18.4.
        let (mut e, mut b) = test_engine();
        let mut t = TxnState::new(&mut b, 256).unwrap();
        run_txn(&mut e, &mut b, &mut t, "CREATE TABLE t (a int[])");
        run_txn(&mut e, &mut b, &mut t, "INSERT INTO t VALUES ('{1,2,3}'),(ARRAY[4,5])");
        // Literal output and storage roundtrip with ORDER BY (element-wise).
        let rows = data_rows(&run_with_txn_bytes(&mut e, &mut b, &mut t, "SELECT a FROM t ORDER BY a"));
        assert_eq!(rows, ["{1,2,3}", "{4,5}"], "array storage/order: {rows:?}");
        let mut run = |sql: &str| run_txn(&mut e, &mut b, &mut t, sql);
        assert!(run("SELECT ARRAY[1,2,3]").contains("{1,2,3}"));
        assert!(run("SELECT '{4,5,6}'::int[]").contains("{4,5,6}"));
        assert!(run("SELECT ARRAY['a','b']").contains("{a,b}"));
        assert!(run("SELECT '{x,y z}'::text[]").contains("{x,\"y z\"}"));
        // 1-based subscript, length/cardinality, and = ANY.
        assert!(run("SELECT (ARRAY[10,20,30])[2]").contains("20"));
        assert!(run("SELECT array_length(ARRAY[1,2,3],1)").contains('3'));
        assert!(run("SELECT cardinality(ARRAY[1,2,3])").contains('3'));
        assert!(run("SELECT 20 = ANY(ARRAY[10,20,30])").contains('t'));
        assert!(run("SELECT 99 = ANY(ARRAY[10,20,30])").contains('f'));
    }

    #[test]
    fn json_and_jsonb_types() {
        // Output/normalization/operators verified against PostgreSQL 18.4.
        let (mut e, mut b) = test_engine();
        let mut t = TxnState::new(&mut b, 256).unwrap();
        let mut run = |sql: &str| run_txn(&mut e, &mut b, &mut t, sql);
        // json is verbatim; jsonb normalizes (sorted keys, last-wins dedup,
        // canonical spacing and numbers).
        assert!(run("SELECT '{\"b\": 1,  \"a\":2, \"b\":3}'::json").contains("{\"b\": 1,  \"a\":2, \"b\":3}"));
        assert!(run("SELECT '{\"b\": 1,  \"a\":2, \"b\":3}'::jsonb").contains("{\"a\": 2, \"b\": 3}"));
        assert!(run("SELECT '[1, 2,   3]'::jsonb").contains("[1, 2, 3]"));
        assert!(run("SELECT '1e2'::jsonb").contains("100"));
        // -> keeps json/jsonb, ->> returns text; array index is 0-based.
        assert!(run("SELECT ('{\"a\":{\"x\":5},\"b\":[10,20]}'::jsonb)->'a'").contains("{\"x\": 5}"));
        assert!(run("SELECT ('{\"a\":5}'::jsonb)->>'a'").contains('5'));
        assert!(run("SELECT ('[10,20,30]'::jsonb)->1").contains("20"));
        // Invalid json is rejected loudly.
        assert!(run("SELECT '{bad}'::jsonb").contains("22P02"));
    }

    #[test]
    fn interval_type() {
        // Output/arithmetic verified against PostgreSQL 18.4.
        let (mut e, mut b) = test_engine();
        let mut t = TxnState::new(&mut b, 256).unwrap();
        let mut run = |sql: &str| run_txn(&mut e, &mut b, &mut t, sql);
        // Output formatting for the various field combinations.
        assert!(run("SELECT '1 day'::interval").contains("1 day"));
        assert!(run("SELECT '2 hours 30 minutes'::interval").contains("02:30:00"));
        assert!(run("SELECT '1 year 2 months'::interval").contains("1 year 2 mons"));
        assert!(run("SELECT '90 minutes'::interval").contains("01:30:00"));
        assert!(run("SELECT '-5 days'::interval").contains("-5 days"));
        // Arithmetic: date/timestamp + interval, month clamping.
        assert!(run("SELECT date '2024-01-15' + '1 day'::interval").contains("2024-01-16 00:00:00"));
        assert!(run("SELECT timestamp '2024-01-15 10:00' + '2 hours'::interval").contains("2024-01-15 12:00:00"));
        assert!(run("SELECT timestamp '2024-03-31 10:00' + '1 month'::interval").contains("2024-04-30 10:00:00"));
        // interval - interval.
        assert!(run("SELECT '1 day 2 hours'::interval - '3 hours'::interval").contains("1 day -01:00:00"));
    }

    #[test]
    fn correlated_subquery_over_aliased_table_and_values_setop() {
        // A correlated scalar subquery whose outer table is aliased must
        // describe/execute (regression: describe resolved the qualifier against
        // the table name, not the alias). And VALUES as a UNION branch.
        let (mut e, mut b) = test_engine();
        let mut t = TxnState::new(&mut b, 256).unwrap();
        let mut run = |sql: &str| run_txn(&mut e, &mut b, &mut t, sql);
        run("CREATE TABLE p (id int)");
        run("CREATE TABLE ch (pid int)");
        run("INSERT INTO p VALUES (1),(2)");
        run("INSERT INTO ch VALUES (1),(1),(2)");
        let rows = data_rows(&run_with_txn_bytes(&mut e, &mut b, &mut t,
            "SELECT x.id, (SELECT count(*) FROM ch WHERE ch.pid = x.id) FROM p x ORDER BY x.id"));
        assert_eq!(rows, ["1|2", "2|1"], "aliased correlated subquery: {rows:?}");
        // VALUES in a UNION branch.
        let vals = data_rows(&run_with_txn_bytes(&mut e, &mut b, &mut t,
            "SELECT 1 UNION ALL VALUES (2),(3) ORDER BY 1"));
        assert_eq!(vals, ["1", "2", "3"], "values in union: {vals:?}");
    }

    #[test]
    fn set_operations_in_subqueries() {
        // Set-operation queries in IN / scalar / derived-table / EXISTS position.
        // Semantics verified against PostgreSQL 18.4.
        let (mut e, mut b) = test_engine();
        let mut t = TxnState::new(&mut b, 256).unwrap();
        let mut run = |sql: &str| run_txn(&mut e, &mut b, &mut t, sql);
        // IN over a UNION ALL (with a VALUES branch).
        assert!(run("SELECT 42 WHERE 3 IN (SELECT 2 UNION ALL VALUES (3))").contains("42"));
        assert!(!run("SELECT 42 WHERE 9 IN (SELECT 2 UNION ALL VALUES (3))").contains("42"));
        // Scalar subquery collapsing a UNION to one row.
        assert!(run("SELECT (SELECT 5 UNION SELECT 5)").contains('5'));
        // Derived table over a UNION ALL.
        assert_eq!(
            data_rows(&run_with_txn_bytes(&mut e, &mut b, &mut t,
                "SELECT sum(x) FROM (SELECT 1 x UNION ALL SELECT 2 UNION ALL SELECT 3) t")),
            ["6"]
        );
        // EXISTS and INTERSECT / EXCEPT.
        assert!(run_txn(&mut e, &mut b, &mut t,
            "SELECT 9 WHERE EXISTS (SELECT 1 UNION ALL SELECT 2)").contains('9'));
        assert!(run_txn(&mut e, &mut b, &mut t,
            "SELECT 7 WHERE 2 IN (SELECT 2 INTERSECT SELECT 2)").contains('7'));
        assert!(!run_txn(&mut e, &mut b, &mut t,
            "SELECT 7 WHERE 2 IN (SELECT 2 EXCEPT SELECT 2)").contains('7'));
    }

    #[test]
    fn array_from_subquery_and_array_to_string() {
        // ARRAY(subquery) constructor and array_to_string, vs PostgreSQL 18.4.
        let (mut e, mut b) = test_engine();
        let mut t = TxnState::new(&mut b, 256).unwrap();
        run_txn(&mut e, &mut b, &mut t, "CREATE TABLE t (x int)");
        run_txn(&mut e, &mut b, &mut t, "INSERT INTO t VALUES (10),(20),(30)");
        // Elements follow the table's physical (insertion) scan order, matching
        // PostgreSQL. (ORDER BY inside a subquery is not yet honored — tracked
        // separately — so it is deliberately not exercised here.)
        assert_eq!(
            data_rows(&run_with_txn_bytes(&mut e, &mut b, &mut t,
                "SELECT array(SELECT x FROM t)")),
            ["{10,20,30}"]
        );
        // Empty subquery yields an empty array, not NULL.
        assert_eq!(
            data_rows(&run_with_txn_bytes(&mut e, &mut b, &mut t,
                "SELECT array(SELECT x FROM t WHERE x > 100)")),
            ["{}"]
        );
        // array_to_string joins, with and without a null replacement.
        assert!(run_txn(&mut e, &mut b, &mut t,
            "SELECT array_to_string(ARRAY[1,NULL,3], ',', '*')").contains("1,*,3"));
        assert!(run_txn(&mut e, &mut b, &mut t,
            "SELECT array_to_string(ARRAY[1,NULL,3], ',')").contains("1,3"));
    }

    #[test]
    fn generate_series_table_function() {
        // generate_series in FROM, vs PostgreSQL 18.4.
        let (mut e, mut b) = test_engine();
        let mut t = TxnState::new(&mut b, 256).unwrap();
        assert_eq!(
            data_rows(&run_with_txn_bytes(&mut e, &mut b, &mut t,
                "SELECT s FROM generate_series(0,3) s ORDER BY s")),
            ["0", "1", "2", "3"]
        );
        assert_eq!(
            data_rows(&run_with_txn_bytes(&mut e, &mut b, &mut t,
                "SELECT s FROM generate_series(1,10,2) s ORDER BY s")),
            ["1", "3", "5", "7", "9"]
        );
        assert_eq!(
            data_rows(&run_with_txn_bytes(&mut e, &mut b, &mut t,
                "SELECT s FROM generate_series(5,1,-2) s ORDER BY s DESC")),
            ["5", "3", "1"]
        );
        assert_eq!(
            data_rows(&run_with_txn_bytes(&mut e, &mut b, &mut t,
                "SELECT count(*) FROM generate_series(1,100) g")),
            ["100"]
        );
    }

    #[test]
    fn catalog_indexes_and_constraints_for_psql_d() {
        // The pg_class/pg_index/pg_constraint rows and pg_get_indexdef /
        // pg_get_constraintdef / oid::regclass that psql `\d <table>` reads,
        // verified against PostgreSQL 18.4's rendering.
        let (mut e, mut b) = test_engine();
        let mut t = TxnState::new(&mut b, 256).unwrap();
        let mut run = |sql: &str| run_txn(&mut e, &mut b, &mut t, sql);
        run("CREATE TABLE parent (a int, b int, PRIMARY KEY (a,b))");
        run("CREATE TABLE child (id int PRIMARY KEY, pa int, pb int, email text UNIQUE, \
             FOREIGN KEY (pa,pb) REFERENCES parent(a,b))");
        // Index relations exist with PostgreSQL-style names.
        let index = data_rows(&run_with_txn_bytes(&mut e, &mut b, &mut t,
            "SELECT relname FROM pg_class WHERE relkind = 'i' ORDER BY relname"));
        assert_eq!(index, ["child_email_key", "child_pkey", "parent_pkey"], "index rels: {index:?}");
        // pg_get_indexdef reconstructs the btree column list.
        let pk = run_txn(&mut e, &mut b, &mut t,
            "SELECT pg_get_indexdef(indexrelid, 0, true) FROM pg_index i \
             JOIN pg_class c ON c.oid = i.indexrelid WHERE c.relname = 'parent_pkey'");
        assert!(pk.contains("btree (a, b)"), "indexdef: {pk}");
        // The foreign key: constraint def + parent name via oid::regclass.
        let fk = data_rows(&run_with_txn_bytes(&mut e, &mut b, &mut t,
            "SELECT confrelid::regclass, pg_get_constraintdef(oid, true) \
             FROM pg_constraint WHERE contype = 'f'"));
        assert_eq!(fk, ["parent|FOREIGN KEY (pa, pb) REFERENCES parent(a, b)"], "fk: {fk:?}");
        // A UNIQUE constraint is backed by an index (conindid links them).
        let uq = data_rows(&run_with_txn_bytes(&mut e, &mut b, &mut t,
            "SELECT conname FROM pg_constraint WHERE contype = 'u' ORDER BY conname"));
        assert_eq!(uq, ["child_email_key"], "unique constraints: {uq:?}");
    }

    #[test]
    fn bitwise_operators_and_string_syntax() {
        // Bitwise operators and SQL trim/substring syntax used by JDBC's
        // DatabaseMetaData queries. Semantics verified against PostgreSQL 18.4.
        let (mut e, mut b) = test_engine();
        let mut t = TxnState::new(&mut b, 256).unwrap();
        let mut run = |sql: &str| run_txn(&mut e, &mut b, &mut t, sql);
        assert!(run("SELECT 6 & 3").contains('2'));
        assert!(run("SELECT 6 | 1").contains('7'));
        assert!(run("SELECT 6 # 3").contains('5'));
        assert!(run("SELECT 1 << 4").contains("16"));
        assert!(run("SELECT 32 >> 2").contains('8'));
        // `substring(str FROM start FOR len)` and `trim([dir] chars FROM str)`.
        assert!(run("SELECT substring('abcdef' FROM 2 FOR 3)").contains("bcd"));
        assert!(run("SELECT trim(both 'x' FROM 'xxhixx')").contains("hi"));
        assert!(run("SELECT trim(leading '0' FROM '007')").contains('7'));
    }

    #[test]
    fn expandarray_and_composite_field_access() {
        // `_pg_expandarray` (set-returning) + `(expression).n/.x` composite access,
        // driving JDBC getPrimaryKeys. A single-column PK expands to one row.
        let (mut e, mut b) = test_engine();
        let mut t = TxnState::new(&mut b, 256).unwrap();
        run_txn(&mut e, &mut b, &mut t, "CREATE TABLE pk1 (id int PRIMARY KEY, v text)");
        run_txn(&mut e, &mut b, &mut t,
            "CREATE TABLE pk2 (a int, b int, PRIMARY KEY (a, b))");
        // Single-column: one (x=1, n=1) row.
        let r1 = data_rows(&run_with_txn_bytes(&mut e, &mut b, &mut t,
            "SELECT (information_schema._pg_expandarray(i.indkey)).n AS seq, \
             (information_schema._pg_expandarray(i.indkey)).x AS att \
             FROM pg_index i JOIN pg_class c ON c.oid = i.indexrelid \
             WHERE c.relname = 'pk1_pkey'"));
        assert_eq!(r1, ["1|1"], "single-col expand: {r1:?}");
        // Two-column PK: the SRF expands into two rows (ordinals 1 and 2). Sort
        // in a wrapping subquery, as JDBC's getPrimaryKeys does.
        let mut r2 = data_rows(&run_with_txn_bytes(&mut e, &mut b, &mut t,
            "SELECT (information_schema._pg_expandarray(i.indkey)).n AS seq \
             FROM pg_index i JOIN pg_class c ON c.oid = i.indexrelid \
             WHERE c.relname = 'pk2_pkey'"));
        r2.sort();
        assert_eq!(r2, ["1", "2"], "multi-col expand: {r2:?}");
    }

    #[test]
    fn regex_match_operators_and_operator_syntax() {
        // Semantics verified against PostgreSQL 18.4.
        let (mut e, mut b) = test_engine();
        let mut t = TxnState::new(&mut b, 256).unwrap();
        run_txn(&mut e, &mut b, &mut t, "CREATE TABLE t (s text)");
        run_txn(&mut e, &mut b, &mut t, "INSERT INTO t VALUES ('pg_toast'),('public'),('pg_temp_1'),('foo')");
        // `~` and `!~` filter rows; `~*` is case-insensitive.
        assert_eq!(data_rows(&run_with_txn_bytes(&mut e, &mut b, &mut t,
            "SELECT s FROM t WHERE s ~ '^pg_' ORDER BY s")), ["pg_temp_1", "pg_toast"]);
        assert_eq!(data_rows(&run_with_txn_bytes(&mut e, &mut b, &mut t,
            "SELECT s FROM t WHERE s !~ '^pg_' ORDER BY s")), ["foo", "public"]);
        assert!(run_txn(&mut e, &mut b, &mut t, "SELECT 'ABC' ~* '^abc'").contains('t'));
        assert!(run_txn(&mut e, &mut b, &mut t, "SELECT 'ABC' ~ '^abc'").contains('f'));
        // Grouping + alternation, and the explicit OPERATOR(...) syntax psql
        // emits, plus COLLATE (accepted, default collation).
        assert_eq!(data_rows(&run_with_txn_bytes(&mut e, &mut b, &mut t,
            "SELECT s FROM t WHERE s OPERATOR(pg_catalog.~) '^(foo|public)$' COLLATE \"C\" ORDER BY s")),
            ["foo", "public"]);
    }

    #[test]
    fn window_functions() {
        // All outputs verified against PostgreSQL 18.4.
        let (mut e, mut b) = test_engine();
        let mut t = TxnState::new(&mut b, 256).unwrap();
        run_txn(&mut e, &mut b, &mut t, "CREATE TABLE s (dept text, name text, sal int)");
        run_txn(&mut e, &mut b, &mut t,
            "INSERT INTO s VALUES ('a','w1',100),('a','w2',200),('a','w3',200),('b','w4',50),('b','w5',75)");
        // row_number / rank / dense_rank with PARTITION BY + ORDER BY. Ranks
        // share for the tied 200/200 rows; row_number does not.
        let r = data_rows(&run_with_txn_bytes(&mut e, &mut b, &mut t,
            "SELECT dept, sal, row_number() OVER (PARTITION BY dept ORDER BY sal, name), rank() OVER (PARTITION BY dept ORDER BY sal), dense_rank() OVER (PARTITION BY dept ORDER BY sal) FROM s ORDER BY dept, sal, name"));
        assert_eq!(r, ["a|100|1|1|1", "a|200|2|2|2", "a|200|3|2|2", "b|50|1|1|1", "b|75|2|2|2"], "rankings: {r:?}");
        // Running sum (peers share) vs whole-partition sum.
        let s = data_rows(&run_with_txn_bytes(&mut e, &mut b, &mut t,
            "SELECT sal, sum(sal) OVER (PARTITION BY dept ORDER BY sal), sum(sal) OVER (PARTITION BY dept) FROM s ORDER BY dept, sal, name"));
        assert_eq!(s, ["100|100|500", "200|500|500", "200|500|500", "50|50|125", "75|125|125"], "sums: {s:?}");
        // lag / lead with a default.
        let l = data_rows(&run_with_txn_bytes(&mut e, &mut b, &mut t,
            "SELECT sal, lag(sal) OVER (ORDER BY sal), lead(sal,1,-1) OVER (ORDER BY sal) FROM s ORDER BY sal"));
        assert_eq!(l, ["50|NULL|75", "75|50|100", "100|75|200", "200|100|200", "200|200|-1"], "lag/lead: {l:?}");
    }

    #[test]
    fn savepoints_rollback_and_release() {
        // Behavior verified against PostgreSQL 18.4.
        let (mut e, mut b) = test_engine();
        let mut t = TxnState::new(&mut b, 256).unwrap();
        run_txn(&mut e, &mut b, &mut t, "CREATE TABLE t (id int, v text)");
        run_txn(&mut e, &mut b, &mut t, "BEGIN");
        run_txn(&mut e, &mut b, &mut t, "INSERT INTO t VALUES (1,'a')");
        run_txn(&mut e, &mut b, &mut t, "SAVEPOINT s1");
        // Modify row 1 (touched before AND after the savepoint) and add row 2.
        run_txn(&mut e, &mut b, &mut t, "UPDATE t SET v='b' WHERE id=1");
        run_txn(&mut e, &mut b, &mut t, "INSERT INTO t VALUES (2,'x')");
        // ROLLBACK TO restores row 1 to 'a' and removes row 2 — the reverse
        // replay reconstructs the savepoint-time image.
        assert!(run_txn(&mut e, &mut b, &mut t, "ROLLBACK TO SAVEPOINT s1").contains("ROLLBACK"));
        let rows = data_rows(&run_with_txn_bytes(&mut e, &mut b, &mut t, "SELECT id, v FROM t ORDER BY id"));
        assert_eq!(rows, ["1|a"], "rollback to savepoint: {rows:?}");
        run_txn(&mut e, &mut b, &mut t, "COMMIT");
        // RELEASE keeps the subtransaction's changes.
        run_txn(&mut e, &mut b, &mut t, "BEGIN");
        run_txn(&mut e, &mut b, &mut t, "INSERT INTO t VALUES (3,'c')");
        run_txn(&mut e, &mut b, &mut t, "SAVEPOINT s2");
        run_txn(&mut e, &mut b, &mut t, "INSERT INTO t VALUES (4,'d')");
        assert!(run_txn(&mut e, &mut b, &mut t, "RELEASE SAVEPOINT s2").contains("RELEASE"));
        run_txn(&mut e, &mut b, &mut t, "COMMIT");
        let all = data_rows(&run_with_txn_bytes(&mut e, &mut b, &mut t, "SELECT id FROM t ORDER BY id"));
        assert_eq!(all, ["1", "3", "4"], "release kept changes: {all:?}");
        // ROLLBACK TO recovers a failed subtransaction.
        run_txn(&mut e, &mut b, &mut t, "BEGIN");
        run_txn(&mut e, &mut b, &mut t, "SAVEPOINT s3");
        run_txn(&mut e, &mut b, &mut t, "SELECT 1/0");
        assert_eq!(t.status_byte(), b'E', "aborted after error");
        run_txn(&mut e, &mut b, &mut t, "ROLLBACK TO SAVEPOINT s3");
        assert_eq!(t.status_byte(), b'T', "recovered by rollback to savepoint");
        assert!(run_txn(&mut e, &mut b, &mut t, "SELECT 42").contains("42"), "works after recovery");
        run_txn(&mut e, &mut b, &mut t, "COMMIT");
        // A nonexistent savepoint errors 3B001.
        run_txn(&mut e, &mut b, &mut t, "BEGIN");
        assert!(run_txn(&mut e, &mut b, &mut t, "ROLLBACK TO SAVEPOINT nope").contains("3B001"));
        run_txn(&mut e, &mut b, &mut t, "ROLLBACK");
    }

    #[test]
    fn update_from_and_delete_using() {
        // Row images verified against PostgreSQL 18.4.
        let (mut e, mut b) = test_engine();
        let mut t = TxnState::new(&mut b, 256).unwrap();
        run_txn(&mut e, &mut b, &mut t, "CREATE TABLE t (id int, v int, label text)");
        run_txn(&mut e, &mut b, &mut t, "CREATE TABLE s (id int, delta int, lbl text)");
        run_txn(&mut e, &mut b, &mut t, "INSERT INTO t VALUES (1,10,'x'),(2,20,'y'),(3,30,'z')");
        run_txn(&mut e, &mut b, &mut t, "INSERT INTO s VALUES (1,100,'one'),(2,200,'two')");
        // UPDATE ... FROM: the SET may reference both target and source columns.
        assert!(run_txn(&mut e, &mut b, &mut t,
            "UPDATE t SET v = t.v + s.delta, label = s.lbl FROM s WHERE t.id = s.id")
            .contains("UPDATE 2"));
        let rows = data_rows(&run_with_txn_bytes(
            &mut e, &mut b, &mut t, "SELECT id, v, label FROM t ORDER BY id",
        ));
        assert_eq!(rows, ["1|110|one", "2|220|two", "3|30|z"], "update from: {rows:?}");
        // DELETE ... USING removes the joined target rows.
        run_txn(&mut e, &mut b, &mut t, "CREATE TABLE d (id int, v int)");
        run_txn(&mut e, &mut b, &mut t, "CREATE TABLE k (id int)");
        run_txn(&mut e, &mut b, &mut t, "INSERT INTO d VALUES (1,1),(2,2),(3,3)");
        run_txn(&mut e, &mut b, &mut t, "INSERT INTO k VALUES (2),(3)");
        assert!(run_txn(&mut e, &mut b, &mut t, "DELETE FROM d USING k WHERE d.id = k.id")
            .contains("DELETE 2"));
        let left = data_rows(&run_with_txn_bytes(
            &mut e, &mut b, &mut t, "SELECT id FROM d ORDER BY id",
        ));
        assert_eq!(left, ["1"], "delete using: {left:?}");
    }

    #[test]
    fn multiway_equijoin_prunes_early() {
        // A chained k-way equi-join must push each equality down to the level
        // where its tables are bound and prune doomed partial rows there.
        // Without that this is a naive O(N^k) nested loop that never returns;
        // with it the test completes in milliseconds. Counts verified against
        // PostgreSQL 18.4.
        let (mut e, mut b) = test_engine();
        let mut t = TxnState::new(&mut b, 256).unwrap();
        run_txn(&mut e, &mut b, &mut t, "CREATE TABLE t (id int, v int)");
        run_txn(&mut e, &mut b, &mut t,
            "INSERT INTO t SELECT g, g % 10 FROM generate_series(1, 80) g");
        // Six-way self-join chained on a unique key: N distinct chains.
        let rows = data_rows(&run_with_txn_bytes(&mut e, &mut b, &mut t,
            "SELECT count(*) FROM t a, t b, t c, t d, t e, t f \
             WHERE a.id=b.id AND b.id=c.id AND c.id=d.id AND d.id=e.id AND e.id=f.id"));
        assert_eq!(rows, ["80"], "6-way chained equi-join: {rows:?}");
        // A constant equality on a middle table prunes every chain but one.
        let rows = data_rows(&run_with_txn_bytes(&mut e, &mut b, &mut t,
            "SELECT count(*) FROM t a, t b, t c WHERE a.id=b.id AND b.id=c.id AND b.id=7"));
        assert_eq!(rows, ["1"], "constant-pruned join: {rows:?}");
        // Pushdown must not change results: the leaf still checks the full WHERE,
        // so a non-key predicate that only the leaf can evaluate still filters.
        let rows = data_rows(&run_with_txn_bytes(&mut e, &mut b, &mut t,
            "SELECT count(*) FROM t a, t b WHERE a.id=b.id AND a.v + b.v = 4"));
        assert_eq!(rows, ["8"], "leaf-checked predicate: {rows:?}");
    }

    #[test]
    fn named_timezone_dst_rendering() {
        let (mut e, mut b) = test_engine();
        let mut t = TxnState::new(&mut b, 256).unwrap();
        let mut run = |sql: &str| run_txn(&mut e, &mut b, &mut t, sql);
        // America/New_York: EST (-05) in winter, EDT (-04) in summer — DST honored.
        let out = run("SET timezone='America/New_York'; SELECT '2021-01-15 12:00:00+00'::timestamptz, '2021-07-15 12:00:00+00'::timestamptz");
        assert!(out.contains("07:00:00-05"), "winter EST: {out}");
        assert!(out.contains("08:00:00-04"), "summer EDT: {out}");
        // Southern hemisphere: DST in the local summer (January).
        let out = run("SET timezone='Australia/Sydney'; SELECT '2021-01-15 00:00:00+00'::timestamptz");
        assert!(out.contains("+11"), "AEDT: {out}");
        // An unknown zone is rejected loudly, not accepted.
        assert!(!run("SET timezone='Mars/Olympus'").contains("SET\0"), "unknown zone rejected");
    }

    #[test]
    fn commit_makes_writes_visible_and_durable() {
        let config = test_config("txn-durable");
        let mut b = Budget::new(1 << 24);
        {
            let mut e = Engine::new(&config, &mut b).unwrap();
            let mut t = TxnState::new(&mut b, 256).unwrap();
            run_txn(&mut e, &mut b, &mut t, "CREATE TABLE t (id int)");
            run_txn(&mut e, &mut b, &mut t, "BEGIN; INSERT INTO t VALUES (1); INSERT INTO t VALUES (2); COMMIT");
            run_txn(&mut e, &mut b, &mut t, "BEGIN; INSERT INTO t VALUES (3); ROLLBACK");
        }
        let mut b2 = Budget::new(1 << 24);
        let mut e = Engine::new(&config, &mut b2).unwrap();
        let mut t = TxnState::new(&mut b2, 256).unwrap();
        let out = run_txn(&mut e, &mut b2, &mut t, "SELECT id FROM t ORDER BY id");
        assert!(out.contains("SELECT 2"), "committed rows must replay: {out}");
        assert!(!out.contains('3'), "rolled-back row must not replay: {out}");
    }

    #[test]
    fn implicit_transaction_rolls_back_whole_message() {
        // B-001: an error in a multi-statement message undoes all of it.
        let (mut e, mut b) = test_engine();
        let mut t = TxnState::new(&mut b, 256).unwrap();
        run_txn(&mut e, &mut b, &mut t, "CREATE TABLE t (id int)");
        let out = run_txn(
            &mut e,
            &mut b,
            &mut t,
            "INSERT INTO t VALUES (1); INSERT INTO t VALUES (2); SELECT 1/0",
        );
        assert!(out.contains("22012"), "{out}");
        let out = run_txn(&mut e, &mut b, &mut t, "SELECT count(*) FROM t");
        assert!(out.contains("count") || out.contains('0'), "{out}");
        let rows = data_rows(&run_with_txn_bytes(&mut e, &mut b, &mut t, "SELECT count(*) FROM t"));
        assert_eq!(rows, ["0"], "inserts before the error must be undone");
    }

    fn run_with_txn_bytes(
        engine: &mut Engine,
        budget: &mut Budget,
        txn: &mut TxnState,
        sql_text: &str,
    ) -> Vec<u8> {
        let mut buffer = crate::mem::FixedBuf::new(budget, "send", 1 << 18).unwrap();
        let arena = Arena::new(budget, "sql", 1 << 18).unwrap();
        let mut pool = test_pool(budget);
        let mut guc = GucState::new();
        let mut resp = Responder::new(&mut buffer);
        engine
            .execute_simple(sql_text, &arena, txn, &mut pool, &mut guc, &mut resp)
            .unwrap();
        buffer.readable().to_vec()
    }

    #[test]
    fn failed_transaction_blocks_until_end() {
        let (mut e, mut b) = test_engine();
        let mut t = TxnState::new(&mut b, 256).unwrap();
        run_txn(&mut e, &mut b, &mut t, "CREATE TABLE t (id int)");
        run_txn(&mut e, &mut b, &mut t, "BEGIN");
        run_txn(&mut e, &mut b, &mut t, "INSERT INTO t VALUES (1)");
        let out = run_txn(&mut e, &mut b, &mut t, "SELECT 1/0");
        assert!(out.contains("22012"), "{out}");
        assert_eq!(t.status_byte(), b'E');
        let out = run_txn(&mut e, &mut b, &mut t, "SELECT 1");
        assert!(out.contains("25P02"), "{out}");
        // COMMIT of a failed txn reports ROLLBACK and undoes the insert.
        let out = run_txn(&mut e, &mut b, &mut t, "COMMIT");
        assert!(out.contains("ROLLBACK"), "{out}");
        assert_eq!(t.status_byte(), b'I');
        let rows = data_rows(&run_with_txn_bytes(&mut e, &mut b, &mut t, "SELECT count(*) FROM t"));
        assert_eq!(rows, ["0"]);
    }

    #[test]
    fn isolation_and_write_conflicts() {
        let (mut e, mut b) = test_engine();
        let mut alice = TxnState::new(&mut b, 256).unwrap();
        let mut bob = TxnState::new(&mut b, 256).unwrap();
        run_txn(&mut e, &mut b, &mut alice, "CREATE TABLE t (id int, v text)");
        run_txn(&mut e, &mut b, &mut alice, "INSERT INTO t VALUES (1,'base')");

        run_txn(&mut e, &mut b, &mut alice, "BEGIN");
        run_txn(&mut e, &mut b, &mut alice, "UPDATE t SET v = 'alice' WHERE id = 1");
        run_txn(&mut e, &mut b, &mut alice, "INSERT INTO t VALUES (2,'alice-new')");

        // Bob sees only committed state.
        let rows = data_rows(&run_with_txn_bytes(&mut e, &mut b, &mut bob, "SELECT v FROM t ORDER BY id"));
        assert_eq!(rows, ["base"], "uncommitted changes must be invisible");

        // Bob's write to Alice's row conflicts immediately.
        let out = run_txn(&mut e, &mut b, &mut bob, "UPDATE t SET v = 'bob' WHERE id = 1");
        assert!(out.contains("40001"), "{out}");

        run_txn(&mut e, &mut b, &mut alice, "COMMIT");
        let rows = data_rows(&run_with_txn_bytes(&mut e, &mut b, &mut bob, "SELECT v FROM t ORDER BY id"));
        assert_eq!(rows, ["alice", "alice-new"]);
    }

    #[test]
    fn ddl_rolls_back_with_implicit_transaction() {
        let (mut e, mut b) = test_engine();
        let mut t = TxnState::new(&mut b, 256).unwrap();
        let out = run_txn(
            &mut e,
            &mut b,
            &mut t,
            "CREATE TABLE brand_new (id int); INSERT INTO brand_new VALUES (1); SELECT 1/0",
        );
        assert!(out.contains("22012"), "{out}");
        let out = run_txn(&mut e, &mut b, &mut t, "SELECT * FROM brand_new");
        assert!(out.contains("42P01"), "created table must be rolled back: {out}");
        // DDL inside explicit blocks is transactional.
        run_txn(&mut e, &mut b, &mut t, "BEGIN");
        run_txn(&mut e, &mut b, &mut t, "CREATE TABLE txn_ddl (id int)");
        run_txn(&mut e, &mut b, &mut t, "INSERT INTO txn_ddl VALUES (1)");
        run_txn(&mut e, &mut b, &mut t, "ROLLBACK");
        let out = run_txn(&mut e, &mut b, &mut t, "SELECT * FROM txn_ddl");
        assert!(out.contains("42P01"), "{out}");
        run_txn(&mut e, &mut b, &mut t, "BEGIN");
        run_txn(&mut e, &mut b, &mut t, "CREATE TABLE txn_ddl (id int)");
        run_txn(&mut e, &mut b, &mut t, "COMMIT");
        let out = run_txn(&mut e, &mut b, &mut t, "SELECT count(*) FROM txn_ddl");
        assert!(out.contains("count"), "{out}");
        // DROP rolls back too: the table and its rows survive.
        run_txn(&mut e, &mut b, &mut t, "INSERT INTO txn_ddl VALUES (7)");
        run_txn(&mut e, &mut b, &mut t, "BEGIN");
        run_txn(&mut e, &mut b, &mut t, "DROP TABLE txn_ddl");
        run_txn(&mut e, &mut b, &mut t, "ROLLBACK");
        let rows = data_rows(&run_with_txn_bytes(&mut e, &mut b, &mut t, "SELECT id FROM txn_ddl"));
        assert_eq!(rows, ["7"], "dropped table must revive with its rows");
        // CHECKPOINT stays outside transaction blocks.
        run_txn(&mut e, &mut b, &mut t, "BEGIN");
        let out = run_txn(&mut e, &mut b, &mut t, "CHECKPOINT");
        assert!(out.contains("0A000") || out.contains("25001"), "{out}");
        run_txn(&mut e, &mut b, &mut t, "ROLLBACK");
    }

    #[test]
    fn data_survives_engine_restart() {
        let config = test_config("restart");
        {
            let mut budget = Budget::new(1 << 24);
            let mut e = Engine::new(&config, &mut budget).unwrap();
            run_with(&mut e, &mut budget, "CREATE TABLE t (id int, v text)");
            run_with(&mut e, &mut budget, "INSERT INTO t VALUES (1,'a'),(2,'b'),(3,'c')");
            run_with(&mut e, &mut budget, "UPDATE t SET v = 'B' WHERE id = 2");
            run_with(&mut e, &mut budget, "DELETE FROM t WHERE id = 3");
            run_with(&mut e, &mut budget, "CREATE TABLE gone (x int)");
            run_with(&mut e, &mut budget, "DROP TABLE gone");
            e.commit_wal();
        }
        let mut budget = Budget::new(1 << 24);
        let mut e = Engine::new(&config, &mut budget).unwrap();
        let bytes = run_with(&mut e, &mut budget, "SELECT id, v FROM t ORDER BY id");
        assert_eq!(data_rows(&bytes), ["1|a", "2|B"]);
        let bytes = run_with(&mut e, &mut budget, "SELECT * FROM gone");
        assert!(String::from_utf8_lossy(&bytes).contains("42P01"));
        // New rowids do not collide with replayed ones.
        run_with(&mut e, &mut budget, "INSERT INTO t VALUES (4,'d')");
        let bytes = run_with(&mut e, &mut budget, "SELECT id FROM t ORDER BY id");
        assert_eq!(data_rows(&bytes), ["1", "2", "4"]);
    }

    #[test]
    fn indexes_survive_restart() {
        // Indexes (and their UNIQUE constraint) are journaled and survive a
        // WAL-replay restart.
        let config = test_config("idx_restart");
        {
            let mut budget = Budget::new(1 << 24);
            let mut e = Engine::new(&config, &mut budget).unwrap();
            run_with(&mut e, &mut budget, "CREATE TABLE t (a int, b int)");
            run_with(&mut e, &mut budget, "INSERT INTO t VALUES (1,1),(1,2)");
            run_with(&mut e, &mut budget, "CREATE UNIQUE INDEX u ON t(a,b)");
            e.commit_wal();
        }
        let mut budget = Budget::new(1 << 24);
        let mut e = Engine::new(&config, &mut budget).unwrap();
        // The UNIQUE index survived: a conflicting insert is rejected.
        assert!(String::from_utf8_lossy(&run_with(&mut e, &mut budget, "INSERT INTO t VALUES (1,1)"))
            .contains("23505"));
        // A non-conflicting insert works.
        let out =
            String::from_utf8_lossy(&run_with(&mut e, &mut budget, "INSERT INTO t VALUES (3,3)")).to_string();
        assert!(!out.contains("23505"), "{out}");
    }

    #[test]
    fn views_survive_restart() {
        // View definitions are journaled, so they survive a WAL-replay restart.
        let config = test_config("view_restart");
        {
            let mut budget = Budget::new(1 << 24);
            let mut e = Engine::new(&config, &mut budget).unwrap();
            run_with(&mut e, &mut budget, "CREATE TABLE t (id int, v int)");
            run_with(&mut e, &mut budget, "INSERT INTO t VALUES (1,10),(2,20),(3,30)");
            run_with(&mut e, &mut budget, "CREATE VIEW big AS SELECT id FROM t WHERE v > 15");
            run_with(&mut e, &mut budget, "CREATE VIEW gone AS SELECT 1");
            run_with(&mut e, &mut budget, "DROP VIEW gone");
            e.commit_wal();
        }
        let mut budget = Budget::new(1 << 24);
        let mut e = Engine::new(&config, &mut budget).unwrap();
        // The surviving view still expands and queries.
        assert_eq!(
            data_rows(&run_with(&mut e, &mut budget, "SELECT id FROM big ORDER BY id")),
            ["2", "3"]
        );
        // The dropped view is gone.
        assert!(String::from_utf8_lossy(&run_with(&mut e, &mut budget, "SELECT * FROM gone"))
            .contains("42P01"));
    }

    #[test]
    fn sql_surface_batch() {
        let (mut e, mut b) = test_engine();
        run_with(&mut e, &mut b, "CREATE TABLE s (id int, name text DEFAULT 'x', qty int DEFAULT 3)");
        let bytes = run_with(&mut e, &mut b, "INSERT INTO s (id) VALUES (1), (2) RETURNING id, name, qty");
        assert_eq!(data_rows(&bytes), ["1|x|3", "2|x|3"]);
        run_with(&mut e, &mut b, "INSERT INTO s VALUES (3, DEFAULT, 9), (4, 'y', 1)");

        let bytes = run_with(&mut e, &mut b, "SELECT id FROM s WHERE id IN (2,4) ORDER BY 1");
        assert_eq!(data_rows(&bytes), ["2", "4"]);
        let bytes = run_with(&mut e, &mut b, "SELECT id FROM s WHERE qty BETWEEN 2 AND 5 ORDER BY id");
        assert_eq!(data_rows(&bytes), ["1", "2"]);
        let bytes = run_with(&mut e, &mut b, "SELECT DISTINCT name FROM s ORDER BY name");
        assert_eq!(data_rows(&bytes), ["x", "y"]);
        let bytes = run_with(&mut e, &mut b, "SELECT id FROM s ORDER BY id OFFSET 1 LIMIT 2");
        assert_eq!(data_rows(&bytes), ["2", "3"]);
        let bytes = run_with(
            &mut e,
            &mut b,
            "SELECT CASE WHEN qty > 5 THEN 'hi' ELSE 'lo' END FROM s ORDER BY id",
        );
        assert_eq!(data_rows(&bytes), ["lo", "lo", "hi", "lo"]);
        let bytes = run_with(&mut e, &mut b, "SELECT name FROM s WHERE name LIKE '_' AND name NOT LIKE 'x' ORDER BY id LIMIT 1");
        assert_eq!(data_rows(&bytes), ["y"]);
        let bytes = run_with(&mut e, &mut b, "UPDATE s SET qty = 0 WHERE id = 4 RETURNING qty");
        assert_eq!(data_rows(&bytes), ["0"]);
        let bytes = run_with(&mut e, &mut b, "DELETE FROM s WHERE id = 1 RETURNING name");
        assert_eq!(data_rows(&bytes), ["x"]);

        run_with(&mut e, &mut b, "ALTER TABLE s ADD COLUMN price float8 DEFAULT 1.5");
        run_with(&mut e, &mut b, "ALTER TABLE s RENAME COLUMN name TO title");
        run_with(&mut e, &mut b, "ALTER TABLE s DROP COLUMN qty");
        run_with(&mut e, &mut b, "ALTER TABLE s RENAME TO stock");
        let bytes = run_with(&mut e, &mut b, "SELECT id, title, price FROM stock ORDER BY id");
        assert_eq!(data_rows(&bytes), ["2|x|1.5", "3|x|1.5", "4|y|1.5"]);

        // The pool is per-connection; one message keeps one pool here.
        let bytes = run_with(
            &mut e,
            &mut b,
            "PREPARE q (int) AS SELECT title FROM stock WHERE id = $1; EXECUTE q(4); \
             DEALLOCATE q; EXECUTE q(4)",
        );
        let text = String::from_utf8_lossy(&bytes).to_string();
        assert_eq!(data_rows(&bytes), ["y"], "{text}");
        assert!(text.contains("26000"), "{text}");
    }

    #[test]
    fn altered_table_survives_restart() {
        let config = test_config("alter-durable");
        {
            let mut b = Budget::new(1 << 24);
            let mut e = Engine::new(&config, &mut b).unwrap();
            run_with(&mut e, &mut b, "CREATE TABLE a (id int, v text)");
            run_with(&mut e, &mut b, "INSERT INTO a VALUES (1, 'one')");
            run_with(&mut e, &mut b, "ALTER TABLE a ADD COLUMN n int DEFAULT 42");
            run_with(&mut e, &mut b, "ALTER TABLE a RENAME TO b");
        }
        let mut b = Budget::new(1 << 24);
        let mut e = Engine::new(&config, &mut b).unwrap();
        let bytes = run_with(&mut e, &mut b, "SELECT id, v, n FROM b");
        assert_eq!(data_rows(&bytes), ["1|one|42"]);
    }

    #[test]
    fn joins_group_by_subqueries() {
        let (mut e, mut b) = test_engine();
        run_with(&mut e, &mut b, "CREATE TABLE d (id int, name text)");
        run_with(&mut e, &mut b, "CREATE TABLE emp (id int, did int, name text, pay int)");
        run_with(&mut e, &mut b, "INSERT INTO d VALUES (1,'eng'),(2,'ops'),(3,'none')");
        run_with(
            &mut e,
            &mut b,
            "INSERT INTO emp VALUES (1,1,'ada',120),(2,1,'bob',100),(3,2,'cyd',90),(4,NULL,'dee',80)",
        );

        let bytes = run_with(
            &mut e,
            &mut b,
            "SELECT e.name, d.name FROM emp e JOIN d ON e.did = d.id ORDER BY e.id",
        );
        assert_eq!(data_rows(&bytes), ["ada|eng", "bob|eng", "cyd|ops"]);

        let bytes = run_with(
            &mut e,
            &mut b,
            "SELECT e.name, d.name FROM emp e LEFT JOIN d ON e.did = d.id ORDER BY e.id",
        );
        assert_eq!(data_rows(&bytes), ["ada|eng", "bob|eng", "cyd|ops", "dee|NULL"]);

        let bytes = run_with(&mut e, &mut b, "SELECT count(*) FROM emp, d");
        assert_eq!(data_rows(&bytes), ["12"]);

        let bytes = run_with(
            &mut e,
            &mut b,
            "SELECT d.name, count(*), sum(e.pay) FROM emp e JOIN d ON e.did = d.id \
             GROUP BY d.name HAVING count(*) > 1",
        );
        assert_eq!(data_rows(&bytes), ["eng|2|220"]);

        let bytes = run_with(
            &mut e,
            &mut b,
            "SELECT name FROM emp WHERE pay > (SELECT avg(pay) FROM emp) ORDER BY name",
        );
        assert_eq!(data_rows(&bytes), ["ada", "bob"]);

        let bytes = run_with(
            &mut e,
            &mut b,
            "SELECT name FROM d WHERE id IN (SELECT did FROM emp) ORDER BY name",
        );
        assert_eq!(data_rows(&bytes), ["eng", "ops"]);
        // NOT IN with a NULL member yields no rows (SQL three-valued logic).
        let bytes = run_with(
            &mut e,
            &mut b,
            "SELECT name FROM d WHERE id NOT IN (SELECT did FROM emp) ORDER BY name",
        );
        assert_eq!(data_rows(&bytes), Vec::<String>::new());

        // UPDATE with an IN-subquery.
        run_with(
            &mut e,
            &mut b,
            "UPDATE emp SET pay = 0 WHERE did IN (SELECT id FROM d WHERE name = 'ops')",
        );
        let bytes = run_with(&mut e, &mut b, "SELECT pay FROM emp WHERE name = 'cyd'");
        assert_eq!(data_rows(&bytes), ["0"]);

        // Ambiguity and qualification errors.
        let bytes = run_with(
            &mut e,
            &mut b,
            "SELECT name FROM emp e JOIN d ON e.did = d.id",
        );
        assert!(String::from_utf8_lossy(&bytes).contains("42702"));
    }

    #[test]
    fn datetime_uuid_bytea_types() {
        let config = test_config("types-durable");
        {
            let mut b = Budget::new(1 << 24);
            let mut e = Engine::new(&config, &mut b).unwrap();
            run_with(&mut e, &mut b, "CREATE TABLE ev (d date, t timestamptz, u uuid, raw bytea)");
            run_with(
                &mut e,
                &mut b,
                "INSERT INTO ev VALUES ('2024-02-29', '2024-02-29 12:00:00+02', \
                 'a0eebc99-9c0b-4ef8-bb6d-6bb9bd380a11', '\\xdeadbeef')",
            );
            let bytes = run_with(&mut e, &mut b, "SELECT d, t, u, raw FROM ev");
            assert_eq!(
                data_rows(&bytes),
                ["2024-02-29|2024-02-29 10:00:00+00|a0eebc99-9c0b-4ef8-bb6d-6bb9bd380a11|\\xdeadbeef"]
            );
            let bytes = run_with(&mut e, &mut b, "SELECT count(*) FROM ev WHERE d = '2024-02-29' AND t < '2025-01-01'");
            assert_eq!(data_rows(&bytes), ["1"]);
        }
        // Types survive WAL replay.
        let mut b = Budget::new(1 << 24);
        let mut e = Engine::new(&config, &mut b).unwrap();
        let bytes = run_with(&mut e, &mut b, "SELECT u FROM ev");
        assert_eq!(data_rows(&bytes), ["a0eebc99-9c0b-4ef8-bb6d-6bb9bd380a11"]);
        let bytes = run_with(&mut e, &mut b, "SELECT 'bad-uuid'::uuid");
        assert!(String::from_utf8_lossy(&bytes).contains("22P02"));
    }

    #[test]
    fn drop_table_frees_the_name() {
        let (mut e, mut b) = test_engine();
        run_with(&mut e, &mut b, "CREATE TABLE t (id int)");
        run_with(&mut e, &mut b, "DROP TABLE t");
        let bytes = run_with(&mut e, &mut b, "CREATE TABLE t (id int)");
        assert_eq!(message_types(&bytes), [b'C']);
        let bytes = run_with(&mut e, &mut b, "DROP TABLE IF EXISTS never_was");
        assert_eq!(message_types(&bytes), [b'N', b'C']);
    }

    #[test]
    fn correlated_scalar_subquery_in_projection() {
        let (mut e, mut b) = test_engine();
        run_with(&mut e, &mut b, "CREATE TABLE t (a int, b int)");
        run_with(&mut e, &mut b, "INSERT INTO t VALUES (1,10),(2,20),(3,30)");
        // For each row, count rows with a smaller b (a running rank).
        let bytes = run_with(
            &mut e,
            &mut b,
            "SELECT a, (SELECT count(*) FROM t AS x WHERE x.b < t.b) AS rnk FROM t ORDER BY a",
        );
        assert_eq!(data_rows(&bytes), ["1|0", "2|1", "3|2"]);
    }

    #[test]
    fn correlated_scalar_subquery_streaming() {
        let (mut e, mut b) = test_engine();
        run_with(&mut e, &mut b, "CREATE TABLE t (a int, b int)");
        run_with(&mut e, &mut b, "INSERT INTO t VALUES (1,10),(2,20)");
        // No ORDER BY: streaming path (scan order is unspecified, so compare
        // as a set).
        let bytes = run_with(
            &mut e,
            &mut b,
            "SELECT a, (SELECT count(*) FROM t AS x WHERE x.b <= t.b) FROM t",
        );
        let mut got = data_rows(&bytes);
        got.sort();
        assert_eq!(got, ["1|1", "2|2"]);
    }

    #[test]
    fn exists_correlated_in_where() {
        let (mut e, mut b) = test_engine();
        run_with(&mut e, &mut b, "CREATE TABLE t (a int)");
        run_with(&mut e, &mut b, "CREATE TABLE u (k int)");
        run_with(&mut e, &mut b, "INSERT INTO t VALUES (1),(2),(3)");
        run_with(&mut e, &mut b, "INSERT INTO u VALUES (2),(3),(4)");
        let bytes = run_with(
            &mut e,
            &mut b,
            "SELECT a FROM t WHERE EXISTS (SELECT 1 FROM u WHERE u.k = t.a) ORDER BY a",
        );
        assert_eq!(data_rows(&bytes), ["2", "3"]);
        let bytes = run_with(
            &mut e,
            &mut b,
            "SELECT a FROM t WHERE NOT EXISTS (SELECT 1 FROM u WHERE u.k = t.a) ORDER BY a",
        );
        assert_eq!(data_rows(&bytes), ["1"]);
    }

    #[test]
    fn exists_uncorrelated() {
        let (mut e, mut b) = test_engine();
        run_with(&mut e, &mut b, "CREATE TABLE t (a int)");
        run_with(&mut e, &mut b, "CREATE TABLE u (k int)");
        run_with(&mut e, &mut b, "INSERT INTO t VALUES (1),(2)");
        // u empty: EXISTS is false for all rows, NOT EXISTS true for all.
        let bytes = run_with(&mut e, &mut b, "SELECT a FROM t WHERE EXISTS (SELECT 1 FROM u)");
        assert_eq!(data_rows(&bytes), Vec::<String>::new());
        run_with(&mut e, &mut b, "INSERT INTO u VALUES (9)");
        let bytes =
            run_with(&mut e, &mut b, "SELECT a FROM t WHERE EXISTS (SELECT 1 FROM u) ORDER BY a");
        assert_eq!(data_rows(&bytes), ["1", "2"]);
    }

    #[test]
    fn fromless_select_with_subquery() {
        let (mut e, mut b) = test_engine();
        run_with(&mut e, &mut b, "CREATE TABLE t1 (x int)");
        run_with(&mut e, &mut b, "INSERT INTO t1 VALUES (1),(2),(3)");
        // IN-subquery with SELECT * (single column) in a FROM-less SELECT.
        let bytes = run_with(&mut e, &mut b, "SELECT 1 IN (SELECT * FROM t1)");
        assert_eq!(data_rows(&bytes), ["t"]);
        let bytes = run_with(&mut e, &mut b, "SELECT 9 IN (SELECT * FROM t1)");
        assert_eq!(data_rows(&bytes), ["f"]);
        // Scalar subquery in a FROM-less SELECT.
        let bytes = run_with(&mut e, &mut b, "SELECT (SELECT count(*) FROM t1) AS c");
        assert_eq!(data_rows(&bytes), ["3"]);
        // EXISTS in a FROM-less SELECT.
        let bytes = run_with(&mut e, &mut b, "SELECT EXISTS (SELECT 1 FROM t1 WHERE x > 2)");
        assert_eq!(data_rows(&bytes), ["t"]);
    }

    #[test]
    fn in_subquery_empty_and_null_semantics() {
        let (mut e, mut b) = test_engine();
        run_with(&mut e, &mut b, "CREATE TABLE empt (x int)");
        run_with(&mut e, &mut b, "CREATE TABLE nn (x int)");
        run_with(&mut e, &mut b, "INSERT INTO nn VALUES (NULL)");
        // Over an empty set, IN is FALSE and NOT IN is TRUE even for NULL.
        assert_eq!(data_rows(&run_with(&mut e, &mut b, "SELECT 1 IN (SELECT * FROM empt)")), ["f"]);
        assert_eq!(
            data_rows(&run_with(&mut e, &mut b, "SELECT NULL IN (SELECT * FROM empt)")),
            ["f"]
        );
        assert_eq!(
            data_rows(&run_with(&mut e, &mut b, "SELECT NULL NOT IN (SELECT * FROM empt)")),
            ["t"]
        );
        // A NULL operand against a non-empty set is unknown (NULL).
        assert_eq!(
            data_rows(&run_with(&mut e, &mut b, "SELECT NULL IN (SELECT * FROM nn)")),
            ["NULL"]
        );
        // A value absent from a set that contains NULL is unknown (NULL).
        assert_eq!(
            data_rows(&run_with(&mut e, &mut b, "SELECT 1 IN (SELECT * FROM nn)")),
            ["NULL"]
        );
    }

    #[test]
    fn in_subquery_operand_type_check() {
        let (mut e, mut b) = test_engine();
        run_with(&mut e, &mut b, "CREATE TABLE ti (x int)");
        // A string literal that cannot become the column type errors even over
        // an empty set, as PostgreSQL does (invalid_text_representation).
        let bytes = run_with(&mut e, &mut b, "SELECT 'hello' IN (SELECT * FROM ti)");
        assert!(String::from_utf8_lossy(&bytes).contains("22P02"), "{:?}", String::from_utf8_lossy(&bytes));
        // A numeric string still coerces fine and is simply not present.
        run_with(&mut e, &mut b, "INSERT INTO ti VALUES (NULL)");
        let bytes = run_with(&mut e, &mut b, "SELECT 'hello' NOT IN (SELECT * FROM ti)");
        assert!(String::from_utf8_lossy(&bytes).contains("22P02"));
    }

    #[test]
    fn subquery_wildcard_multi_column_errors() {
        let (mut e, mut b) = test_engine();
        run_with(&mut e, &mut b, "CREATE TABLE t2 (a int, b int)");
        run_with(&mut e, &mut b, "INSERT INTO t2 VALUES (1,2)");
        // SELECT * over a two-column source is not a scalar/IN subquery.
        let bytes = run_with(&mut e, &mut b, "SELECT 1 IN (SELECT * FROM t2)");
        let text = String::from_utf8_lossy(&bytes);
        assert!(text.contains("42601"), "{text}");
    }

    #[test]
    fn scalar_functions() {
        let (mut e, mut b) = test_engine();
        let r = |e: &mut Engine, b: &mut Budget, sql: &str| data_rows(&run_with(e, b, sql));
        assert_eq!(r(&mut e, &mut b, "SELECT trim('  hi  ')"), ["hi"]);
        assert_eq!(r(&mut e, &mut b, "SELECT ltrim('xxhi', 'x')"), ["hi"]);
        assert_eq!(r(&mut e, &mut b, "SELECT rtrim('hixx', 'x')"), ["hi"]);
        assert_eq!(r(&mut e, &mut b, "SELECT substr('hello', 2, 3)"), ["ell"]);
        assert_eq!(r(&mut e, &mut b, "SELECT substr('hello', 2)"), ["ello"]);
        assert_eq!(r(&mut e, &mut b, "SELECT substr('hello', -1, 3)"), ["h"]);
        assert_eq!(r(&mut e, &mut b, "SELECT replace('a-b-c', '-', '+')"), ["a+b+c"]);
        assert_eq!(r(&mut e, &mut b, "SELECT repeat('ab', 3)"), ["ababab"]);
        assert_eq!(r(&mut e, &mut b, "SELECT reverse('abc')"), ["cba"]);
        assert_eq!(r(&mut e, &mut b, "SELECT left('hello', 3)"), ["hel"]);
        assert_eq!(r(&mut e, &mut b, "SELECT left('hello', -2)"), ["hel"]);
        assert_eq!(r(&mut e, &mut b, "SELECT right('hello', 3)"), ["llo"]);
        assert_eq!(r(&mut e, &mut b, "SELECT right('hello', -2)"), ["llo"]);
        assert_eq!(r(&mut e, &mut b, "SELECT strpos('hello', 'll')"), ["3"]);
        assert_eq!(r(&mut e, &mut b, "SELECT strpos('hello', 'z')"), ["0"]);
        assert_eq!(r(&mut e, &mut b, "SELECT concat('a', NULL, 'b', 1)"), ["ab1"]);
        assert_eq!(r(&mut e, &mut b, "SELECT concat_ws(',', 'a', NULL, 'b')"), ["a,b"]);
        assert_eq!(r(&mut e, &mut b, "SELECT initcap('hello world')"), ["Hello World"]);
        assert_eq!(r(&mut e, &mut b, "SELECT ascii('A')"), ["65"]);
        assert_eq!(r(&mut e, &mut b, "SELECT chr(65)"), ["A"]);
        assert_eq!(r(&mut e, &mut b, "SELECT octet_length('héllo')"), ["6"]);
        assert_eq!(r(&mut e, &mut b, "SELECT greatest(3, 1, 2)"), ["3"]);
        assert_eq!(r(&mut e, &mut b, "SELECT least(3, 1, 2)"), ["1"]);
        assert_eq!(r(&mut e, &mut b, "SELECT nullif(5, 5)"), ["NULL"]);
        assert_eq!(r(&mut e, &mut b, "SELECT nullif(5, 6)"), ["5"]);
    }

    #[test]
    fn padding_and_split_functions() {
        let (mut e, mut b) = test_engine();
        let r = |e: &mut Engine, b: &mut Budget, sql: &str| data_rows(&run_with(e, b, sql));
        assert_eq!(r(&mut e, &mut b, "SELECT lpad('hi', 5)"), ["   hi"]);
        assert_eq!(r(&mut e, &mut b, "SELECT lpad('hi', 5, 'ab')"), ["abahi"]);
        assert_eq!(r(&mut e, &mut b, "SELECT lpad('hello', 3)"), ["hel"]);
        assert_eq!(r(&mut e, &mut b, "SELECT rpad('hi', 5, '*')"), ["hi***"]);
        assert_eq!(r(&mut e, &mut b, "SELECT split_part('a,b,c', ',', 2)"), ["b"]);
        assert_eq!(r(&mut e, &mut b, "SELECT split_part('a,b,c', ',', -1)"), ["c"]);
        assert_eq!(r(&mut e, &mut b, "SELECT split_part('a,b,c', ',', 5)"), [""]);
        assert_eq!(r(&mut e, &mut b, "SELECT translate('hello', 'el', 'ip')"), ["hippo"]);
        assert_eq!(r(&mut e, &mut b, "SELECT translate('hello', 'l', '')"), ["heo"]);
    }

    #[test]
    fn bool_aggregates() {
        let (mut e, mut b) = test_engine();
        run_with(&mut e, &mut b, "CREATE TABLE t (g int, flag bool)");
        run_with(&mut e, &mut b, "INSERT INTO t VALUES (1,true),(1,true),(2,true),(2,false),(3,NULL)");
        assert_eq!(
            data_rows(&run_with(&mut e, &mut b, "SELECT g, bool_and(flag), bool_or(flag) FROM t GROUP BY g ORDER BY g")),
            ["1|t|t", "2|f|t", "3|NULL|NULL"]
        );
        // Whole-table aggregate + `every` alias for bool_and.
        assert_eq!(data_rows(&run_with(&mut e, &mut b, "SELECT bool_or(flag), every(flag) FROM t")), ["t|f"]);
    }

    #[test]
    fn create_index_and_unique() {
        // Validated against PostgreSQL 18.4.
        let (mut e, mut b) = test_engine();
        run_with(&mut e, &mut b, "CREATE TABLE t (a int, b int, c int)");
        run_with(&mut e, &mut b, "INSERT INTO t VALUES (1,1,10),(1,2,20),(2,1,30)");
        // A non-unique index: succeeds, results unchanged (no acceleration).
        assert!(String::from_utf8_lossy(&run_with(&mut e, &mut b, "CREATE INDEX i1 ON t(c)"))
            .contains("CREATE INDEX"));
        assert_eq!(
            data_rows(&run_with(&mut e, &mut b, "SELECT a,b,c FROM t ORDER BY a,b")),
            ["1|1|10", "1|2|20", "2|1|30"]
        );
        // Duplicate index name errors; unknown column errors.
        assert!(String::from_utf8_lossy(&run_with(&mut e, &mut b, "CREATE INDEX i1 ON t(a)"))
            .contains("42P07"));
        assert!(String::from_utf8_lossy(&run_with(&mut e, &mut b, "CREATE INDEX i2 ON t(nope)"))
            .contains("42703"));
        // A composite UNIQUE index over non-duplicate data succeeds and then
        // enforces the constraint on inserts.
        run_with(&mut e, &mut b, "CREATE UNIQUE INDEX u1 ON t(a,b)");
        assert!(String::from_utf8_lossy(&run_with(&mut e, &mut b, "INSERT INTO t VALUES (1,1,99)"))
            .contains("23505"));
        // A distinct (a,b) tuple is fine.
        run_with(&mut e, &mut b, "INSERT INTO t VALUES (2,2,40)");
        // NULLs in a unique index do not conflict (SQL semantics).
        run_with(&mut e, &mut b, "INSERT INTO t VALUES (NULL,1,1),(NULL,1,2)");
        // CREATE UNIQUE INDEX over duplicate existing rows fails.
        run_with(&mut e, &mut b, "CREATE TABLE d (x int)");
        run_with(&mut e, &mut b, "INSERT INTO d VALUES (5),(5)");
        assert!(String::from_utf8_lossy(&run_with(&mut e, &mut b, "CREATE UNIQUE INDEX ud ON d(x)"))
            .contains("23505"));
        // DROP INDEX removes the constraint: the once-conflicting insert works.
        run_with(&mut e, &mut b, "DROP INDEX u1");
        let out = String::from_utf8_lossy(&run_with(&mut e, &mut b, "INSERT INTO t VALUES (1,1,7)"))
            .to_string();
        assert!(!out.contains("23505"), "constraint should be gone: {out}");
    }

    #[test]
    fn updatable_view_dml() {
        // DML on an auto-updatable view rewrites to the base table (PG 18.4).
        let (mut e, mut b) = test_engine();
        run_with(&mut e, &mut b, "CREATE TABLE t1 (x int, y text)");
        run_with(&mut e, &mut b, "INSERT INTO t1 VALUES (1,'a'),(2,'b'),(3,'c'),(-1,'neg')");
        run_with(&mut e, &mut b, "CREATE VIEW v AS SELECT x FROM t1 WHERE x>0");
        run_with(&mut e, &mut b, "DELETE FROM v WHERE x=2");
        run_with(&mut e, &mut b, "UPDATE v SET x=5 WHERE x=1");
        run_with(&mut e, &mut b, "INSERT INTO v VALUES (9)");
        assert_eq!(
            data_rows(&run_with(&mut e, &mut b, "SELECT x,y FROM t1 ORDER BY x")),
            ["-1|neg", "3|c", "5|a", "9|NULL"]
        );
        // Too many values for the view's exposed columns errors like PG.
        assert!(String::from_utf8_lossy(&run_with(&mut e, &mut b, "INSERT INTO v VALUES (2,'z')"))
            .contains("42601"));
        // The view itself still reads correctly (base filtered).
        assert_eq!(
            data_rows(&run_with(&mut e, &mut b, "SELECT x FROM v ORDER BY x")),
            ["3", "5", "9"]
        );
    }

    #[test]
    fn where_error_safe_conjuncts_first() {
        // PostgreSQL's qual order is unspecified/cost-driven, so a filtering
        // condition can run before an error-prone one; we match by evaluating
        // error-safe conjuncts first. Validated against PG 18.4.
        let (mut e, mut b) = test_engine();
        run_with(&mut e, &mut b, "CREATE TABLE t (x int)");
        run_with(&mut e, &mut b, "INSERT INTO t VALUES (1),(2),(0),(3)");
        // The x=0 row is filtered by x>0 before 100/x evaluates — no error.
        assert_eq!(
            data_rows(&run_with(&mut e, &mut b,
                "SELECT x FROM t WHERE 100/x>10 AND x>0 ORDER BY x")),
            ["1", "2", "3"]
        );
        // Order of the conjuncts does not matter.
        assert_eq!(
            data_rows(&run_with(&mut e, &mut b,
                "SELECT x FROM t WHERE x<>0 AND 100/x>=33 ORDER BY x")),
            ["1", "2", "3"]
        );
        // With no filtering conjunct, the error still surfaces (as in PG).
        assert!(String::from_utf8_lossy(&run_with(&mut e, &mut b, "SELECT x FROM t WHERE 100/x>10"))
            .contains("22012"));
    }

    #[test]
    fn transactional_ddl_rollback() {
        // View/index DDL is rolled back with the transaction (PG semantics).
        let (mut e, mut b) = test_engine();
        run_with(&mut e, &mut b, "CREATE TABLE t (a int, c int)");
        run_with(&mut e, &mut b, "INSERT INTO t VALUES (1,10),(2,20)");
        // CREATE VIEW rolled back → the view is gone.
        run_with(&mut e, &mut b, "BEGIN; CREATE VIEW v AS SELECT a FROM t; ROLLBACK");
        assert!(String::from_utf8_lossy(&run_with(&mut e, &mut b, "SELECT * FROM v"))
            .contains("42P01"));
        // CREATE VIEW committed → persists; DROP VIEW rolled back → survives.
        run_with(&mut e, &mut b, "BEGIN; CREATE VIEW v AS SELECT a FROM t; COMMIT");
        run_with(&mut e, &mut b, "BEGIN; DROP VIEW v; ROLLBACK");
        assert_eq!(data_rows(&run_with(&mut e, &mut b, "SELECT a FROM v ORDER BY a")), ["1", "2"]);
        // CREATE OR REPLACE rolled back → the original definition is restored.
        run_with(&mut e, &mut b,
            "BEGIN; CREATE OR REPLACE VIEW v AS SELECT a FROM t WHERE a>1; ROLLBACK");
        assert_eq!(data_rows(&run_with(&mut e, &mut b, "SELECT a FROM v ORDER BY a")), ["1", "2"]);
        // CREATE UNIQUE INDEX rolled back → the constraint is gone.
        run_with(&mut e, &mut b, "BEGIN; CREATE UNIQUE INDEX u ON t(a); ROLLBACK");
        let out = String::from_utf8_lossy(&run_with(&mut e, &mut b, "INSERT INTO t VALUES (1,99)"))
            .to_string();
        assert!(!out.contains("23505"), "index constraint should be gone: {out}");
        // DROP TABLE rolled back → the table and its UNIQUE index both revive.
        run_with(&mut e, &mut b, "CREATE TABLE u2 (k int)");
        run_with(&mut e, &mut b, "INSERT INTO u2 VALUES (1),(2)");
        run_with(&mut e, &mut b, "CREATE UNIQUE INDEX uk ON u2(k)");
        run_with(&mut e, &mut b, "BEGIN; DROP TABLE u2; ROLLBACK");
        assert!(String::from_utf8_lossy(&run_with(&mut e, &mut b, "INSERT INTO u2 VALUES (1)"))
            .contains("23505"));
    }

    #[test]
    fn catalog_joins_and_subqueries() {
        // Joins/subqueries across catalog relations (B-007). Validated vs PG 18.4.
        let (mut e, mut b) = test_engine();
        run_with(&mut e, &mut b, "CREATE TABLE demo (a int, b text)");
        // pg_class JOIN pg_attribute on oid = attrelid.
        assert_eq!(
            data_rows(&run_with(&mut e, &mut b,
                "SELECT c.relname, a.attname FROM pg_class c \
                 JOIN pg_attribute a ON a.attrelid = c.oid \
                 WHERE c.relname='demo' AND a.attnum > 0 ORDER BY a.attnum")),
            ["demo|a", "demo|b"]
        );
        // A catalog relation inside a subquery.
        assert_eq!(
            data_rows(&run_with(&mut e, &mut b,
                "SELECT count(*) FROM pg_attribute \
                 WHERE attrelid IN (SELECT oid FROM pg_class WHERE relname='demo') AND attnum>0")),
            ["2"]
        );
    }

    #[test]
    fn create_view_basic() {
        // Values validated against PostgreSQL 18.4.
        let (mut e, mut b) = test_engine();
        run_with(&mut e, &mut b, "CREATE TABLE t (id int, v int)");
        run_with(&mut e, &mut b, "INSERT INTO t VALUES (1,10),(2,20),(3,30),(4,40)");
        run_with(&mut e, &mut b, "CREATE VIEW big AS SELECT id, v FROM t WHERE v > 15");
        // Query the view.
        assert_eq!(
            data_rows(&run_with(&mut e, &mut b, "SELECT id, v FROM big ORDER BY id")),
            ["2|20", "3|30", "4|40"]
        );
        // Aggregate over the view.
        assert_eq!(data_rows(&run_with(&mut e, &mut b, "SELECT sum(v) FROM big")), ["90"]);
        // A view over a view.
        run_with(&mut e, &mut b, "CREATE VIEW big2 AS SELECT id FROM big WHERE v > 25");
        assert_eq!(
            data_rows(&run_with(&mut e, &mut b, "SELECT id FROM big2 ORDER BY id")),
            ["3", "4"]
        );
        // Duplicate view name errors; OR REPLACE succeeds.
        assert!(String::from_utf8_lossy(&run_with(&mut e, &mut b, "CREATE VIEW big AS SELECT 1"))
            .contains("42P07"));
        run_with(&mut e, &mut b, "CREATE OR REPLACE VIEW big AS SELECT id FROM t WHERE id = 1");
        assert_eq!(data_rows(&run_with(&mut e, &mut b, "SELECT id FROM big")), ["1"]);
        // DROP VIEW; then querying it errors.
        run_with(&mut e, &mut b, "DROP VIEW big2");
        assert!(String::from_utf8_lossy(&run_with(&mut e, &mut b, "SELECT * FROM big2"))
            .contains("42P01"));
    }

    #[test]
    fn distinct_aggregates() {
        // Values validated against PostgreSQL 18.4.
        let (mut e, mut b) = test_engine();
        run_with(&mut e, &mut b, "CREATE TABLE t (g int, x int)");
        run_with(&mut e, &mut b, "INSERT INTO t VALUES (1,10),(1,10),(1,20),(2,5),(2,NULL),(3,NULL)");
        // Per group: DISTINCT drops duplicate 10 in group 1; NULLs never count.
        assert_eq!(
            data_rows(&run_with(&mut e, &mut b,
                "SELECT g, count(distinct x), sum(distinct x), min(distinct x), max(distinct x) \
                 FROM t GROUP BY g ORDER BY g")),
            ["1|2|30|10|20", "2|1|5|5|5", "3|0|NULL|NULL|NULL"]
        );
        // Whole-table: distinct set {10,20,5}, plus non-distinct for contrast.
        assert_eq!(
            data_rows(&run_with(&mut e, &mut b,
                "SELECT count(distinct x), sum(distinct x), count(x), count(*) FROM t")),
            ["3|35|4|6"]
        );
        // All-NULL input: count(DISTINCT) is 0, not NULL.
        assert_eq!(
            data_rows(&run_with(&mut e, &mut b, "SELECT count(distinct x) FROM t WHERE x IS NULL")),
            ["0"]
        );
        // avg(DISTINCT int) -> numeric with PG's 16-digit scale.
        assert_eq!(
            data_rows(&run_with(&mut e, &mut b, "SELECT avg(distinct x) FROM t WHERE g = 1")),
            ["15.0000000000000000"]
        );
        // DISTINCT outside an aggregate is rejected loudly.
        assert!(String::from_utf8_lossy(&run_with(&mut e, &mut b, "SELECT length(distinct 'x')"))
            .contains("42883"));
    }

    #[test]
    fn more_scalar_functions() {
        // Values + types validated against PostgreSQL 18.4.
        let (mut e, mut b) = test_engine();
        let r = |e: &mut Engine, b: &mut Budget, sql: &str| data_rows(&run_with(e, b, sql));
        assert_eq!(r(&mut e, &mut b, "SELECT to_hex(255)"), ["ff"]);
        assert_eq!(r(&mut e, &mut b, "SELECT to_hex(4096)"), ["1000"]);
        assert_eq!(r(&mut e, &mut b, "SELECT to_hex(-1)"), ["ffffffff"]); // two's complement
        assert_eq!(r(&mut e, &mut b, "SELECT gcd(12, 18)"), ["6"]);
        assert_eq!(r(&mut e, &mut b, "SELECT gcd(0, 0)"), ["0"]);
        assert_eq!(r(&mut e, &mut b, "SELECT lcm(4, 6)"), ["12"]);
        assert_eq!(r(&mut e, &mut b, "SELECT lcm(0, 5)"), ["0"]);
        assert_eq!(r(&mut e, &mut b, "SELECT bit_length('abc')"), ["24"]);
        assert_eq!(r(&mut e, &mut b, "SELECT md5('abc')"), ["900150983cd24fb0d6963f7d28e17f72"]);
        assert_eq!(
            r(&mut e, &mut b, "SELECT md5('The quick brown fox jumps over the lazy dog')"),
            ["9e107d9d372bb6826bd81d3542a419d6"]
        );
        assert_eq!(r(&mut e, &mut b, "SELECT starts_with('foobar', 'foo')"), ["t"]);
        assert_eq!(r(&mut e, &mut b, "SELECT starts_with('foobar', 'bar')"), ["f"]);
        assert_eq!(r(&mut e, &mut b, "SELECT cbrt(27)"), ["3"]);
        assert_eq!(r(&mut e, &mut b, "SELECT factorial(0)"), ["1"]);
        assert_eq!(r(&mut e, &mut b, "SELECT factorial(5)"), ["120"]);
        assert_eq!(r(&mut e, &mut b, "SELECT factorial(20)"), ["2432902008176640000"]);
        assert!(String::from_utf8_lossy(&run_with(&mut e, &mut b, "SELECT factorial(-1)"))
            .contains("22003"));
        // lcm overflow errors (22003).
        assert!(String::from_utf8_lossy(&run_with(&mut e, &mut b,
            "SELECT lcm(1000000000000000000, 999999999999999999)")).contains("22003"));
    }

    #[test]
    fn trig_and_rounding_functions() {
        // Values + types validated against PostgreSQL 18.4.
        let (mut e, mut b) = test_engine();
        let r = |e: &mut Engine, b: &mut Budget, sql: &str| data_rows(&run_with(e, b, sql));
        assert_eq!(r(&mut e, &mut b, "SELECT pi()"), ["3.141592653589793"]);
        assert_eq!(r(&mut e, &mut b, "SELECT degrees(pi())"), ["180"]);
        assert_eq!(r(&mut e, &mut b, "SELECT sin(0)"), ["0"]);
        assert_eq!(r(&mut e, &mut b, "SELECT cos(0)"), ["1"]);
        assert_eq!(r(&mut e, &mut b, "SELECT cosh(0)"), ["1"]);
        assert_eq!(r(&mut e, &mut b, "SELECT tanh(0)"), ["0"]);
        // Transcendental results differ in the last bits across platform libms
        // (as PostgreSQL's own float8 output does), so compare with tolerance.
        let approx = |e: &mut Engine, b: &mut Budget, sql: &str, want: f64| {
            let got: f64 = data_rows(&run_with(e, b, sql))[0].parse().expect("float output");
            assert!((got - want).abs() < 1e-12, "{sql}: got {got}, want {want}");
        };
        approx(&mut e, &mut b, "SELECT sinh(1)", 1.175_201_193_643_801_4);
        approx(&mut e, &mut b, "SELECT cot(1)", 0.642_092_615_934_330_8);
        // trunc(x, n) truncates toward zero to n decimals (numeric).
        assert_eq!(r(&mut e, &mut b, "SELECT trunc(1.2345, 2)"), ["1.23"]);
        assert_eq!(r(&mut e, &mut b, "SELECT trunc(1.9999, 2)"), ["1.99"]);
        assert_eq!(r(&mut e, &mut b, "SELECT trunc(-1.2999, 1)"), ["-1.2"]);
    }

    #[test]
    fn ordered_and_distinct_row_sources() {
        // DISTINCT / ORDER BY / LIMIT inside a derived table or CTE must be
        // honored (top-N, dedup), not dropped. Validated against PG 18.4.
        let (mut e, mut b) = test_engine();
        run_with(&mut e, &mut b, "CREATE TABLE t (v int)");
        run_with(&mut e, &mut b, "INSERT INTO t VALUES (5),(3),(1),(4),(2),(3),(1)");
        // ORDER BY ... LIMIT inside a derived table (top-3 smallest).
        assert_eq!(
            data_rows(&run_with(&mut e, &mut b,
                "SELECT s.v FROM (SELECT v FROM t ORDER BY v LIMIT 3) s ORDER BY s.v")),
            ["1", "1", "2"]
        );
        // DISTINCT inside a derived table.
        assert_eq!(
            data_rows(&run_with(&mut e, &mut b,
                "SELECT s.v FROM (SELECT DISTINCT v FROM t) s ORDER BY s.v")),
            ["1", "2", "3", "4", "5"]
        );
        // DISTINCT + ORDER BY + LIMIT inside a CTE.
        assert_eq!(
            data_rows(&run_with(&mut e, &mut b,
                "WITH c AS (SELECT DISTINCT v FROM t ORDER BY v LIMIT 2) SELECT v FROM c ORDER BY v")),
            ["1", "2"]
        );
        // A SELECT DISTINCT set-operation branch.
        assert_eq!(
            data_rows(&run_with(&mut e, &mut b,
                "SELECT DISTINCT v FROM t UNION SELECT 9 ORDER BY 1")),
            ["1", "2", "3", "4", "5", "9"]
        );
    }

    #[test]
    fn grouped_row_sources() {
        // GROUP BY / aggregates as a row source: derived tables, CTEs, set-op
        // branches, and INSERT ... SELECT. Values validated against PG 18.4.
        let (mut e, mut b) = test_engine();
        run_with(&mut e, &mut b, "CREATE TABLE t (g int, v int)");
        run_with(&mut e, &mut b, "INSERT INTO t VALUES (1,10),(1,20),(2,30),(2,40),(3,50)");
        // Derived table over a grouped subquery.
        assert_eq!(
            data_rows(&run_with(&mut e, &mut b,
                "SELECT s.g, s.total FROM (SELECT g, sum(v) AS total FROM t GROUP BY g) s \
                 ORDER BY s.g")),
            ["1|30", "2|70", "3|50"]
        );
        // CTE over a grouped query.
        assert_eq!(
            data_rows(&run_with(&mut e, &mut b,
                "WITH gs AS (SELECT g, count(*) AS c FROM t GROUP BY g) \
                 SELECT g, c FROM gs ORDER BY g")),
            ["1|2", "2|2", "3|1"]
        );
        // Set-operation branch with an aggregate.
        assert_eq!(
            data_rows(&run_with(&mut e, &mut b, "SELECT count(*) FROM t UNION SELECT 1 ORDER BY 1")),
            ["1", "5"]
        );
        // INSERT ... SELECT with GROUP BY.
        run_with(&mut e, &mut b, "CREATE TABLE dst (g int, total int)");
        run_with(&mut e, &mut b, "INSERT INTO dst SELECT g, sum(v) FROM t GROUP BY g");
        assert_eq!(
            data_rows(&run_with(&mut e, &mut b, "SELECT g, total FROM dst ORDER BY g")),
            ["1|30", "2|70", "3|50"]
        );
    }

    #[test]
    fn common_table_expressions() {
        // Values validated against PostgreSQL 18.4.
        let (mut e, mut b) = test_engine();
        run_with(&mut e, &mut b, "CREATE TABLE t (id int, v int)");
        run_with(&mut e, &mut b, "INSERT INTO t VALUES (1,10),(2,20),(3,30),(4,40)");
        // Single CTE referenced in the main query.
        assert_eq!(
            data_rows(&run_with(&mut e, &mut b,
                "WITH big AS (SELECT id, v FROM t WHERE v > 15) SELECT id, v FROM big ORDER BY id")),
            ["2|20", "3|30", "4|40"]
        );
        // Aggregate over a CTE.
        assert_eq!(
            data_rows(&run_with(&mut e, &mut b,
                "WITH big AS (SELECT id, v FROM t WHERE v > 15) SELECT sum(v) FROM big")),
            ["90"]
        );
        // A CTE that references an earlier CTE.
        assert_eq!(
            data_rows(&run_with(&mut e, &mut b,
                "WITH a AS (SELECT id, v FROM t), b AS (SELECT id, v*2 AS w FROM a WHERE v > 20) \
                 SELECT id, w FROM b ORDER BY id")),
            ["3|60", "4|80"]
        );
        // A CTE referenced inside a subquery.
        assert_eq!(
            data_rows(&run_with(&mut e, &mut b,
                "WITH big AS (SELECT id FROM t WHERE v > 25) \
                 SELECT count(*) FROM t WHERE id IN (SELECT id FROM big)")),
            ["2"]
        );
        // A CTE joined against a physical table.
        assert_eq!(
            data_rows(&run_with(&mut e, &mut b,
                "WITH j AS (SELECT id, v FROM t WHERE v >= 30) \
                 SELECT t.id, j.v FROM t JOIN j ON t.id = j.id ORDER BY t.id")),
            ["3|30", "4|40"]
        );
        // WITH RECURSIVE is rejected loudly.
        assert!(String::from_utf8_lossy(&run_with(
            &mut e,
            &mut b,
            "WITH RECURSIVE r AS (SELECT 1) SELECT * FROM r"
        ))
        .contains("42601"));
    }

    #[test]
    fn derived_tables() {
        // Values validated against PostgreSQL 18.4.
        let (mut e, mut b) = test_engine();
        run_with(&mut e, &mut b, "CREATE TABLE t (id int, v int)");
        run_with(&mut e, &mut b, "INSERT INTO t VALUES (1,10),(2,20),(3,30),(4,40)");
        // Simple derived table with a WHERE inside and outside.
        assert_eq!(
            data_rows(&run_with(&mut e, &mut b,
                "SELECT s.id, s.v FROM (SELECT id, v FROM t WHERE v > 15) s ORDER BY s.id")),
            ["2|20", "3|30", "4|40"]
        );
        // Aggregate over a derived table.
        assert_eq!(
            data_rows(&run_with(&mut e, &mut b,
                "SELECT sum(s.v) FROM (SELECT id, v FROM t WHERE v > 15) s")),
            ["90"]
        );
        // Computed column with an alias, filtered by the outer query.
        assert_eq!(
            data_rows(&run_with(&mut e, &mut b,
                "SELECT s.id, s.doubled FROM (SELECT id, v*2 AS doubled FROM t) s \
                 WHERE s.doubled > 40 ORDER BY s.id")),
            ["3|60", "4|80"]
        );
        // Join a physical table against a derived table.
        assert_eq!(
            data_rows(&run_with(&mut e, &mut b,
                "SELECT a.id, b.v FROM t a JOIN (SELECT id, v FROM t WHERE v > 25) b \
                 ON a.id = b.id ORDER BY a.id")),
            ["3|30", "4|40"]
        );
        // A derived table must have an alias.
        assert!(String::from_utf8_lossy(&run_with(&mut e, &mut b, "SELECT * FROM (SELECT 1)"))
            .contains("42601"));
        // A derived table as a set-operation branch (exercises describe_leaf).
        assert_eq!(
            data_rows(&run_with(&mut e, &mut b,
                "SELECT 1 UNION SELECT * FROM (SELECT 2) s ORDER BY 1")),
            ["1", "2"]
        );
        // Derived tables also work inside EXISTS / IN / scalar subqueries.
        assert_eq!(
            data_rows(&run_with(&mut e, &mut b,
                "SELECT 1 WHERE EXISTS (SELECT 1 FROM (SELECT id FROM t WHERE v > 25) s)")),
            ["1"]
        );
        assert_eq!(
            data_rows(&run_with(&mut e, &mut b,
                "SELECT id FROM t WHERE v IN (SELECT s.v FROM (SELECT v FROM t WHERE v > 25) s) \
                 ORDER BY id")),
            ["3", "4"]
        );
        assert_eq!(
            data_rows(&run_with(&mut e, &mut b,
                "SELECT (SELECT max(s.v) FROM (SELECT v FROM t) s)")),
            ["40"]
        );
    }

    #[test]
    fn date_arithmetic() {
        // Values validated against PostgreSQL 18.4.
        let (mut e, mut b) = test_engine();
        let r = |e: &mut Engine, b: &mut Budget, sql: &str| data_rows(&run_with(e, b, sql));
        assert_eq!(r(&mut e, &mut b, "SELECT date '2024-01-10' + 5"), ["2024-01-15"]);
        assert_eq!(r(&mut e, &mut b, "SELECT date '2024-01-10' - 5"), ["2024-01-05"]);
        assert_eq!(r(&mut e, &mut b, "SELECT 5 + date '2024-01-10'"), ["2024-01-15"]);
        // date - date -> integer days.
        assert_eq!(r(&mut e, &mut b, "SELECT date '2024-03-01' - date '2024-01-01'"), ["60"]);
        // Crossing a month boundary and a leap day.
        assert_eq!(r(&mut e, &mut b, "SELECT date '2024-02-28' + 1"), ["2024-02-29"]);
        // int - date is not defined in PostgreSQL.
        assert!(String::from_utf8_lossy(&run_with(&mut e, &mut b, "SELECT 5 - date '2024-01-10'"))
            .contains("42883"));
    }

    #[test]
    fn string_agg_aggregate() {
        // Values validated against PostgreSQL 18.4. Without an aggregate
        // ORDER BY, PostgreSQL leaves the concatenation order unspecified; our
        // scan order is a valid such order, so the non-distinct assertions
        // check the multiset of elements rather than a fixed sequence.
        let (mut e, mut b) = test_engine();
        run_with(&mut e, &mut b, "CREATE TABLE s (g int, v text)");
        run_with(&mut e, &mut b, "INSERT INTO s VALUES (1,'b'),(1,'a'),(1,NULL),(1,'a'),(2,'z')");
        // Per group: NULL skipped, duplicates kept (order unspecified).
        let rows = data_rows(&run_with(&mut e, &mut b,
            "SELECT g, string_agg(v, ',') FROM s GROUP BY g ORDER BY g"));
        let g1: Vec<&str> = rows[0].strip_prefix("1|").unwrap().split(',').collect();
        let mut g1s = g1.clone();
        g1s.sort_unstable();
        assert_eq!(g1s, ["a", "a", "b"]);
        assert_eq!(rows[1], "2|z");
        // All-NULL input yields NULL, not an empty string.
        assert_eq!(
            data_rows(&run_with(&mut e, &mut b, "SELECT string_agg(v, ',') FROM s WHERE v IS NULL")),
            ["NULL"]
        );
        // DISTINCT deduplicates and emits the values in sorted order (PG).
        assert_eq!(
            data_rows(&run_with(&mut e, &mut b, "SELECT string_agg(distinct v, ',') FROM s WHERE g = 1")),
            ["a,b"]
        );
        // string_agg with both DISTINCT and ORDER BY is not supported yet.
        assert!(String::from_utf8_lossy(
            &run_with(&mut e, &mut b, "SELECT string_agg(distinct v, ',' ORDER BY v) FROM s")
        )
        .contains("0A000"));
    }

    #[test]
    fn string_agg_ordered() {
        // string_agg(x, sep ORDER BY key) — values validated against PG 18.4.
        let (mut e, mut b) = test_engine();
        run_with(&mut e, &mut b, "CREATE TABLE s (g int, v text, ord int)");
        run_with(&mut e, &mut b, "INSERT INTO s VALUES (1,'b',2),(1,'a',1),(1,'c',3),(2,'z',1)");
        // ORDER BY a separate key column.
        assert_eq!(
            data_rows(&run_with(&mut e, &mut b,
                "SELECT g, string_agg(v, ',' ORDER BY ord) FROM s GROUP BY g ORDER BY g")),
            ["1|a,b,c", "2|z"]
        );
        // ORDER BY the value, descending.
        assert_eq!(
            data_rows(&run_with(&mut e, &mut b,
                "SELECT g, string_agg(v, ',' ORDER BY v DESC) FROM s GROUP BY g ORDER BY g")),
            ["1|c,b,a", "2|z"]
        );
    }

    #[test]
    fn math_functions() {
        // Values + types validated against PostgreSQL 18.4.
        let (mut e, mut b) = test_engine();
        let r = |e: &mut Engine, b: &mut Budget, sql: &str| data_rows(&run_with(e, b, sql));
        assert_eq!(r(&mut e, &mut b, "SELECT floor(5.7)"), ["5"]); // numeric
        assert_eq!(r(&mut e, &mut b, "SELECT ceil(5.2)"), ["6"]);
        assert_eq!(r(&mut e, &mut b, "SELECT trunc(5.7)"), ["5"]);
        assert_eq!(r(&mut e, &mut b, "SELECT floor(-2.5)"), ["-3"]); // toward -inf
        assert_eq!(r(&mut e, &mut b, "SELECT ceil(-2.5)"), ["-2"]);
        assert_eq!(r(&mut e, &mut b, "SELECT trunc(-2.9)"), ["-2"]);
        assert_eq!(r(&mut e, &mut b, "SELECT round(2.5)"), ["3"]); // numeric: half away from zero
        assert_eq!(r(&mut e, &mut b, "SELECT round(3.5)"), ["4"]);
        assert_eq!(r(&mut e, &mut b, "SELECT round(2.5::float8)"), ["2"]); // float: half to even
        assert_eq!(r(&mut e, &mut b, "SELECT round(3.5::float8)"), ["4"]);
        assert_eq!(r(&mut e, &mut b, "SELECT round(1.2345, 2)"), ["1.23"]);
        assert_eq!(r(&mut e, &mut b, "SELECT round(1.005, 2)"), ["1.01"]);
        assert_eq!(r(&mut e, &mut b, "SELECT floor(5)"), ["5"]); // int -> double
        assert_eq!(r(&mut e, &mut b, "SELECT sign(-3)"), ["-1"]);
        assert_eq!(r(&mut e, &mut b, "SELECT sign(0.0)"), ["0"]);
        assert_eq!(r(&mut e, &mut b, "SELECT sqrt(9)"), ["3"]);
        assert_eq!(r(&mut e, &mut b, "SELECT sqrt(2)"), ["1.4142135623730951"]);
        assert_eq!(r(&mut e, &mut b, "SELECT power(2, 10)"), ["1024"]);
        assert_eq!(r(&mut e, &mut b, "SELECT mod(7, 3)"), ["1"]);
        assert_eq!(r(&mut e, &mut b, "SELECT mod(-7, 3)"), ["-1"]);
        // Errors.
        assert!(String::from_utf8_lossy(&run_with(&mut e, &mut b, "SELECT sqrt(-1)")).contains("2201F"));
        assert!(String::from_utf8_lossy(&run_with(&mut e, &mut b, "SELECT mod(1, 0)")).contains("22012"));
    }

    #[test]
    fn datetime_functions() {
        // Values validated against PostgreSQL 18.4 for
        // timestamp '2024-03-15 14:30:45.5'.
        let (mut e, mut b) = test_engine();
        let r = |e: &mut Engine, b: &mut Budget, sql: &str| data_rows(&run_with(e, b, sql));
        let ts = "timestamp '2024-03-15 14:30:45.5'";
        assert_eq!(r(&mut e, &mut b, &format!("SELECT extract(year from {ts})")), ["2024"]);
        assert_eq!(r(&mut e, &mut b, &format!("SELECT extract(month from {ts})")), ["3"]);
        assert_eq!(r(&mut e, &mut b, &format!("SELECT extract(day from {ts})")), ["15"]);
        assert_eq!(r(&mut e, &mut b, &format!("SELECT extract(hour from {ts})")), ["14"]);
        assert_eq!(r(&mut e, &mut b, &format!("SELECT extract(dow from {ts})")), ["5"]);
        assert_eq!(r(&mut e, &mut b, &format!("SELECT extract(doy from {ts})")), ["75"]);
        assert_eq!(r(&mut e, &mut b, &format!("SELECT extract(quarter from {ts})")), ["1"]);
        assert_eq!(r(&mut e, &mut b, &format!("SELECT extract(week from {ts})")), ["11"]);
        assert_eq!(r(&mut e, &mut b, &format!("SELECT extract(isodow from {ts})")), ["5"]);
        // extract returns numeric (second/epoch keep 6 decimals); date_part is float.
        assert_eq!(r(&mut e, &mut b, &format!("SELECT extract(second from {ts})")), ["45.500000"]);
        assert_eq!(r(&mut e, &mut b, &format!("SELECT date_part('second', {ts})")), ["45.5"]);
        assert_eq!(r(&mut e, &mut b, &format!("SELECT extract(epoch from {ts})")), ["1710513045.500000"]);
        assert_eq!(r(&mut e, &mut b, &format!("SELECT date_part('epoch', {ts})")), ["1710513045.5"]);
        // date_trunc.
        assert_eq!(r(&mut e, &mut b, &format!("SELECT date_trunc('year', {ts})")), ["2024-01-01 00:00:00"]);
        assert_eq!(r(&mut e, &mut b, &format!("SELECT date_trunc('month', {ts})")), ["2024-03-01 00:00:00"]);
        assert_eq!(r(&mut e, &mut b, &format!("SELECT date_trunc('hour', {ts})")), ["2024-03-15 14:00:00"]);
        assert_eq!(r(&mut e, &mut b, &format!("SELECT date_trunc('minute', {ts})")), ["2024-03-15 14:30:00"]);
    }

    #[test]
    fn set_operations() {
        let (mut e, mut b) = test_engine();
        run_with(&mut e, &mut b, "CREATE TABLE t (a int)");
        run_with(&mut e, &mut b, "CREATE TABLE u (b int)");
        run_with(&mut e, &mut b, "INSERT INTO t VALUES (1),(2),(3)");
        run_with(&mut e, &mut b, "INSERT INTO u VALUES (2),(3),(4)");
        // UNION deduplicates and sorts by the trailing ORDER BY.
        assert_eq!(
            data_rows(&run_with(&mut e, &mut b, "SELECT a FROM t UNION SELECT b FROM u ORDER BY a")),
            ["1", "2", "3", "4"]
        );
        // UNION ALL keeps duplicates.
        let mut all = data_rows(&run_with(&mut e, &mut b, "SELECT a FROM t UNION ALL SELECT b FROM u"));
        all.sort();
        assert_eq!(all, ["1", "2", "2", "3", "3", "4"]);
        // INTERSECT and EXCEPT.
        assert_eq!(
            data_rows(&run_with(&mut e, &mut b, "SELECT a FROM t INTERSECT SELECT b FROM u ORDER BY 1")),
            ["2", "3"]
        );
        assert_eq!(
            data_rows(&run_with(&mut e, &mut b, "SELECT a FROM t EXCEPT SELECT b FROM u ORDER BY 1")),
            ["1"]
        );
        // Literal branches, dedup, LIMIT.
        assert_eq!(
            data_rows(&run_with(&mut e, &mut b, "SELECT 1 UNION SELECT 2 UNION SELECT 1 ORDER BY 1")),
            ["1", "2"]
        );
        // INTERSECT binds tighter than UNION: 1 UNION (2 INTERSECT 2) = {1,2}.
        assert_eq!(
            data_rows(&run_with(&mut e, &mut b, "SELECT 1 UNION SELECT 2 INTERSECT SELECT 2 ORDER BY 1")),
            ["1", "2"]
        );
        // Numeric-tower unification (int + numeric -> numeric).
        assert_eq!(
            data_rows(&run_with(&mut e, &mut b, "SELECT 1 UNION SELECT 2.5 ORDER BY 1")),
            ["1", "2.5"]
        );
        // Multiset ALL variants (validated against PostgreSQL 18.4).
        run_with(&mut e, &mut b, "CREATE TABLE m1 (x int)");
        run_with(&mut e, &mut b, "CREATE TABLE m2 (y int)");
        run_with(&mut e, &mut b, "CREATE TABLE m3 (z int)");
        run_with(&mut e, &mut b, "INSERT INTO m1 VALUES (1),(1),(2)");
        run_with(&mut e, &mut b, "INSERT INTO m2 VALUES (1),(2),(2)");
        run_with(&mut e, &mut b, "INSERT INTO m3 VALUES (1)");
        assert_eq!(
            data_rows(&run_with(&mut e, &mut b, "SELECT x FROM m1 INTERSECT ALL SELECT y FROM m2 ORDER BY 1")),
            ["1", "2"]
        );
        assert_eq!(
            data_rows(&run_with(&mut e, &mut b, "SELECT x FROM m1 EXCEPT ALL SELECT z FROM m3 ORDER BY 1")),
            ["1", "2"]
        );
        // Parenthesized branches override precedence: (1 UNION 2) INTERSECT 2 = {2}.
        assert_eq!(
            data_rows(&run_with(&mut e, &mut b, "(SELECT 1 UNION SELECT 2) INTERSECT SELECT 2 ORDER BY 1")),
            ["2"]
        );
        assert_eq!(
            data_rows(&run_with(&mut e, &mut b, "(SELECT 1) UNION (SELECT 2) ORDER BY 1")),
            ["1", "2"]
        );
    }

    #[test]
    fn set_operation_errors() {
        let (mut e, mut b) = test_engine();
        // Column-count mismatch.
        let a = run_with(&mut e, &mut b, "SELECT 1 UNION SELECT 1, 2");
        assert!(String::from_utf8_lossy(&a).contains("42601"), "{:?}", String::from_utf8_lossy(&a));
        // Incompatible types.
        let c = run_with(&mut e, &mut b, "SELECT 1 UNION SELECT 'x'");
        assert!(String::from_utf8_lossy(&c).contains("42804"), "{:?}", String::from_utf8_lossy(&c));
    }

    #[test]
    fn timezone_offset_affects_timestamptz() {
        // Reference outputs from PostgreSQL 18.4 for
        // timestamptz '2024-01-15 14:30:00+00'.
        let (mut e, mut b) = test_engine();
        let tstz = "timestamptz '2024-01-15 14:30:00+00'";
        let go = |e: &mut Engine, b: &mut Budget, sql: String| data_rows(&run_with(e, b, &sql));
        // ISO output with fixed offsets (note PostgreSQL's inverted signs).
        assert_eq!(go(&mut e, &mut b, format!("SET timezone='Etc/GMT+5'; SELECT {tstz}")), ["2024-01-15 09:30:00-05"]);
        assert_eq!(go(&mut e, &mut b, format!("SET timezone='-08:00'; SELECT {tstz}")), ["2024-01-15 22:30:00+08"]);
        assert_eq!(go(&mut e, &mut b, format!("SET timezone='+05:30'; SELECT {tstz}")), ["2024-01-15 09:00:00-05:30"]);
        // Non-ISO zone abbreviation: Etc zones show the offset, bare offsets show
        // nothing (a trailing space), exactly as PostgreSQL does.
        assert_eq!(go(&mut e, &mut b, format!("SET datestyle='SQL'; SET timezone='Etc/GMT+5'; SELECT {tstz}")), ["01/15/2024 09:30:00 -05"]);
        assert_eq!(go(&mut e, &mut b, format!("SET datestyle='Postgres'; SET timezone='-08:00'; SELECT {tstz}")), ["Mon Jan 15 22:30:00 2024 "]);
        // Named zones with DST are modeled: the winter timestamp above falls in
        // standard time, so New York is -05 (matches PostgreSQL 18.4).
        assert_eq!(go(&mut e, &mut b, format!("SET timezone='America/New_York'; SELECT {tstz}")), ["2024-01-15 09:30:00-05"]);
        // A summer timestamp in the same zone shows daylight time (-04).
        let summer = "timestamptz '2024-07-15 14:30:00+00'";
        assert_eq!(go(&mut e, &mut b, format!("SET timezone='America/New_York'; SELECT {summer}")), ["2024-07-15 10:30:00-04"]);
        // An unknown zone name is still rejected loudly.
        assert!(String::from_utf8_lossy(&run_with(&mut e, &mut b, "SET timezone='Mars/Olympus'")).contains("0A000"));
    }

    #[test]
    fn datestyle_affects_date_output() {
        let (mut e, mut b) = test_engine();
        // ISO is the default.
        assert_eq!(data_rows(&run_with(&mut e, &mut b, "SELECT date '2024-01-15'")), ["2024-01-15"]);
        // A SET earlier in the batch changes a later SELECT's rendering.
        let r = run_with(
            &mut e,
            &mut b,
            "SET datestyle='SQL, DMY'; SELECT date '2024-01-15', timestamp '2024-01-15 14:30:00'",
        );
        assert_eq!(data_rows(&r), ["15/01/2024|15/01/2024 14:30:00"]);
        let r = run_with(&mut e, &mut b, "SET datestyle='Postgres'; SELECT timestamp '2024-01-15 14:30:00'");
        assert_eq!(data_rows(&r), ["Mon Jan 15 14:30:00 2024"]);
        let r = run_with(&mut e, &mut b, "SET datestyle='German'; SELECT date '2024-01-15'");
        assert_eq!(data_rows(&r), ["15.01.2024"]);
        // Cumulative canonical form in SHOW (German defaults to DMY).
        assert_eq!(
            data_rows(&run_with(&mut e, &mut b, "SET datestyle='ISO,MDY'; SET datestyle='German'; SHOW datestyle")),
            ["German, DMY"]
        );
    }

    #[test]
    fn set_and_show_session_gucs() {
        // GucState is per run_with call, so SET and SHOW share one call.
        let (mut e, mut b) = test_engine();
        // A supported value is stored and reflected by SHOW.
        let r = run_with(&mut e, &mut b, "SET application_name = 'myapp'; SHOW application_name");
        assert_eq!(data_rows(&r), ["myapp"]);
        // client_encoding accepts UTF8 (and synonyms) and rejects others.
        assert_eq!(message_types(&run_with(&mut e, &mut b, "SET client_encoding = 'UTF8'")), [b'C']);
        let bad = run_with(&mut e, &mut b, "SET client_encoding = 'LATIN1'");
        assert!(String::from_utf8_lossy(&bad).contains("0A000"), "{:?}", String::from_utf8_lossy(&bad));
        // A named IANA time zone is now accepted.
        assert_eq!(message_types(&run_with(&mut e, &mut b, "SET timezone = 'America/New_York'")), [b'C']);
        // An unknown zone name is still rejected loudly.
        assert!(String::from_utf8_lossy(&run_with(&mut e, &mut b, "SET timezone = 'Mars/Olympus'")).contains("0A000"));
        // DateStyle values are now honored (see datestyle_affects_date_output).
        assert_eq!(message_types(&run_with(&mut e, &mut b, "SET DateStyle = 'German'")), [b'C']);
        // SET TIME ZONE spelling maps to timezone; UTC is accepted.
        assert_eq!(message_types(&run_with(&mut e, &mut b, "SET TIME ZONE 'UTC'")), [b'C']);
        // An unknown parameter is rejected.
        assert!(String::from_utf8_lossy(&run_with(&mut e, &mut b, "SET no_such_guc = 1")).contains("42704"));
        // SHOW of a fixed server parameter still works.
        assert_eq!(data_rows(&run_with(&mut e, &mut b, "SHOW server_encoding")), ["UTF8"]);
    }

    #[test]
    fn prepare_coerces_args_to_declared_types() {
        // The prepared-statement pool is per run_with call, so PREPARE and
        // EXECUTE must share one call (one multi-statement simple query).
        let (mut e, mut b) = test_engine();
        run_with(&mut e, &mut b, "CREATE TABLE t (id int)");
        run_with(&mut e, &mut b, "INSERT INTO t VALUES (5)");
        // A text argument is coerced to the declared int type.
        let r = run_with(
            &mut e,
            &mut b,
            "PREPARE p (int) AS SELECT id FROM t WHERE id = $1; EXECUTE p('5')",
        );
        assert_eq!(data_rows(&r), ["5"]);
        // An argument that cannot become the declared type errors (not ignored).
        let bad = run_with(&mut e, &mut b, "PREPARE p2 (int) AS SELECT $1; EXECUTE p2('nope')");
        assert!(String::from_utf8_lossy(&bad).contains("22P02"), "{:?}", String::from_utf8_lossy(&bad));
        // Wrong argument count is rejected.
        let count = run_with(&mut e, &mut b, "PREPARE p3 (int) AS SELECT $1; EXECUTE p3(1, 2)");
        assert!(String::from_utf8_lossy(&count).contains("08P01"), "{:?}", String::from_utf8_lossy(&count));
        // An unknown declared type is rejected at PREPARE.
        let unk = run_with(&mut e, &mut b, "PREPARE q (nosuchtype) AS SELECT $1");
        assert!(String::from_utf8_lossy(&unk).contains("42704"));
    }

    #[test]
    fn varchar_length_is_enforced() {
        let (mut e, mut b) = test_engine();
        run_with(&mut e, &mut b, "CREATE TABLE t (s varchar(3))");
        assert_eq!(message_types(&run_with(&mut e, &mut b, "INSERT INTO t VALUES ('abc')")), [b'C']);
        let over = run_with(&mut e, &mut b, "INSERT INTO t VALUES ('abcd')");
        assert!(String::from_utf8_lossy(&over).contains("22001"), "{:?}", String::from_utf8_lossy(&over));
        // The stored value is unchanged (not truncated).
        assert_eq!(data_rows(&run_with(&mut e, &mut b, "SELECT s FROM t")), ["abc"]);
    }

    #[test]
    fn numeric_scale_and_precision_enforced() {
        let (mut e, mut b) = test_engine();
        run_with(&mut e, &mut b, "CREATE TABLE t (n numeric(5,2))");
        // Rounds to scale 2 (half away from zero) and pads to 2 fractional digits.
        run_with(&mut e, &mut b, "INSERT INTO t VALUES (12.345), (12.5), (1)");
        assert_eq!(
            data_rows(&run_with(&mut e, &mut b, "SELECT n FROM t ORDER BY n")),
            ["1.00", "12.35", "12.50"]
        );
        // Too many integer digits (p - s = 3) overflows.
        let over = run_with(&mut e, &mut b, "INSERT INTO t VALUES (1234.5)");
        assert!(String::from_utf8_lossy(&over).contains("22003"), "{:?}", String::from_utf8_lossy(&over));
        // Rounding that carries into a new integer digit also overflows.
        let carry = run_with(&mut e, &mut b, "INSERT INTO t VALUES (999.999)");
        assert!(String::from_utf8_lossy(&carry).contains("22003"));
    }

    #[test]
    fn type_modifier_on_wrong_type_is_rejected() {
        let (mut e, mut b) = test_engine();
        // A modifier on a type that does not take one errors loudly, in both a
        // column definition and a cast — rejected, not accepted.
        let bad = run_with(&mut e, &mut b, "CREATE TABLE t (x int(4))");
        assert!(String::from_utf8_lossy(&bad).contains("42601"), "{:?}", String::from_utf8_lossy(&bad));
        let bad2 = run_with(&mut e, &mut b, "SELECT 1::int(4)");
        assert!(String::from_utf8_lossy(&bad2).contains("42601"), "{:?}", String::from_utf8_lossy(&bad2));
    }

    #[test]
    fn insert_select() {
        let (mut e, mut b) = test_engine();
        run_with(&mut e, &mut b, "CREATE TABLE src (a int, b text)");
        run_with(&mut e, &mut b, "CREATE TABLE dst (a int, b text)");
        run_with(&mut e, &mut b, "INSERT INTO src VALUES (1,'x'),(2,'y'),(3,'z')");
        // INSERT ... SELECT with a WHERE filter and projection.
        let bytes = run_with(&mut e, &mut b, "INSERT INTO dst SELECT a, b FROM src WHERE a >= 2");
        assert_eq!(message_types(&bytes), [b'C']);
        assert_eq!(
            data_rows(&run_with(&mut e, &mut b, "SELECT a, b FROM dst ORDER BY a")),
            ["2|y", "3|z"]
        );
        // SELECT * source.
        run_with(&mut e, &mut b, "INSERT INTO dst SELECT * FROM src WHERE a = 1");
        assert_eq!(
            data_rows(&run_with(&mut e, &mut b, "SELECT a FROM dst ORDER BY a")),
            ["1", "2", "3"]
        );
        // Column list + constant projection; RETURNING.
        let bytes = run_with(
            &mut e,
            &mut b,
            "INSERT INTO dst (a) SELECT a * 10 FROM src WHERE a = 3 RETURNING a",
        );
        assert_eq!(data_rows(&bytes), ["30"]);
        // Self-insert reads the pre-insert snapshot (must not loop).
        run_with(&mut e, &mut b, "CREATE TABLE s2 (v int)");
        run_with(&mut e, &mut b, "INSERT INTO s2 VALUES (1),(2)");
        run_with(&mut e, &mut b, "INSERT INTO s2 SELECT v FROM s2");
        assert_eq!(
            data_rows(&run_with(&mut e, &mut b, "SELECT v FROM s2 ORDER BY v")),
            ["1", "1", "2", "2"]
        );
    }

    #[test]
    fn insert_select_column_count_mismatch() {
        let (mut e, mut b) = test_engine();
        run_with(&mut e, &mut b, "CREATE TABLE src (a int, b int)");
        run_with(&mut e, &mut b, "CREATE TABLE dst (a int)");
        run_with(&mut e, &mut b, "INSERT INTO src VALUES (1,2)");
        let bytes = run_with(&mut e, &mut b, "INSERT INTO dst SELECT * FROM src");
        assert!(String::from_utf8_lossy(&bytes).contains("42601"));
    }

    #[test]
    fn correlated_in_subquery() {
        let (mut e, mut b) = test_engine();
        run_with(&mut e, &mut b, "CREATE TABLE t (a int, g int)");
        run_with(&mut e, &mut b, "CREATE TABLE u (v int, g int)");
        run_with(&mut e, &mut b, "INSERT INTO t VALUES (1,100),(2,100),(3,200)");
        run_with(&mut e, &mut b, "INSERT INTO u VALUES (1,100),(3,200)");
        // a IN (values of u.v sharing t's group g)
        let bytes = run_with(
            &mut e,
            &mut b,
            "SELECT a FROM t WHERE a IN (SELECT v FROM u WHERE u.g = t.g) ORDER BY a",
        );
        assert_eq!(data_rows(&bytes), ["1", "3"]);
    }
}
