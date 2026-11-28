//! The scan node of a plan PostgreSQL's join search chose: one plain table
//! read by its chosen sequential, index, index-only or bitmap heap path, a
//! row at a time.
//!
//! The node keeps everything its scan needs between rows in its own record:
//! the sequential scan's [`RowCursor`], or the path's candidates in their
//! scan order with the [`CandidateCursor`] reading them, the scan's filter,
//! the chosen path's `EXPLAIN ANALYZE` counts and the rows a `LIMIT` still
//! takes. A storage wait, while the candidates are found or while a row is
//! read, returns to the caller with that state intact; the same call resumes
//! it without counting anything twice. A rescan finds the candidates again
//! for the new outer row and reuses the node's storage for them.
//!
//! The node holds no `Storage` reference: every call takes the storage it
//! reads, so a retained reader that keeps the node between calls borrows
//! none. An index's statistics counter is captured for each read only.

use super::*;
use crate::storage::{CandidateCursor, Candidates, CursorScratch, RowCursor};

/// The candidates of an index, index-only or bitmap path, in its order.
/// They live in the statement arena's persistent tail, which front rewinds
/// never reclaim; a rescan reuses them and grows them geometrically, so
/// rescans hold at most twice the most candidates any one execution found.
struct CandidateRows<'a> {
    cursor: CandidateCursor,
    rowids: &'a mut [u64],
    len: usize,
    /// The index whose statistics each row read counts in, captured
    /// again for each read.
    index: Option<crate::storage::IndexStatisticsTicket>,
}

/// Where the rows of the running execution come from.
#[derive(Clone, Copy)]
enum Rows {
    /// Before the first execution.
    None,
    /// The table's rows in a sequential scan's order.
    Sequential,
    Candidates,
    /// A condition that reads no column of the table is false: nothing is
    /// read.
    Empty,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum State {
    /// The next call finds the execution's rows: the first call after
    /// [`PathScan::rescan`], or a call after that one waited.
    Opening,
    Reading,
    /// The execution returned its last row.
    Done,
}

/// The nodes one execution counts in under `EXPLAIN ANALYZE`, open while it
/// runs.
#[allow(dead_code, reason = "the loops end when dropped")]
struct ExecutionLoops<'a>(instrument::Loop, instrument::Loop, instrument::Loops<'a>);

/// The scan of one plain table by its chosen path.
pub(crate) struct PathScan<'a> {
    txid: u32,
    slot: usize,
    scope: QueryScope<'a>,
    path: super::super::index_paths::ScanPath<'a>,
    /// The scan's condition as planned, whose constant parts decide whether
    /// it reads anything.
    condition: Option<&'a Expr<'a>>,
    /// The condition each row is checked by: simplified, folded and ordered
    /// by cost once.
    filter: Option<&'a Expr<'a>>,
    schema: &'a [ColType],
    /// The current row's values, their stored forms and its identity,
    /// valid until the next call.
    values: &'a mut [Datum<'a>],
    forms: &'a mut [StoredForm],
    /// Whether a column of the table can hold a value with a stored form.
    formed: bool,
    rowid: [Option<u64>; 1],
    sequential: Option<RowCursor>,
    candidates: Option<CandidateRows<'a>>,
    rows: Rows,
    state: State,
    /// The rows a `LIMIT` above an ordered path still takes in this
    /// execution.
    remaining: Option<u64>,
    counts: Option<ChosenScanCounts>,
    scan_node: instrument::Node,
    bitmap_node: instrument::Node,
    tree_nodes: &'a [instrument::Node],
    loops: Option<ExecutionLoops<'a>>,
    /// The rows the scan's `Filter` removed since [`Self::take_removed`]
    /// last read them, counted as its node counts them.
    removed: core::cell::Cell<u64>,
}

impl<'a> PathScan<'a> {
    /// The rows the scan's `Filter` removed since the last call.
    pub(crate) fn take_removed(&self) -> u64 {
        self.removed.replace(0)
    }

    /// The scan of `from`, a plain table, by `path` under `condition`.
    /// Its nodes count in the current `EXPLAIN ANALYZE` block. The first
    /// call to [`Self::next`] after [`Self::rescan`] starts an execution.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        storage: &Storage,
        txid: u32,
        from: &'a FromClause<'a>,
        condition: Option<&'a Expr<'a>>,
        path: super::super::index_paths::ScanPath<'a>,
        arena: &'a Arena,
        params: &[Datum<'a>],
        hooks: &EvalHooks<'_, 'a>,
    ) -> Result<Self, SqlError> {
        let scope = QueryScope::resolve_exec(
            storage,
            from,
            condition,
            txid,
            arena,
            params,
            hooks.sequences,
        )?;
        if scope.n != 1 || scope.slots[0] == usize::MAX || scope.derived[0].is_some() {
            return Err(sql_err!(
                sqlstate::INTERNAL_ERROR,
                "a chosen scan path reads one table"
            ));
        }
        admit_sources(
            storage,
            &scope,
            txid,
            PaxReadDemand::full_row(PaxFullRowReason::WildcardProjection),
        )?;
        validate_materialized_view_reads(storage, &scope, condition, arena, txid)?;
        let filter = match condition {
            Some(condition) => {
                let simplified = simplify_qual(condition, arena)?;
                let simplified =
                    super::super::plan::fold_catalog_inputs(simplified, arena, params, hooks)?;
                Some(reorder_qual(
                    fold_null(simplified, &scope, arena)?,
                    &scope,
                    storage,
                    txid,
                    arena,
                )?)
            }
            None => None,
        };
        let slot = scope.slots[0];
        let definition = scope.defs.stored(0).expect("a chosen path scans a table");
        let mut schema = [ColType::Bool; MAX_COLUMNS];
        let n_columns = definition.schema(&mut schema);
        let schema = arena
            .alloc_slice_copy(&schema[..n_columns])
            .map_err(|_| arena_full())?;
        let values = arena
            .alloc_slice_with(scope.stored_width(0).max(n_columns), |_| Datum::Null)
            .map_err(|_| arena_full())?;
        let forms = arena
            .alloc_slice_with(values.len(), |_| StoredForm::Inline)
            .map_err(|_| arena_full())?;
        let formed = schema
            .iter()
            .any(|&ctype| rowenc::column_carries_stored_form(ctype));
        use super::super::index_paths::ScanKind;
        let (scan_node, bitmap_node, tree_nodes, counts) = if instrument::active() {
            let scan_node = instrument::node(instrument::Role::Scan(0));
            let bitmap_node = if path.kind == ScanKind::Bitmap {
                instrument::node(instrument::Role::BitmapIndex(0))
            } else {
                instrument::Node::NONE
            };
            // A bitmap tree counts each of its nodes.
            let tree_nodes: &'a [instrument::Node] = if path.bitmap.len() > 1 {
                arena
                    .alloc_slice_with(path.bitmap.len(), |at| {
                        instrument::node(instrument::Role::BitmapNode(
                            u16::try_from(at).unwrap_or(u16::MAX),
                        ))
                    })
                    .map_err(|_| arena_full())?
            } else {
                &[]
            };
            let counts = chosen_scan_counts(
                storage,
                txid,
                slot,
                &path,
                scan_node,
                bitmap_node,
                tree_nodes,
                arena,
            )?;
            (scan_node, bitmap_node, tree_nodes, Some(counts))
        } else {
            (
                instrument::Node::NONE,
                instrument::Node::NONE,
                &[][..],
                None,
            )
        };
        Ok(Self {
            txid,
            slot,
            scope,
            path,
            condition,
            filter,
            schema,
            values,
            forms,
            formed,
            rowid: [None],
            sequential: None,
            candidates: None,
            rows: Rows::None,
            state: State::Done,
            remaining: None,
            counts,
            scan_node,
            bitmap_node,
            tree_nodes,
            loops: None,
            removed: core::cell::Cell::new(0),
        })
    }

    /// Ends the running execution; the next call to [`Self::next`] starts
    /// another, for the outer row its caller passes then.
    pub(crate) fn rescan(&mut self) {
        self.loops = None;
        self.state = State::Opening;
    }

    /// Ends the running execution where it is, as a node above that stops
    /// reading it does.
    pub(crate) fn stop(&mut self) {
        self.loops = None;
        self.state = State::Done;
    }

    /// The current row, as a one-table joined row.
    pub(crate) fn row(&self) -> JoinRow<'_, 'a, 'a> {
        JoinRow {
            scope: &self.scope,
            values: JoinRowValues::Flat {
                values: self.values,
                offsets: &[0],
                forms: Some(self.forms),
            },
            rowids: &self.rowid,
            visible: 1,
        }
    }

    /// The current row's values.
    pub(crate) fn values(&self) -> &[Datum<'a>] {
        &self.values[..self.scope.stored_width(0)]
    }

    /// The stored forms of the current row's values, in `arena`; empty when
    /// every one is as `heap_form_tuple` forms it.
    pub(crate) fn stored_forms(&self, arena: &'a Arena) -> Result<&'a [StoredForm], SqlError> {
        let width = self.scope.stored_width(0);
        if self.forms[..width]
            .iter()
            .all(|&form| form == StoredForm::Inline)
        {
            return Ok(&[]);
        }
        let forms = arena
            .alloc_slice_copy(&self.forms[..width])
            .map_err(|_| arena_full())?;
        Ok(forms)
    }

    /// The row identity of the current row.
    pub(crate) fn rowid(&self) -> Option<u64> {
        self.rowid[0]
    }

    /// Moves to the next row of the execution that passes the scan's
    /// condition; `Ok(false)` at its end. `outer` holds the values of the
    /// outer relations the condition reads, the same for every call of one
    /// execution. A failed call, a storage wait among them, keeps the
    /// execution's position: the same call resumes it.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn next(
        &mut self,
        storage: &Storage,
        mut scratch: CursorScratch<'_>,
        arena: &'a Arena,
        params: &[Datum<'a>],
        hooks: &EvalHooks<'_, 'a>,
        outer: Option<&dyn ColumnLookup<'a>>,
    ) -> Result<bool, SqlError> {
        if self.state == State::Opening {
            self.open(storage, arena, params, hooks, outer)?;
        }
        loop {
            if self.state == State::Done {
                self.loops = None;
                return Ok(false);
            }
            // A row the condition rejects releases its bytes.
            let mark = arena.mark();
            let read = self.read_row(storage, &mut scratch, arena);
            let passed = match read.and_then(|found| {
                if !found {
                    return Ok(None);
                }
                self.check(arena, params, hooks, outer).map(Some)
            }) {
                Ok(Some(passed)) => passed,
                Ok(None) => {
                    self.state = State::Done;
                    continue;
                }
                Err(error) => {
                    // SAFETY: nothing borrows the bytes of a row not returned.
                    unsafe { arena.rewind_to(mark) };
                    return Err(error);
                }
            };
            if !passed {
                // SAFETY: the rejected row's values are overwritten by the
                // next row before anything reads them.
                unsafe { arena.rewind_to(mark) };
                continue;
            }
            // An ordered scan under a `LIMIT` ends with the limit's rows.
            match self.remaining {
                Some(1) => self.state = State::Done,
                Some(rows) => self.remaining = Some(rows - 1),
                None => {}
            }
            return Ok(true);
        }
    }

    /// Finds the execution's rows. A wait leaves the node opening, its
    /// loops started and nothing counted twice.
    fn open(
        &mut self,
        storage: &Storage,
        arena: &'a Arena,
        params: &[Datum<'a>],
        hooks: &EvalHooks<'_, 'a>,
        outer: Option<&dyn ColumnLookup<'a>>,
    ) -> Result<(), SqlError> {
        if self.loops.is_none() {
            if !one_time_filter_passes(self.condition, &self.scope, arena, params, hooks, outer)? {
                self.rows = Rows::Empty;
                self.state = State::Done;
                return Ok(());
            }
            if let Some(counts) = self.counts {
                // SAFETY: the page bits are the node's own, in its arena;
                // nothing else borrows them between rows.
                unsafe { &mut *counts.seen }.fill(0);
            }
            self.loops = Some(ExecutionLoops(
                self.scan_node.start(),
                self.bitmap_node.start(),
                instrument::Loops::start(self.tree_nodes),
            ));
        }
        let mark = arena.mark();
        let found = self.find_rows(storage, arena, params, hooks, outer);
        // SAFETY: the candidates are copied into the node's tail storage;
        // nothing else the search allocated is kept.
        unsafe { arena.rewind_to(mark) };
        let rows = found?;
        if let Rows::Sequential = rows {
            self.sequential
                .as_mut()
                .expect("a sequential execution opened its cursor")
                .rewind(arena)?;
        }
        self.rows = rows;
        self.remaining = self.path.stop_after.filter(|&rows| rows > 0);
        self.state = State::Reading;
        Ok(())
    }

    /// Where the execution's rows come from, as the general executor's scan
    /// of the path decides it. The scan is recorded in the table's
    /// statistics once nothing can wait any more.
    fn find_rows(
        &mut self,
        storage: &Storage,
        arena: &'a Arena,
        params: &[Datum<'a>],
        hooks: &EvalHooks<'_, 'a>,
        outer: Option<&dyn ColumnLookup<'a>>,
    ) -> Result<Rows, SqlError> {
        use super::super::index_paths::ScanKind;
        let (txid, slot) = (self.txid, self.slot);
        // The rows an index finds for the outer row's keys, a superset the
        // condition filters. `EXPLAIN ANALYZE` counts the path's own reads:
        // the rows an index finds for the path's own index conditions,
        // every row they reach among them.
        let access = if self.path.kind == ScanKind::Seq {
            None
        } else if instrument::active() {
            match path_index_conditions(&self.path, arena)? {
                Some(conditions) => indexed_candidates(
                    storage,
                    &self.scope,
                    txid,
                    Some(conditions),
                    arena,
                    params,
                    hooks,
                    outer,
                )?
                .map(IndexedCandidates::unordered),
                None => None,
            }
        } else {
            indexed_candidates(
                storage,
                &self.scope,
                txid,
                self.filter,
                arena,
                params,
                hooks,
                outer,
            )?
            .map(IndexedCandidates::unordered)
        };
        // A sequential path, or a single row whose order nothing decides,
        // streams saved rows in physical order.
        if self.path.kind == ScanKind::Seq
            || access.is_none() && self.path.single_row && storage.spill_generation_count(slot) != 0
        {
            if self.sequential.is_none() {
                let mut cursor = RowCursor::closed();
                cursor.open(storage, slot, txid, arena)?;
                self.sequential = Some(cursor);
            }
            storage.record_relation_scan(
                txid,
                slot,
                None,
                0,
                crate::sql::datetime::now_micros(),
            )?;
            return Ok(Rows::Sequential);
        }
        // The candidates the snapshot sees, by row identity: rows an index
        // condition reaches go in identity order, as its index entries keep
        // an updated row's first position.
        let count = match &access {
            Some(access) => access.rowids.len(),
            None => storage.visible_row_count(slot, txid)?,
        };
        let found = arena
            .alloc_slice_with(count, |_| 0u64)
            .map_err(|_| arena_full())?;
        let mut fill = 0usize;
        match &access {
            Some(access) => {
                for &rowid in access.rowids {
                    let Some(state) = storage.row_state(slot, rowid)? else {
                        continue;
                    };
                    if storage
                        .visible_row_home(slot, rowid, state, txid)?
                        .is_some()
                    {
                        found[fill] = rowid;
                        fill += 1;
                    }
                }
            }
            None => storage.for_each_row_state(slot, &mut |rowid, state| {
                if storage
                    .visible_row_home(slot, rowid, state, txid)?
                    .is_some()
                {
                    let cell = found.get_mut(fill).ok_or_else(|| {
                        sql_err!(sqlstate::INTERNAL_ERROR, "scan row count changed")
                    })?;
                    *cell = rowid;
                    fill += 1;
                }
                Ok(core::ops::ControlFlow::Continue(()))
            })?,
        }
        let found = &mut found[..fill];
        found.sort_unstable();
        // An index scan reads them by its keys.
        let order: Option<&[usize]> =
            if matches!(self.path.kind, ScanKind::Index | ScanKind::IndexOnly) {
                let found = &*found;
                Some(index_key_permutation(
                    storage,
                    &self.scope,
                    txid,
                    &self.path,
                    found.len(),
                    arena,
                    |position| {
                        let rowid = found[position];
                        let vanished =
                            || sql_err!(sqlstate::INTERNAL_ERROR, "scan candidate vanished");
                        let state = storage.row_state(slot, rowid)?.ok_or_else(vanished)?;
                        let home = storage
                            .visible_row_home(slot, rowid, state, txid)?
                            .ok_or_else(vanished)?;
                        storage.row_bytes(slot, rowid, home, arena)
                    },
                )?)
            } else {
                None
            };
        if access.as_ref().is_none_or(|access| access.scan_executed) {
            let statistics = access
                .as_ref()
                .map(|access| access.statistics(storage))
                .transpose()?;
            storage.record_relation_scan(
                txid,
                slot,
                statistics.as_ref(),
                access.as_ref().map_or(0, |access| access.index_entries),
                crate::sql::datetime::now_micros(),
            )?;
        }
        let rows = self.candidates.get_or_insert_with(|| CandidateRows {
            cursor: CandidateCursor::open(storage, slot, txid),
            rowids: &mut [],
            len: 0,
            index: None,
        });
        if fill > rows.rowids.len() {
            rows.rowids = arena
                .alloc_persistent_slice_with(fill.max(rows.rowids.len() * 2), |_| 0u64)
                .map_err(|_| arena_full())?;
        }
        for (position, rowid) in rows.rowids[..fill].iter_mut().enumerate() {
            *rowid = found[order.map_or(position, |order| order[position])];
        }
        rows.len = fill;
        rows.index = access.map(|access| access.statistics_ticket());
        rows.cursor = CandidateCursor::open(storage, slot, txid);
        Ok(Rows::Candidates)
    }

    /// Reads the execution's next row into the node's values; `Ok(false)`
    /// once none is left.
    fn read_row(
        &mut self,
        storage: &Storage,
        scratch: &mut CursorScratch<'_>,
        arena: &'a Arena,
    ) -> Result<bool, SqlError> {
        let (txid, slot) = (self.txid, self.slot);
        let (rowid, index, representation) = match self.rows {
            Rows::Sequential => {
                let cursor = self
                    .sequential
                    .as_mut()
                    .expect("a sequential execution opened its cursor");
                let Some(row) = cursor.next(scratch.reborrow(), arena, arena, None)? else {
                    return Ok(false);
                };
                let rowid = row.rowid;
                (rowid, None, row.into_representation(arena)?)
            }
            Rows::Candidates => {
                let rows = self
                    .candidates
                    .as_mut()
                    .expect("a candidate execution holds its candidates");
                let Some(row) = rows.cursor.next(
                    storage,
                    arena,
                    Candidates::Rowids(&rows.rowids[..rows.len]),
                )?
                else {
                    return Ok(false);
                };
                (
                    row.rowid,
                    rows.index,
                    crate::storage::SpilledRowRepresentation::Encoded(row.bytes),
                )
            }
            Rows::None | Rows::Empty => return Ok(false),
        };
        let statistics = index
            .map(|ticket| storage.recapture_index_statistics(ticket))
            .transpose()?;
        storage.record_relation_tuple_read(txid, slot, statistics.as_ref())?;
        let n_columns = self.schema.len();
        let encoded = match representation {
            crate::storage::SpilledRowRepresentation::Encoded(bytes) => {
                rowenc::decode(bytes, self.schema, &mut self.values[..n_columns])?;
                bytes
            }
            crate::storage::SpilledRowRepresentation::Values(values) => {
                let len = values.values.len().min(n_columns);
                self.values[..len].copy_from_slice(&values.values[..len]);
                values.encoded
            }
        };
        // The forms the row's encoding records for its leading columns; a
        // column added after the row was written reads its default, formed
        // inline.
        self.forms.fill(StoredForm::Inline);
        if self.formed {
            let width = rowenc::encoded_column_count(encoded)?.min(n_columns);
            rowenc::stored_forms(encoded, &self.schema[..width], &mut self.forms[..width])?;
        }
        refresh_catalog_object_names(storage, txid, &mut self.values[..n_columns], arena)?;
        self.rowid = [Some(rowid)];
        Ok(true)
    }

    /// Whether the current row passes the scan's condition, counting it as
    /// the chosen path and the scan's `Filter` do.
    fn check(
        &self,
        arena: &'a Arena,
        params: &[Datum<'a>],
        hooks: &EvalHooks<'_, 'a>,
        outer: Option<&dyn ColumnLookup<'a>>,
    ) -> Result<bool, SqlError> {
        let row = self.row();
        let chained = Chained { inner: &row, outer };
        let rowid = self.rowid[0];
        if let Some(filter) = self.filter
            && !where_passes(filter, arena, params, &chained, hooks)?
        {
            if chosen_path_reaches(self.counts, rowid, false, arena, params, &chained, hooks)? {
                self.scan_node.filtered();
                self.removed.set(self.removed.get() + 1);
            }
            return Ok(false);
        }
        chosen_path_reaches(self.counts, rowid, true, arena, params, &chained, hooks)?;
        self.scan_node.row();
        Ok(true)
    }
}
