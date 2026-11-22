//! The pull nodes of a join plan PostgreSQL's join search chose: each node
//! returns one row at a time and keeps its position in its own record, so a
//! storage wait anywhere below returns to the reader with every node's
//! state intact and the same call resumes it.
//!
//! The records live in one `plan_build_bytes` lease, one per plan path. A
//! scan's current row lives in the reader's arena above the mark it took
//! before reading it, and is released when the scan reads its next row; an
//! outer row's bytes lie below its inner rows', so a new outer row releases
//! every inner one with it. A `Materialize` keeps its input's rows encoded
//! in `plan_build_bytes` leases and reads them again at each rescan.
//!
//! No record holds a `Storage` or `Arena` reference: every call takes them,
//! so a retained reader keeping the nodes between calls borrows neither.
//!
//! A hash join keeps its table in leases too ([`hash::BatchStore`]), its
//! inner rows and the outer rows it saves to later batches encoded in its
//! store, and moves through `ExecHashJoin`'s phases one row at a time.
//!
//! Nested loops, hash joins over keys of hashable types, `Materialize` and
//! scans run here; a plan holding any other node runs in
//! [`super::run_modeled`].

use super::*;
use crate::mem::arena::ArenaMark;
use crate::mem::lease_pool::{LeaseError, LeasePool, MutableSlice};
use crate::storage::{CursorScratch, RowCursorBuffers};
use std::sync::Arc;

mod hash;

/// Whether every node of `model`'s plan, whose relations resolve in
/// `written`, runs as a pull node.
pub(crate) fn pulls<'a>(model: &ModeledJoin<'a>, written: &QueryScope<'a>) -> bool {
    fn path_pulls(model: &ModeledJoin<'_>, scope: &QueryScope<'_>, id: u32) -> bool {
        match model.plan.path(id).kind {
            PathKind::Dummy => true,
            PathKind::Scan { rel, .. } => model.rels[usize::from(rel)].lateral.is_none(),
            PathKind::Material { input } => path_pulls(model, scope, input),
            PathKind::NestLoop {
                jointype,
                outer,
                inner,
                ..
            } => {
                matches!(
                    jointype,
                    JoinType::Inner | JoinType::Left | JoinType::Semi | JoinType::Anti
                ) && path_pulls(model, scope, outer)
                    && path_pulls(model, scope, inner)
            }
            // A join over keys without a hash function here builds a sorted
            // table instead, which runs in the general executor.
            PathKind::HashJoin {
                outer, inner, hash, ..
            } => {
                hash.as_slice().iter().all(|&clause| {
                    model
                        .clause_columns(&model.plan.clauses[usize::from(clause)])
                        .is_some_and(|(left, right)| {
                            [left, right].into_iter().all(|member| {
                                datum_type_hashable(member_type(model, scope, member))
                            })
                        })
                }) && path_pulls(model, scope, outer)
                    && path_pulls(model, scope, inner)
            }
            PathKind::Unique { .. } | PathKind::Memoize { .. } | PathKind::MergeJoin { .. } => {
                false
            }
        }
    }
    path_pulls(model, model.scope(written), model.plan.top)
}

/// Where a nested loop is between its rows.
#[derive(Clone, Copy, PartialEq, Eq)]
enum LoopPhase {
    /// The next call reads the next outer row.
    Outer,
    /// The next call reads the current outer row's next inner row.
    Inner,
    Done,
}

/// One relation of a row a `Materialize` stored: where its encoded values
/// lie in the store's bytes, and its row identity.
#[derive(Clone, Copy)]
struct StoredRel {
    rel: u8,
    start: u32,
    len: u32,
    rowid: Option<u64>,
}

/// Rows a node stored, each its relations in order: a `Materialize`'s
/// input rows, or a hash join's inner rows and saved outer rows.
struct MaterialStore {
    entries: MutableSlice<StoredRel>,
    bytes: MutableSlice<u8>,
}

fn store_full(error: LeaseError) -> SqlError {
    crate::sql_err!(
        crate::sql::eval::sqlstate::PROGRAM_LIMIT_EXCEEDED,
        "a join node's stored rows exceed plan_build_bytes: {}",
        error
    )
    .plan_build_exhausted()
}

impl MaterialStore {
    fn new(pool: &Arc<LeasePool>) -> Result<Self, SqlError> {
        Ok(Self {
            entries: pool.lease(0).map_err(store_full)?,
            bytes: pool.lease(0).map_err(store_full)?,
        })
    }

    fn clear(&mut self) {
        self.entries.clear();
        self.bytes.clear();
    }

    /// Encodes the bound rows of `relids` into the store, returning the
    /// row's first entry.
    fn save<'a>(
        &mut self,
        cx: JoinContext<'_, 'a>,
        bound: &[Option<BaseRow<'a>>],
        relids: Relids,
    ) -> Result<u32, SqlError> {
        let first = u32::try_from(self.entries.len()).map_err(|_| {
            crate::sql_err!(
                crate::sql::eval::sqlstate::PROGRAM_LIMIT_EXCEEDED,
                "a join node stores more than 4294967295 relation rows"
            )
            .plan_build_exhausted()
        })?;
        for table in (0..cx.scope.n).filter(|&table| relids.contains(table as u32)) {
            let row = bound[table].expect("a stored row binds its relations");
            let mark = cx.arena.mark();
            let pushed = crate::sql::exec::encode_projected_formed(row.values, row.forms, cx.arena)
                .and_then(|encoded| self.push_bytes(encoded));
            // SAFETY: the encoding was copied into the store.
            unsafe { cx.arena.rewind_to(mark) };
            let (start, len) = pushed?;
            self.push_entry(StoredRel {
                rel: table as u8,
                start,
                len,
                rowid: row.rowid,
            })?;
        }
        Ok(first)
    }

    /// Binds the `count` relations stored from entry `first` in `bound`,
    /// their values decoded into the arena.
    fn load<'a>(
        &self,
        first: usize,
        count: usize,
        cx: JoinContext<'_, 'a>,
        bound: &mut [Option<BaseRow<'a>>],
    ) -> Result<(), SqlError> {
        for rel in &self.entries.as_slice()[first..first + count] {
            let table = usize::from(rel.rel);
            // The values read the row's bytes, copied out of the store.
            let bytes = cx
                .arena
                .alloc_slice_copy(
                    &self.bytes.as_slice()[rel.start as usize..(rel.start + rel.len) as usize],
                )
                .map_err(|_| arena_full())?;
            let count = cx.scope.stored_width(table) + cx.model.expressions.len();
            let values = cx
                .arena
                .alloc_slice_with(count, |_| Datum::Null)
                .map_err(|_| arena_full())?;
            for (column, value) in values.iter_mut().enumerate() {
                *value = crate::sql::exec::decode_projected_col_record(bytes, column, cx.arena)?;
            }
            // The stored forms the row's values carried, kept with them.
            let width = cx.scope.stored_width(table);
            let forms = cx
                .arena
                .alloc_slice_with(width, |_| StoredForm::Inline)
                .map_err(|_| arena_full())?;
            crate::sql::exec::projected_stored_forms(bytes, forms);
            let forms: &[StoredForm] = if forms.iter().all(|&form| form == StoredForm::Inline) {
                &[]
            } else {
                forms
            };
            bound[table] = Some(BaseRow {
                values,
                forms,
                rowid: rel.rowid,
            });
        }
        Ok(())
    }

    /// Appends `bytes`, growing the store geometrically: each move copies
    /// the rows saved so far.
    fn push_bytes(&mut self, bytes: &[u8]) -> Result<(u32, u32), SqlError> {
        let start = self.bytes.len();
        let end = start + bytes.len();
        let (Ok(start32), Ok(len32), Ok(_)) = (
            u32::try_from(start),
            u32::try_from(bytes.len()),
            u32::try_from(end),
        ) else {
            return Err(crate::sql_err!(
                crate::sql::eval::sqlstate::PROGRAM_LIMIT_EXCEEDED,
                "a join node's stored rows exceed 4 GiB"
            )
            .plan_build_exhausted());
        };
        if end > self.bytes.capacity_entries() {
            let grown = self
                .bytes
                .capacity_entries()
                .saturating_mul(2)
                .min(self.bytes.max_entries())
                .max(end);
            self.bytes.reserve_exact(grown).map_err(store_full)?;
        }
        self.bytes.extend_from_slice(bytes).map_err(store_full)?;
        Ok((start32, len32))
    }

    fn push_entry(&mut self, entry: StoredRel) -> Result<(), SqlError> {
        if self.entries.len() == self.entries.capacity_entries() {
            let grown = self
                .entries
                .capacity_entries()
                .saturating_mul(2)
                .min(self.entries.max_entries())
                .max(self.entries.len() + 1);
            self.entries.reserve_exact(grown).map_err(store_full)?;
        }
        self.entries.push(entry).map_err(store_full)
    }
}

/// One plan path's state between rows.
#[expect(
    clippy::large_enum_variant,
    reason = "one per plan path in a fixed lease, never moved between rows"
)]
enum Record<'a> {
    /// A path no row reaches, or one reached only as a `Materialize`'s
    /// input after it filled its store.
    Idle,
    Scan {
        record: ScanRecord<'a>,
        /// The arena's frontier before the current row.
        mark: Option<ArenaMark>,
    },
    NestLoop {
        phase: LoopPhase,
        /// Whether the current outer row met the join's conditions.
        matched: bool,
    },
    Material {
        store: MaterialStore,
        /// Relations per stored row.
        width: usize,
        filled: bool,
        /// The next stored row a read returns.
        at: usize,
        mark: Option<ArenaMark>,
    },
    HashJoin(HashRecord),
}

/// Where a hash join is between its rows (`ExecHashJoin`'s states).
#[derive(Clone, Copy, PartialEq, Eq)]
enum HashPhase {
    /// The next call starts an execution: it probes a kept one-batch table
    /// again or builds one.
    Start,
    /// The table is built after the first outer row is read.
    OuterFirst,
    /// The inner rows are read into the table; `outer_pending` says an
    /// outer row read first waits to probe it.
    Build {
        outer_pending: bool,
    },
    /// Batch 0: each outer row read probes the table or is saved to its
    /// batch.
    Probe,
    /// The unmatched inner rows return (`HJ_FILL_INNER_TUPLES`), then the
    /// join goes on to `then`.
    Fill {
        then: AfterFill,
    },
    /// The next batch is loaded (`ExecHashJoinNewBatch`).
    NextBatch,
    /// The current batch's saved outer rows probe it.
    ProbeBatch,
    Done,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum AfterFill {
    NextBatch,
    Done,
}

/// The current outer row's probe (`HJ_SCAN_BUCKET`, `HJ_FILL_OUTER_TUPLE`).
#[derive(Clone, Copy)]
struct HashProbe {
    chain: hash::Chain,
    /// The row met the join's conditions with some inner row.
    matched: bool,
    /// A key of the row is NULL, which matches nothing.
    outer_null: bool,
    /// The row returned with NULLs for the inner relations.
    filled: bool,
}

/// A hash join's state between rows.
struct HashRecord {
    table: Option<hash::BatchStore>,
    /// The table's inner rows and the outer rows saved to later batches.
    store: MaterialStore,
    phase: HashPhase,
    /// The table is one batch's and is probed again at a rescan
    /// (`ExecReScanHashJoin`).
    frozen: bool,
    probe: Option<HashProbe>,
    /// The current batch's saved outer rows.
    outer_rows: MutableSlice<hash::Saved>,
    /// The unmatched inner rows being returned.
    fill: MutableSlice<u32>,
    /// The next entry of `outer_rows` or `fill`.
    at: usize,
    /// The batches batch 0 ended with.
    outer_start: u32,
    /// The arena's frontier before a saved outer row, and before an inner
    /// row read from the store.
    outer_mark: Option<ArenaMark>,
    inner_mark: Option<ArenaMark>,
}

impl HashRecord {
    fn new(pool: &Arc<LeasePool>) -> Result<Self, SqlError> {
        Ok(Self {
            table: None,
            store: MaterialStore::new(pool)?,
            phase: HashPhase::Start,
            frozen: false,
            probe: None,
            outer_rows: pool.lease(0).map_err(store_full)?,
            fill: pool.lease(0).map_err(store_full)?,
            at: 0,
            outer_start: 0,
            outer_mark: None,
            inner_mark: None,
        })
    }

    /// Starts the join again; a table its inner rows depend on the outer
    /// rows of is built again.
    fn restart(&mut self, rebuild: bool) {
        self.phase = HashPhase::Start;
        self.probe = None;
        self.outer_rows.clear();
        self.fill.clear();
        self.at = 0;
        self.outer_mark = None;
        self.inner_mark = None;
        if rebuild {
            self.frozen = false;
        }
    }
}

fn hash_record<'r>(records: &'r mut [Record<'_>], id: u32) -> &'r mut HashRecord {
    let Record::HashJoin(record) = &mut records[id as usize] else {
        unreachable!("a hash join's record");
    };
    record
}

/// The relations of `relids` among the query's `n`.
fn relation_count(relids: Relids, n: usize) -> usize {
    (0..n)
        .filter(|&table| relids.contains(table as u32))
        .count()
}

/// A modeled join read by pull nodes.
pub(crate) struct ModeledPull<'a> {
    txid: u32,
    model: ModeledJoin<'a>,
    /// The statement's parameters, copied in when the reader opened.
    params: &'a [Datum<'a>],
    layout: JoinLayout<'a>,
    output: OutputRow<'a>,
    records: MutableSlice<Record<'a>>,
    /// The rows bound so far, by the plan's relation numbers.
    bound: &'a mut [Option<BaseRow<'a>>],
    /// The arena's frontier before the first row, where a rewind returns:
    /// taken at the first read, past what the reader allocated as it
    /// opened.
    base: Option<ArenaMark>,
    /// A joined row is ready and the reader has not finished with it.
    current: bool,
    done: bool,
}

impl<'a> ModeledPull<'a> {
    /// The pull nodes of `model`, whose relations resolve in `written`
    /// (the query's FROM clause) through [`ModeledJoin::scope`]. Every
    /// scan's node is made here, before any row.
    pub(crate) fn new(
        storage: &Storage,
        txid: u32,
        model: ModeledJoin<'a>,
        written: &QueryScope<'a>,
        params: &[Datum<'a>],
        arena: &'a Arena,
    ) -> Result<Self, SqlError> {
        debug_assert!(pulls(&model, written));
        let params: &'a [Datum<'a>] = arena.alloc_slice_copy(params).map_err(|_| arena_full())?;
        let scope = model.scope(written);
        let layout = JoinLayout::new(&model, scope, arena)?;
        let output = OutputRow::new(&model, written, arena)?;
        let catalog = super::super::storage_catalog(storage, arena, txid);
        let hooks = EvalHooks {
            catalog: Some(&catalog),
            ..crate::sql::eval::NO_HOOKS
        };
        let paths = model.plan.paths.len();
        let pool = storage.plan_build();
        let mut records = pool.lease::<Record<'a>>(paths).map_err(node_pool_full)?;
        for id in 0..paths {
            let record = match model.plan.paths[id].kind {
                PathKind::Scan { rel, path } if reaches(&model, model.plan.top, id as u32) => {
                    let modeled = model.rels[usize::from(rel)];
                    let scan = modeled.scan(storage, txid, path, params, arena)?;
                    let mut node = super::super::scan::PathScan::new(
                        storage,
                        txid,
                        modeled.from,
                        scan.condition,
                        scan.path,
                        arena,
                        params,
                        &hooks,
                    )?;
                    // Its first execution starts at its first read.
                    node.rescan();
                    Record::Scan {
                        record: ScanRecord {
                            node,
                            param: scan.param,
                            outer: arena
                                .alloc_slice_with(layout.width, |_| Datum::Null)
                                .map_err(|_| arena_full())?,
                            outer_forms: arena
                                .alloc_slice_with(layout.width, |_| StoredForm::Inline)
                                .map_err(|_| arena_full())?,
                        },
                        mark: None,
                    }
                }
                PathKind::NestLoop { .. } => Record::NestLoop {
                    phase: LoopPhase::Outer,
                    matched: false,
                },
                PathKind::HashJoin { .. } => Record::HashJoin(HashRecord::new(pool)?),
                PathKind::Material { input } => Record::Material {
                    store: MaterialStore::new(pool)?,
                    width: (0..scope.n)
                        .filter(|&table| model.plan.path(input).relids.contains(table as u32))
                        .count(),
                    filled: false,
                    at: 0,
                    mark: None,
                },
                _ => Record::Idle,
            };
            records.push_reserved(record).map_err(node_pool_full)?;
        }
        let bound = arena
            .alloc_slice_with(scope.n, |_| None::<BaseRow<'a>>)
            .map_err(|_| arena_full())?;
        Ok(Self {
            txid,
            model,
            params,
            layout,
            output,
            records,
            bound,
            base: None,
            current: false,
            done: false,
        })
    }

    /// The physical tables the plan's scans read.
    pub(crate) fn tables<'s>(
        &'s self,
        written: &'s QueryScope<'a>,
    ) -> impl Iterator<Item = usize> + 's {
        let scope = self.model.scope(written);
        self.model
            .plan
            .paths
            .iter()
            .filter_map(move |path| match path.kind {
                PathKind::Scan { rel, .. } => Some(scope.slots[usize::from(rel)]),
                _ => None,
            })
    }

    /// Whether the reader holds a joined row.
    pub(crate) fn has_pending(&self) -> bool {
        self.current
    }

    /// The reader finished with the current row; its bytes are released
    /// when the scan that read it moves on.
    pub(crate) fn finish_row(&mut self) {
        self.current = false;
    }

    /// Ends every scan; nothing is read again.
    pub(crate) fn close(&mut self) {
        for record in self.records.iter_mut() {
            if let Record::Scan { record, .. } = record {
                record.node.stop();
            }
        }
        self.done = true;
    }

    /// Returns to the first row under the same snapshot. A `Materialize`
    /// keeps its rows and reads them again.
    pub(crate) fn rewind(&mut self, state_arena: &Arena) -> Result<(), SqlError> {
        if self.current {
            return Err(crate::sql_err!(
                crate::sql::eval::sqlstate::INTERNAL_ERROR,
                "a modeled join rewound with a joined row"
            ));
        }
        if let Some(base) = self.base {
            // SAFETY: the reader holds no joined row; every row the nodes
            // read lies above the frontier taken before the first one.
            unsafe { state_arena.rewind_to(base) };
        }
        for record in self.records.iter_mut() {
            match record {
                Record::Idle => {}
                Record::Scan { record, mark } => {
                    record.node.rescan();
                    *mark = None;
                }
                Record::NestLoop { phase, matched } => {
                    *phase = LoopPhase::Outer;
                    *matched = false;
                }
                Record::Material { at, mark, .. } => {
                    *at = 0;
                    *mark = None;
                }
                Record::HashJoin(record) => record.restart(false),
            }
        }
        self.bound.fill(None);
        self.done = false;
        Ok(())
    }

    /// Moves to the next joined row; `Ok(false)` at the join's end. A wait
    /// keeps every node's position; the same call resumes it.
    pub(crate) fn prepare_row(
        &mut self,
        storage: &Storage,
        written: &QueryScope<'a>,
        state_arena: &'a Arena,
        buffers: &mut RowCursorBuffers,
        work: &Arena,
    ) -> Result<bool, SqlError> {
        if self.current {
            return Ok(true);
        }
        if self.done {
            return Ok(false);
        }
        if self.base.is_none() {
            self.base = Some(state_arena.mark());
        }
        let Self {
            txid,
            model,
            params,
            layout,
            output,
            records,
            bound,
            ..
        } = self;
        let txid = *txid;
        let catalog = super::super::storage_catalog(storage, work, txid);
        let hooks = EvalHooks {
            catalog: Some(&catalog),
            ..crate::sql::eval::NO_HOOKS
        };
        let scope = model.scope(written);
        let cx = JoinContext::new(
            storage,
            scope,
            txid,
            model,
            state_arena,
            params,
            &hooks,
            layout,
        );
        let mut step = Step {
            cx,
            records,
            bound,
            buffers,
        };
        if !step.next(model.plan.top)? {
            self.done = true;
            return Ok(false);
        }
        cx.check()?;
        output.fill(model, written, layout.nulls, bound);
        self.current = true;
        Ok(true)
    }

    /// The current joined row over `written`.
    pub(crate) fn with_row<'work, R>(
        &'work self,
        written: &'work QueryScope<'a>,
        each: impl FnOnce(&JoinRow<'_, 'work, '_>) -> Result<R, SqlError>,
    ) -> Result<R, SqlError> {
        debug_assert!(self.current);
        each(&self.output.row(&self.model, written))
    }
}

/// Whether path `id` is in the plan under path `from`.
fn reaches(model: &ModeledJoin<'_>, from: u32, id: u32) -> bool {
    if from == id {
        return true;
    }
    match model.plan.path(from).kind {
        PathKind::Material { input } => reaches(model, input, id),
        PathKind::NestLoop { outer, inner, .. } | PathKind::HashJoin { outer, inner, .. } => {
            reaches(model, outer, id) || reaches(model, inner, id)
        }
        _ => false,
    }
}

/// One call's view of the nodes.
struct Step<'s, 'r, 'a> {
    cx: JoinContext<'r, 'a>,
    records: &'s mut [Record<'a>],
    bound: &'s mut [Option<BaseRow<'a>>],
    buffers: &'s mut RowCursorBuffers,
}

impl<'a> Step<'_, '_, 'a> {
    /// Moves path `id` to its next row, binding its relations' rows.
    fn next(&mut self, id: u32) -> Result<bool, SqlError> {
        super::super::check_timeout()?;
        let path = *self.cx.model.plan.path(id);
        match path.kind {
            PathKind::Dummy => Ok(false),
            PathKind::Scan { rel, .. } => self.scan(id, usize::from(rel)),
            PathKind::Material { input } => self.material(id, input),
            PathKind::NestLoop {
                jointype,
                outer,
                inner,
                clauses,
                inner_unique,
            } => {
                let inner_relids = self.cx.model.plan.path(inner).relids;
                let (join_quals, other_quals) =
                    self.cx
                        .split(&clauses, path.relids, jointype, &ClauseList::EMPTY);
                // `ExecNestLoop`: a semi join, or a unique inner relation,
                // ends each outer row's scan at its first match, an anti
                // join at its first match without returning it; a left or
                // anti join returns an unmatched outer row with NULLs.
                let single_match = inner_unique || jointype == JoinType::Semi;
                loop {
                    let Record::NestLoop { phase, matched } = self.records[id as usize] else {
                        unreachable!("a nested loop's record");
                    };
                    match phase {
                        LoopPhase::Done => return Ok(false),
                        LoopPhase::Outer => {
                            if !self.next(outer)? {
                                self.set_loop(id, LoopPhase::Done, false);
                                return Ok(false);
                            }
                            self.rescan(inner);
                            self.set_loop(id, LoopPhase::Inner, false);
                        }
                        LoopPhase::Inner => {
                            if !self.next(inner)? {
                                self.set_loop(id, LoopPhase::Outer, matched);
                                if !matched && jointype.fills_outer() {
                                    self.cx.null_extend(self.bound, inner_relids);
                                    if self.cx.equal(self.bound, &other_quals)? {
                                        return Ok(true);
                                    }
                                }
                                continue;
                            }
                            if !self.cx.equal(self.bound, &join_quals)? {
                                continue;
                            }
                            if jointype == JoinType::Anti {
                                self.set_loop(id, LoopPhase::Outer, true);
                                continue;
                            }
                            let phase = if single_match {
                                LoopPhase::Outer
                            } else {
                                LoopPhase::Inner
                            };
                            self.set_loop(id, phase, true);
                            if self.cx.equal(self.bound, &other_quals)? {
                                return Ok(true);
                            }
                        }
                    }
                }
            }
            PathKind::HashJoin {
                jointype,
                outer,
                inner,
                clauses,
                hash,
                ..
            } => self.hash_join(id, jointype, (outer, inner), &clauses, &hash),
            PathKind::Unique { .. } | PathKind::Memoize { .. } | PathKind::MergeJoin { .. } => {
                Err(crate::sql_err!(
                    crate::sql::eval::sqlstate::INTERNAL_ERROR,
                    "a join plan node without a pull node"
                ))
            }
        }
    }

    fn set_loop(&mut self, id: u32, to: LoopPhase, met: bool) {
        if let Record::NestLoop { phase, matched } = &mut self.records[id as usize] {
            *phase = to;
            *matched = met;
        }
    }

    /// Starts path `id` again for the current outer rows: their bytes now
    /// end the arena, so no row of the path's is held.
    fn rescan(&mut self, id: u32) {
        match self.cx.model.plan.path(id).kind {
            PathKind::Scan { .. } => {
                if let Record::Scan { record, mark } = &mut self.records[id as usize] {
                    record.node.rescan();
                    *mark = None;
                }
            }
            PathKind::Material { .. } => {
                if let Record::Material { at, mark, .. } = &mut self.records[id as usize] {
                    *at = 0;
                    *mark = None;
                }
            }
            PathKind::NestLoop { outer, .. } => {
                self.set_loop(id, LoopPhase::Outer, false);
                self.rescan(outer);
            }
            PathKind::HashJoin { outer, inner, .. } => {
                // An inner input reading the values of an outer row above
                // changes at each rescan, which builds the table again
                // (`ExecReScanHashJoin` with the inner plan's `chgParam`).
                let rebuild = self.cx.model.plan.path(inner).param != Relids::EMPTY;
                hash_record(self.records, id).restart(rebuild);
                self.rescan(outer);
            }
            _ => {}
        }
    }

    /// Releases the bytes of a path's previous row and marks where its next
    /// one starts.
    fn release(arena: &Arena, mark: &mut Option<ArenaMark>) {
        if let Some(previous) = mark.take() {
            // SAFETY: the previous row and everything read after it are no
            // longer bound: the caller moves this path on.
            unsafe { arena.rewind_to(previous) };
        }
        *mark = Some(arena.mark());
    }

    fn scan(&mut self, id: u32, rel: usize) -> Result<bool, SqlError> {
        let cx = self.cx;
        let Record::Scan { record, mark } = &mut self.records[id as usize] else {
            unreachable!("a scan's record");
        };
        Self::release(cx.arena, mark);
        let ScanRecord {
            node,
            param,
            outer: outer_values,
            outer_forms,
        } = record;
        // The bound rows of the relations the scan's join clauses read.
        let outer_row;
        let outer: Option<&dyn ColumnLookup<'a>> = if *param != Relids::EMPTY {
            for table in 0..cx.scope.n {
                let start = cx.offsets[table];
                let width = cx.scope.stored_width(table);
                let slots = &mut outer_values[start..start + width];
                match self.bound[table] {
                    Some(row) if param.contains(table as u32) => {
                        let columns = cx.columns_of(row.values);
                        slots[..columns.len()].copy_from_slice(columns);
                        slots[columns.len()..].fill(Datum::Null);
                    }
                    _ => slots.fill(Datum::Null),
                }
            }
            cx.flat_forms(self.bound, Some(*param), outer_forms);
            outer_row = JoinRow {
                scope: cx.scope,
                values: JoinRowValues::Flat {
                    values: outer_values,
                    offsets: cx.offsets,
                    forms: Some(outer_forms),
                },
                rowids: &[],
                visible: cx.scope.n,
            };
            Some(&outer_row)
        } else {
            None
        };
        if !node.next(
            cx.storage,
            CursorScratch::Reader(self.buffers),
            cx.arena,
            cx.params,
            cx.hooks,
            outer,
        )? {
            return Ok(false);
        }
        let columns = node.values();
        let values = cx
            .arena
            .alloc_slice_with(columns.len() + cx.model.expressions.len(), |_| Datum::Null)
            .map_err(|_| arena_full())?;
        values[..columns.len()].copy_from_slice(columns);
        // The relation's expression members, computed by its scan.
        let row = node.row();
        for (at, expression) in cx.model.expressions.iter().enumerate() {
            if expression.of(rel) {
                values[columns.len() + at] = crate::sql::eval::eval_full(
                    expression.expression,
                    cx.arena,
                    cx.params,
                    &row,
                    cx.hooks,
                )?;
            }
        }
        self.bound[rel] = Some(BaseRow {
            values,
            forms: node.stored_forms(cx.arena)?,
            rowid: node.rowid(),
        });
        Ok(true)
    }

    /// A `Materialize`: its input's rows, read whole at its first row and
    /// kept for every rescan (`ExecMaterial`, `ExecReScanMaterial`); the
    /// general executor reads them whole too.
    fn material(&mut self, id: u32, input: u32) -> Result<bool, SqlError> {
        let cx = self.cx;
        let relids = cx.model.plan.path(input).relids;
        while matches!(
            self.records[id as usize],
            Record::Material { filled: false, .. }
        ) {
            if !self.next(input)? {
                if let Record::Material { filled, .. } = &mut self.records[id as usize] {
                    *filled = true;
                }
                break;
            }
            // Each relation's values encoded into the store.
            let Record::Material { store, .. } = &mut self.records[id as usize] else {
                unreachable!("a materialize's record");
            };
            store.save(cx, self.bound, relids)?;
        }
        let Record::Material {
            store,
            width,
            at,
            mark,
            ..
        } = &mut self.records[id as usize]
        else {
            unreachable!("a materialize's record");
        };
        Self::release(cx.arena, mark);
        let first = *at * *width;
        if first >= store.entries.len() {
            return Ok(false);
        }
        *at += 1;
        store.load(first, *width, cx, self.bound)?;
        Ok(true)
    }

    /// A hash join as `ExecHashJoin` runs it: the inner rows are built
    /// into a table first, or after the first outer row when the join
    /// returns unmatched outer rows or its outer input starts cheaper than
    /// the table costs; batch 0's outer rows probe it as they are read and
    /// the others are saved to their batch; each later batch is loaded and
    /// probed in turn, each batch's unmatched inner rows returning after
    /// its outer rows. A one-batch table is kept and probed again at a
    /// rescan.
    fn hash_join(
        &mut self,
        id: u32,
        jointype: JoinType,
        (outer, inner): (u32, u32),
        clauses: &ClauseList,
        hash: &ClauseList,
    ) -> Result<bool, SqlError> {
        let cx = self.cx;
        let path = *cx.model.plan.path(id);
        let outer_path = *cx.model.plan.path(outer);
        let inner_path = *cx.model.plan.path(inner);
        let (outer_relids, inner_relids) = (outer_path.relids, inner_path.relids);
        let keys = cx.sides(hash, outer_relids);
        let (join_quals, other_quals) = cx.split(clauses, path.relids, jointype, hash);
        let quals = (&join_quals, &other_quals);
        let (fill_outer, fill_inner) = (jointype.fills_outer(), jointype.fills_inner());
        loop {
            let record = hash_record(self.records, id);
            match record.phase {
                HashPhase::Done => return Ok(false),
                HashPhase::Start => {
                    if record.frozen {
                        let table = record.table.as_mut().expect("a kept table");
                        table.reset_matches();
                        record.phase = if table.memory_empty() && !fill_outer {
                            HashPhase::Done
                        } else {
                            HashPhase::Probe
                        };
                        continue;
                    }
                    record.table = None;
                    record.store.clear();
                    let built_first =
                        fill_inner || (!fill_outer && outer_path.startup >= inner_path.total);
                    record.phase = if built_first {
                        HashPhase::Build {
                            outer_pending: false,
                        }
                    } else {
                        HashPhase::OuterFirst
                    };
                    self.rescan(inner);
                }
                HashPhase::OuterFirst => {
                    record.inner_mark = None;
                    if !self.next(outer)? {
                        hash_record(self.records, id).phase = HashPhase::Done;
                        continue;
                    }
                    hash_record(self.records, id).phase = HashPhase::Build {
                        outer_pending: true,
                    };
                }
                HashPhase::Build { outer_pending } => {
                    if record.table.is_none() {
                        let sizing = super::super::index_paths::joins::choose_hash_table_size(
                            inner_path.rows,
                            inner_path.width,
                            cx.skewed(&keys),
                            cx.model.plan.settings,
                        );
                        let mut table = hash::BatchStore::new(cx.storage.plan_build(), sizing)?;
                        // `ExecHashBuildSkewHash`: the outer key's most common
                        // values, when the join hashes one key of a stored
                        // column into several batches.
                        if sizing.batches > 1 && cx.skewed(&keys) {
                            cx.build_skew(&mut table, keys[0].0, sizing)?;
                        }
                        record.table = Some(table);
                    }
                    // A wait while an inner row is read resumes here, every
                    // row before it in the table.
                    while self.next(inner)? {
                        let Some(value) = cx.key_hash(self.bound, &keys, false, fill_inner)? else {
                            continue;
                        };
                        let size = 16 + cx.tuple_len(self.bound, inner)?;
                        let record = hash_record(self.records, id);
                        let row = record.store.save(cx, self.bound, inner_relids)?;
                        let table = record.table.as_mut().expect("a table being built");
                        table.insert(hash::Hashed {
                            row,
                            hash: value,
                            size,
                        })?;
                        table.total += 1;
                    }
                    let record = hash_record(self.records, id);
                    let table = record.table.as_mut().expect("a built table");
                    table.finish_build()?;
                    if table.total == 0 && !fill_outer {
                        // A rescan probes the empty table with every outer
                        // row.
                        record.frozen = table.batches == 1;
                        record.phase = HashPhase::Done;
                        continue;
                    }
                    record.phase = HashPhase::Probe;
                    if outer_pending && !self.begin_probe(id, &keys, fill_outer, outer_relids)? {
                        continue;
                    }
                }
                HashPhase::Probe => {
                    if record.probe.is_none() {
                        record.inner_mark = None;
                        if !self.next(outer)? {
                            self.end_first_batch(id, fill_inner)?;
                            continue;
                        }
                        if !self.begin_probe(id, &keys, fill_outer, outer_relids)? {
                            continue;
                        }
                    }
                    if self.probe_row(id, jointype, &keys, inner_relids, quals)? {
                        return Ok(true);
                    }
                }
                HashPhase::Fill { then } => {
                    let Some(&row) = record.fill.as_slice().get(record.at) else {
                        record.fill.clear();
                        if let Some(mark) = record.inner_mark.take() {
                            // SAFETY: the last returned row is done with: the
                            // reader asks for the next only then.
                            unsafe { cx.arena.rewind_to(mark) };
                        }
                        record.phase = match then {
                            AfterFill::Done => HashPhase::Done,
                            AfterFill::NextBatch => HashPhase::NextBatch,
                        };
                        continue;
                    };
                    record.at += 1;
                    // Each unmatched inner row returns with NULLs for the
                    // outer relations when the join's filters pass it.
                    Self::release(cx.arena, &mut record.inner_mark);
                    let count = relation_count(inner_relids, cx.scope.n);
                    record.store.load(row as usize, count, cx, self.bound)?;
                    cx.null_extend(self.bound, outer_relids);
                    if cx.equal(self.bound, &other_quals)? {
                        return Ok(true);
                    }
                }
                HashPhase::NextBatch => {
                    let table = record.table.as_mut().expect("a table of several batches");
                    table.skew_enabled = false;
                    match table.next_batch(
                        table.current,
                        record.outer_start,
                        fill_outer,
                        fill_inner,
                    ) {
                        None => record.phase = HashPhase::Done,
                        Some(batch) => {
                            table.load(batch, &mut record.outer_rows)?;
                            record.at = 0;
                            record.probe = None;
                            record.phase = HashPhase::ProbeBatch;
                        }
                    }
                }
                HashPhase::ProbeBatch => {
                    if record.probe.is_none() {
                        let table = record.table.as_mut().expect("a loaded batch");
                        let Some(&saved) = record.outer_rows.as_slice().get(record.at) else {
                            record.outer_rows.clear();
                            record.phase = if fill_inner {
                                table.unmatched(&mut record.fill)?;
                                record.at = 0;
                                HashPhase::Fill {
                                    then: AfterFill::NextBatch,
                                }
                            } else {
                                HashPhase::NextBatch
                            };
                            continue;
                        };
                        record.at += 1;
                        // Batches grew since the row was saved: it moves on
                        // to its new batch.
                        let batch = table.batch_of(saved.hashed.hash);
                        if batch != table.current {
                            table.save_outer(batch, saved.hashed)?;
                            continue;
                        }
                        Self::release(cx.arena, &mut record.outer_mark);
                        record.inner_mark = None;
                        let count = relation_count(outer_relids, cx.scope.n);
                        record
                            .store
                            .load(saved.hashed.row as usize, count, cx, self.bound)?;
                        let chain = table.chain(Some(saved.hashed.hash), None);
                        record.probe = Some(HashProbe {
                            chain,
                            matched: false,
                            outer_null: cx.has_null(self.bound, &keys, true),
                            filled: false,
                        });
                    }
                    if self.probe_row(id, jointype, &keys, inner_relids, quals)? {
                        return Ok(true);
                    }
                }
            }
        }
    }

    /// Starts the bound outer row's probe of batch 0, or saves the row to
    /// its later batch: `false` when the row probes nothing now. A row
    /// with a NULL key matches nothing and, unless the join returns
    /// unmatched outer rows, has no hash value and is skipped.
    fn begin_probe(
        &mut self,
        id: u32,
        keys: &KeySides,
        fill_outer: bool,
        outer_relids: Relids,
    ) -> Result<bool, SqlError> {
        let cx = self.cx;
        let Some(value) = cx.key_hash(self.bound, keys, true, fill_outer)? else {
            return Ok(false);
        };
        let record = hash_record(self.records, id);
        let table = record.table.as_mut().expect("a built table");
        let skew = table.skew_bucket(value);
        if skew.is_none() && table.batch_of(value) != 0 {
            let batch = table.batch_of(value);
            let row = record.store.save(cx, self.bound, outer_relids)?;
            table.save_outer(
                batch,
                hash::Hashed {
                    row,
                    hash: value,
                    size: 0,
                },
            )?;
            return Ok(false);
        }
        let chain = table.chain(Some(value), skew);
        record.probe = Some(HashProbe {
            chain,
            matched: false,
            outer_null: cx.has_null(self.bound, keys, true),
            filled: false,
        });
        Ok(true)
    }

    /// Batch 0's outer rows ended: a one-batch table is kept for rescans,
    /// and the unmatched inner rows return before the next batch.
    fn end_first_batch(&mut self, id: u32, fill_inner: bool) -> Result<(), SqlError> {
        let record = hash_record(self.records, id);
        let table = record.table.as_mut().expect("a built table");
        let then = if table.batches == 1 {
            record.frozen = true;
            AfterFill::Done
        } else {
            record.outer_start = table.batches;
            AfterFill::NextBatch
        };
        record.phase = if fill_inner {
            table.unmatched(&mut record.fill)?;
            record.at = 0;
            HashPhase::Fill { then }
        } else {
            match then {
                AfterFill::Done => HashPhase::Done,
                AfterFill::NextBatch => HashPhase::NextBatch,
            }
        };
        Ok(())
    }

    /// Moves the current outer row's probe to its next joined row
    /// (`HJ_SCAN_BUCKET`), then returns the row with NULLs for the inner
    /// relations when it matched none and the join returns those
    /// (`HJ_FILL_OUTER_TUPLE`). `Ok(false)` once the probe ends.
    fn probe_row(
        &mut self,
        id: u32,
        jointype: JoinType,
        keys: &KeySides,
        inner_relids: Relids,
        (join_quals, other_quals): (&ClauseList, &ClauseList),
    ) -> Result<bool, SqlError> {
        let cx = self.cx;
        loop {
            let record = hash_record(self.records, id);
            let table = record.table.as_mut().expect("a probed table");
            let probe = record.probe.as_mut().expect("a probe");
            let candidate = if probe.outer_null {
                None
            } else {
                table.next_candidate(&mut probe.chain)
            };
            let Some(candidate) = candidate else {
                if probe.matched || probe.filled || !jointype.fills_outer() {
                    record.probe = None;
                    return Ok(false);
                }
                probe.filled = true;
                cx.null_extend(self.bound, inner_relids);
                if cx.equal(self.bound, other_quals)? {
                    return Ok(true);
                }
                continue;
            };
            Self::release(cx.arena, &mut record.inner_mark);
            let count = relation_count(inner_relids, cx.scope.n);
            record
                .store
                .load(candidate.hashed.row as usize, count, cx, self.bound)?;
            if cx.has_null(self.bound, keys, false)
                || cx.compare_sides(self.bound, self.bound, keys)? != Ordering::Equal
                || !cx.equal(self.bound, join_quals)?
            {
                continue;
            }
            let record = hash_record(self.records, id);
            let table = record.table.as_mut().expect("a probed table");
            let probe = record.probe.as_mut().expect("a probe");
            probe.matched = true;
            // A right semi join returns each inner row at its first match.
            if table.mark_matched(candidate) && jointype == JoinType::RightSemi {
                continue;
            }
            match jointype {
                JoinType::Anti => {
                    probe.chain = hash::Chain::Empty;
                    continue;
                }
                JoinType::RightAnti => continue,
                _ => {}
            }
            // A semi join returns an outer row at its first match.
            if jointype == JoinType::Semi {
                probe.chain = hash::Chain::Empty;
            }
            if cx.equal(self.bound, other_quals)? {
                return Ok(true);
            }
        }
    }
}
