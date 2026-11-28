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
//! A merge join ([`merge`]) reads an outer input in merge order from its
//! path and keeps a sorted input, and always its inner one, in its store; a
//! sorted or hashed `Unique` keeps its input there too, each ordered over
//! its rows decoded at once and released. A `Memoize` keeps its cache in
//! leases ([`memo`]), evicting as PostgreSQL does.
//!
//! Every node of the join search's plans runs here. A retained reader
//! holds a plan scanning no `LATERAL` item ([`pulls`]); the general
//! executor reads every plan ([`ModeledPull::general`], from
//! [`super::run_modeled`]): its consumers keep the joined rows they are
//! passed, so no row is released there, a store keeps its rows bound as
//! read, a `Materialize` reads its input only as far as a loop fetches, as
//! `ExecMaterial` does, and a `LATERAL` item runs its subquery whole for
//! each outer row. Under `EXPLAIN ANALYZE` each node
//! counts its loops and rows, and what its join conditions removed, in the
//! plan's node of its role; an execution's loop opens at its first read and
//! closes when it ends, is rescanned or the reader finishes.

use super::*;
use crate::mem::arena::ArenaMark;
use crate::mem::lease_pool::{LeaseError, LeasePool, MutableSlice};
use crate::storage::{CursorScratch, RowCursorBuffers};
use core::borrow::Borrow;
use std::sync::Arc;

use super::super::instrument;

mod hash;
mod list;
mod memo;
mod merge;

use list::{ListError, NodeList, Space};

/// Whether every node of `model`'s plan runs as a pull node: every node
/// but a scan of a `LATERAL` item, which reads its own subquery.
pub(crate) fn pulls(model: &ModeledJoin<'_>) -> bool {
    fn path_pulls(model: &ModeledJoin<'_>, id: u32) -> bool {
        match model.plan.path(id).kind {
            PathKind::Dummy => true,
            PathKind::Scan { rel, .. } => model.rels[usize::from(rel)].lateral.is_none(),
            PathKind::Material { input }
            | PathKind::Unique { input, .. }
            | PathKind::Memoize { input, .. } => path_pulls(model, input),
            PathKind::NestLoop { outer, inner, .. }
            | PathKind::HashJoin { outer, inner, .. }
            | PathKind::MergeJoin { outer, inner, .. } => {
                path_pulls(model, outer) && path_pulls(model, inner)
            }
        }
    }
    path_pulls(model, model.plan.top)
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
/// lie in the store's bytes (or, with `len` [`KEPT`], its place among the
/// kept rows), and its row identity.
#[derive(Clone, Copy)]
struct StoredRel {
    rel: u8,
    start: u32,
    len: u32,
    rowid: Option<u64>,
}

/// The `len` of a stored relation row kept bound rather than encoded.
const KEPT: u32 = u32::MAX;

/// Rows a node stored, each its relations in order: a `Materialize`'s
/// input rows, or a hash join's inner rows and saved outer rows.
struct MaterialStore<'a> {
    entries: NodeList<'a, StoredRel>,
    bytes: NodeList<'a, u8>,
    /// The general executor's rows, by entry: kept bound as they were read
    /// instead of encoded, since no row it read is released.
    kept: Option<NodeList<'a, BaseRow<'a>>>,
}

fn store_full(error: ListError) -> SqlError {
    error.into_sql("a join node's stored rows")
}

impl<'a> MaterialStore<'a> {
    /// A store in `space`: in leases it encodes its rows, in the general
    /// executor's arena it keeps them bound.
    fn new(space: &Space<'a>) -> Result<Self, SqlError> {
        Ok(Self {
            entries: space.list().map_err(store_full)?,
            bytes: space.list().map_err(store_full)?,
            kept: matches!(space, Space::Arena(_))
                .then(|| space.list().map_err(store_full))
                .transpose()?,
        })
    }

    /// Whether the arena may be rewound past what the store's lists
    /// allocated: only when they live in leases.
    fn releases(&self) -> bool {
        self.kept.is_none()
    }

    fn clear(&mut self) {
        self.entries.clear();
        self.bytes.clear();
        if let Some(kept) = &mut self.kept {
            kept.clear();
        }
    }

    /// The store's bytes, live and dead.
    fn held_bytes(&self) -> usize {
        self.bytes.len()
            + self
                .kept
                .as_ref()
                .map_or(0, |kept| kept.len() * size_of::<BaseRow<'a>>())
    }

    /// Encodes the bound rows of `relids` into the store, or keeps them,
    /// returning the row's first entry.
    fn save(
        &mut self,
        cx: JoinContext<'_, 'a>,
        bound: &[Option<BaseRow<'a>>],
        relids: Relids,
    ) -> Result<u32, SqlError> {
        let first = self.next_entry()?;
        for table in (0..cx.scope.n).filter(|&table| relids.contains(table as u32)) {
            let row = bound[table].expect("a stored row binds its relations");
            if self.kept.is_some() {
                self.keep(table, row)?;
                continue;
            }
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

    /// Appends relation `table`'s row, kept bound.
    fn keep(&mut self, table: usize, row: BaseRow<'a>) -> Result<(), SqlError> {
        let kept = self.kept.as_ref().expect("a store keeping its rows");
        let start = u32::try_from(kept.len()).map_err(|_| {
            crate::sql_err!(
                crate::sql::eval::sqlstate::PROGRAM_LIMIT_EXCEEDED,
                "a join node keeps more than 4294967295 relation rows"
            )
            .plan_build_exhausted()
        })?;
        self.push_entry(StoredRel {
            rel: table as u8,
            start,
            len: KEPT,
            rowid: row.rowid,
        })?;
        let kept = self.kept.as_mut().expect("a store keeping its rows");
        if kept.len() == kept.capacity_entries() {
            let grown = kept
                .capacity_entries()
                .saturating_mul(2)
                .min(kept.max_entries())
                .max(kept.len() + 1);
            kept.reserve_exact(grown).map_err(store_full)?;
        }
        kept.push(row).map_err(store_full)
    }

    /// Binds the `count` relations stored from entry `first` in `bound`,
    /// their values decoded into the arena, or as they were kept.
    fn load(
        &self,
        first: usize,
        count: usize,
        cx: JoinContext<'_, 'a>,
        bound: &mut [Option<BaseRow<'a>>],
    ) -> Result<(), SqlError> {
        for rel in &self.entries.as_slice()[first..first + count] {
            let table = usize::from(rel.rel);
            if rel.len == KEPT {
                let kept = self.kept.as_ref().expect("a store keeping its rows");
                bound[table] = Some(kept.as_slice()[rel.start as usize]);
                continue;
            }
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

    /// Encodes `values` as one stored relation (a `Memoize` key), returning
    /// its entry, also in a store keeping its rows.
    fn save_values(
        &mut self,
        cx: JoinContext<'_, 'a>,
        values: &[Datum<'a>],
    ) -> Result<u32, SqlError> {
        let first = self.next_entry()?;
        let mark = cx.arena.mark();
        let pushed = crate::sql::exec::encode_projected_formed(values, &[], cx.arena)
            .and_then(|encoded| self.push_bytes(encoded));
        if self.releases() {
            // SAFETY: the encoding was copied into the store's lease.
            unsafe { cx.arena.rewind_to(mark) };
        }
        let (start, len) = pushed?;
        self.push_entry(StoredRel {
            rel: 0,
            start,
            len,
            rowid: None,
        })?;
        Ok(first)
    }

    /// The `count` values [`Self::save_values`] stored at `entry`, decoded
    /// into the arena.
    fn load_values<'v>(
        &self,
        entry: usize,
        count: usize,
        arena: &'v Arena,
    ) -> Result<&'v [Datum<'v>], SqlError> {
        let rel = self.entries.as_slice()[entry];
        let bytes = arena
            .alloc_slice_copy(
                &self.bytes.as_slice()[rel.start as usize..(rel.start + rel.len) as usize],
            )
            .map_err(|_| arena_full())?;
        let values = arena
            .alloc_slice_with(count, |_| Datum::Null)
            .map_err(|_| arena_full())?;
        for (column, value) in values.iter_mut().enumerate() {
            *value = crate::sql::exec::decode_projected_col_record(bytes, column, arena)?;
        }
        Ok(values)
    }

    /// The number the next stored relation takes.
    fn next_entry(&self) -> Result<u32, SqlError> {
        u32::try_from(self.entries.len()).map_err(|_| {
            crate::sql_err!(
                crate::sql::eval::sqlstate::PROGRAM_LIMIT_EXCEEDED,
                "a join node stores more than 4294967295 relation rows"
            )
            .plan_build_exhausted()
        })
    }

    /// The bytes the `count` relations stored from `first` hold.
    fn bytes_of(&self, first: usize, count: usize) -> usize {
        self.entries.as_slice()[first..first + count]
            .iter()
            .map(|rel| match rel.len {
                KEPT => size_of::<BaseRow<'a>>(),
                len => len as usize,
            })
            .sum()
    }

    /// Copies the `count` relations `from` stored at `first` to this
    /// store's end, returning their new first entry.
    fn copy_from(
        &mut self,
        from: &MaterialStore<'a>,
        first: usize,
        count: usize,
    ) -> Result<u32, SqlError> {
        let copied = self.next_entry()?;
        for rel in &from.entries.as_slice()[first..first + count] {
            if rel.len == KEPT {
                let kept = from.kept.as_ref().expect("a store keeping its rows");
                self.keep(usize::from(rel.rel), kept.as_slice()[rel.start as usize])?;
                continue;
            }
            let (start, len) = self.push_bytes(
                &from.bytes.as_slice()[rel.start as usize..(rel.start + rel.len) as usize],
            )?;
            self.push_entry(StoredRel { start, len, ..*rel })?;
        }
        Ok(copied)
    }
}

/// Rows a node keeps in its store and reads in the order its sort or hash
/// table gives them. The order is computed over every row decoded into the
/// arena at once, which is released after; each read decodes one row.
struct OrderedRows<'a> {
    store: MaterialStore<'a>,
    /// The stored rows' numbers in read order.
    order: NodeList<'a, u32>,
    /// Relations per stored row.
    width: usize,
    /// The input was read to its end.
    filled: bool,
    /// `order` holds the rows' order.
    arranged: bool,
}

impl<'a> OrderedRows<'a> {
    fn new(space: &Space<'a>, width: usize) -> Result<Self, SqlError> {
        debug_assert!(width > 0, "a stored row binds a relation");
        Ok(Self {
            store: MaterialStore::new(space)?,
            order: space.list().map_err(store_full)?,
            width,
            filled: false,
            arranged: false,
        })
    }

    /// Forgets every row: the input is read again.
    fn clear(&mut self) {
        self.store.clear();
        self.order.clear();
        self.filled = false;
        self.arranged = false;
    }

    /// The rows a read returns.
    fn len(&self) -> usize {
        self.order.len()
    }

    /// Orders the stored rows by `order`, which ranks them by the values
    /// of `members` in each row and each row's sizes under path `id` and
    /// returns their positions in read order. Only those values are held at
    /// once: each row is decoded alone, its values encoded into a store of
    /// keys, and the keys are decoded together, then released.
    fn arrange(
        &mut self,
        cx: JoinContext<'_, 'a>,
        members: &[(usize, usize)],
        id: u32,
        order: impl FnOnce(&[&'a [Datum<'a>]], &[(usize, usize)]) -> Result<&'a [u32], SqlError>,
    ) -> Result<(), SqlError> {
        let count = self.store.entries.len() / self.width;
        let space = self.order.space();
        let mut keys = MaterialStore::new(&space)?;
        let mut sizes = space.list::<(usize, usize)>().map_err(store_full)?;
        sizes.reserve_exact(count).map_err(store_full)?;
        // In the general executor's arena the lists grow past any mark, so
        // nothing is released there.
        let releases = self.store.releases();
        let mark = cx.arena.mark();
        let ordered = (|| {
            for row in 0..count {
                let row_mark = cx.arena.mark();
                let held = cx
                    .arena
                    .alloc_slice_with(cx.scope.n, |_| None::<BaseRow<'a>>)
                    .map_err(|_| arena_full())?;
                self.store.load(row * self.width, self.width, cx, held)?;
                let values = cx
                    .arena
                    .alloc_slice_with(members.len(), |at| cx.value(held, members[at]))
                    .map_err(|_| arena_full())?;
                sizes
                    .push_reserved(cx.sort_sizes(held, id, members)?)
                    .map_err(store_full)?;
                keys.save_values(cx, values)?;
                if releases {
                    // SAFETY: the row's key values and sizes were copied out.
                    unsafe { cx.arena.rewind_to(row_mark) };
                }
            }
            let rows = cx
                .arena
                .alloc_slice_with(count, |_| -> &'a [Datum<'a>] { &[] })
                .map_err(|_| arena_full())?;
            for (at, row) in rows.iter_mut().enumerate() {
                *row = keys.load_values(at, members.len(), cx.arena)?;
            }
            let order = order(rows, sizes.as_slice())?;
            self.order.clear();
            self.order.reserve_exact(order.len()).map_err(store_full)?;
            self.order.extend_from_slice(order).map_err(store_full)
        })();
        if releases {
            // SAFETY: the order was copied into its lease; no row of this
            // node is bound yet.
            unsafe { cx.arena.rewind_to(mark) };
        }
        ordered?;
        self.arranged = true;
        Ok(())
    }

    /// Reads the stored rows in the order they were stored.
    fn arrange_stored(&mut self) -> Result<(), SqlError> {
        let count = self.store.entries.len() / self.width;
        self.order.clear();
        self.order.reserve_exact(count).map_err(store_full)?;
        for row in 0..count {
            self.order.push_reserved(row as u32).map_err(store_full)?;
        }
        self.arranged = true;
        Ok(())
    }

    /// Each row's `t_len` as path `id`'s tuple, in read order: the sizes
    /// `EXPLAIN ANALYZE` charges a `Materialize` over them.
    fn lengths(&self, cx: JoinContext<'_, 'a>, id: u32) -> Result<NodeList<'a, usize>, SqlError> {
        let mut lengths = self.order.sibling::<usize>().map_err(store_full)?;
        lengths.reserve_exact(self.len()).map_err(store_full)?;
        for at in 0..self.len() {
            let mark = cx.arena.mark();
            let length = cx
                .arena
                .alloc_slice_with(cx.scope.n, |_| None::<BaseRow<'a>>)
                .map_err(|_| arena_full())
                .and_then(|held| {
                    self.load(at, cx, held)?;
                    cx.tuple_len(held, id)
                });
            // SAFETY: only the row's size was kept.
            unsafe { cx.arena.rewind_to(mark) };
            lengths.push_reserved(length?).map_err(store_full)?;
        }
        Ok(lengths)
    }

    /// Binds the row read `at`th in `bound`, decoded into the arena.
    fn load(
        &self,
        at: usize,
        cx: JoinContext<'_, 'a>,
        bound: &mut [Option<BaseRow<'a>>],
    ) -> Result<(), SqlError> {
        let row = self.order.as_slice()[at] as usize;
        self.store.load(row * self.width, self.width, cx, bound)
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
        store: MaterialStore<'a>,
        /// Relations per stored row.
        width: usize,
        filled: bool,
        /// The next stored row a read returns.
        at: usize,
        mark: Option<ArenaMark>,
    },
    HashJoin(HashRecord<'a>),
    /// A sorted or hashed `Unique`: its input's rows and the next one a
    /// read returns.
    Unique {
        rows: OrderedRows<'a>,
        at: usize,
        mark: Option<ArenaMark>,
    },
    Memoize(memo::MemoRecord<'a>),
    /// The scan of a `LATERAL` subquery: its rows for the current outer
    /// row, each encoded as its query projected it, and the next one a
    /// read returns.
    Lateral {
        store: MaterialStore<'a>,
        filled: bool,
        at: usize,
    },
    MergeJoin(merge::MergeRecord<'a>),
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
struct HashRecord<'a> {
    table: Option<hash::BatchStore<'a>>,
    /// The table's inner rows and the outer rows saved to later batches.
    store: MaterialStore<'a>,
    phase: HashPhase,
    /// The table is one batch's and is probed again at a rescan
    /// (`ExecReScanHashJoin`).
    frozen: bool,
    probe: Option<HashProbe>,
    /// The current batch's saved outer rows.
    outer_rows: NodeList<'a, hash::Saved>,
    /// The unmatched inner rows being returned.
    fill: NodeList<'a, u32>,
    /// Where its lists live.
    space: Space<'a>,
    /// The next entry of `outer_rows` or `fill`.
    at: usize,
    /// The batches batch 0 ended with.
    outer_start: u32,
    /// The arena's frontier before a saved outer row, and before an inner
    /// row read from the store.
    outer_mark: Option<ArenaMark>,
    inner_mark: Option<ArenaMark>,
}

impl<'a> HashRecord<'a> {
    fn new(space: &Space<'a>) -> Result<Self, SqlError> {
        Ok(Self {
            table: None,
            store: MaterialStore::new(space)?,
            phase: HashPhase::Start,
            frozen: false,
            probe: None,
            outer_rows: space.list().map_err(store_full)?,
            fill: space.list().map_err(store_full)?,
            space: space.clone(),
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

fn hash_record<'r, 'a>(records: &'r mut [Record<'a>], id: u32) -> &'r mut HashRecord<'a> {
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

/// Where a path's execution is, for its loop under `EXPLAIN ANALYZE`.
enum Execution {
    /// Not read since it started or was rescanned.
    Fresh,
    Open(#[allow(dead_code, reason = "the loop ends when dropped")] instrument::Loop),
    /// Its last read returned no row.
    Ended,
}

/// A path's nodes under `EXPLAIN ANALYZE` (`Node::NONE` otherwise).
struct PathNodes {
    /// The node counting the rows the path returns: its join's, its
    /// `Materialize`'s or its `Memoize`'s. A scan counts in its own nodes.
    rows: instrument::Node,
    /// A hash join's `Hash` node, or a merge join's outer and inner
    /// `Sort`.
    sides: [instrument::Node; 2],
    /// A merge join's `Materialize` over its inner input.
    material: instrument::Node,
    execution: Execution,
    /// The `Hash` node's loop while its table is built.
    building: Option<instrument::Loop>,
}

impl PathNodes {
    fn of(model: &ModeledJoin<'_>, id: u32) -> Self {
        use instrument::Role;
        let none = instrument::Node::NONE;
        let (rows, sides, material) = match model.plan.path(id).kind {
            PathKind::NestLoop { .. } => (node(Role::Join, id), [none; 2], none),
            PathKind::HashJoin { .. } => (node(Role::Join, id), [node(Role::Hash, id), none], none),
            PathKind::MergeJoin { .. } => (
                node(Role::Join, id),
                [
                    node(|number| Role::MergeSort(number, 0), id),
                    node(|number| Role::MergeSort(number, 1), id),
                ],
                node(Role::Materialize, id),
            ),
            PathKind::Material { .. } => (node(Role::Materialize, id), [none; 2], none),
            PathKind::Memoize { .. } => (node(Role::Memoize, id), [none; 2], none),
            PathKind::Unique { method, .. }
                if method != super::super::index_paths::joins::UniqueMethod::Noop =>
            {
                (
                    node(Role::JoinUnique, id),
                    [node(Role::JoinUniqueSort, id), none],
                    none,
                )
            }
            PathKind::Scan { .. } | PathKind::Unique { .. } | PathKind::Dummy => {
                (none, [none; 2], none)
            }
        };
        Self {
            rows,
            sides,
            material,
            execution: Execution::Fresh,
            building: None,
        }
    }
}

/// A modeled join read by pull nodes: a retained reader owns its plan
/// (`M`), the general executor borrows the statement's.
pub(crate) struct ModeledPull<'a, M: Borrow<ModeledJoin<'a>> = ModeledJoin<'a>> {
    txid: u32,
    model: M,
    /// The statement's parameters, copied in when the reader opened.
    params: &'a [Datum<'a>],
    layout: JoinLayout<'a>,
    output: OutputRow<'a>,
    records: MutableSlice<Record<'a>>,
    nodes: MutableSlice<PathNodes>,
    /// The block the join's nodes count in.
    block: instrument::BlockId,
    /// The general executor reads the plan: every row read stays bound.
    keep: bool,
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
    /// The pull nodes of `model` a retained reader reads, whose relations
    /// resolve in `written` (the query's FROM clause) through
    /// [`ModeledJoin::scope`]. Every scan's node is made here, before any
    /// row.
    pub(crate) fn new(
        storage: &Storage,
        txid: u32,
        model: ModeledJoin<'a>,
        written: &QueryScope<'a>,
        params: &[Datum<'a>],
        arena: &'a Arena,
    ) -> Result<Self, SqlError> {
        let catalog = super::super::storage_catalog(storage, arena, txid);
        let hooks = EvalHooks {
            catalog: Some(&catalog),
            ..crate::sql::eval::NO_HOOKS
        };
        Self::open(storage, txid, model, written, params, arena, &hooks, false)
    }
}

impl<'m, 'a> ModeledPull<'a, &'m ModeledJoin<'a>> {
    /// The pull nodes of `model` the general executor reads, under its
    /// hooks: every row read stays in `arena`, and the nodes count in the
    /// current `EXPLAIN ANALYZE` block.
    pub(super) fn general(
        storage: &Storage,
        txid: u32,
        model: &'m ModeledJoin<'a>,
        written: &QueryScope<'a>,
        params: &[Datum<'a>],
        arena: &'a Arena,
        hooks: &EvalHooks<'_, 'a>,
    ) -> Result<Self, SqlError> {
        Self::open(storage, txid, model, written, params, arena, hooks, true)
    }
}

impl<'a, M: Borrow<ModeledJoin<'a>>> ModeledPull<'a, M> {
    #[allow(clippy::too_many_arguments)]
    fn open(
        storage: &Storage,
        txid: u32,
        owned: M,
        written: &QueryScope<'a>,
        params: &[Datum<'a>],
        arena: &'a Arena,
        hooks: &EvalHooks<'_, 'a>,
        keep: bool,
    ) -> Result<Self, SqlError> {
        let model: &ModeledJoin<'a> = owned.borrow();
        // A retained reader reads no `LATERAL` subquery, which reruns at
        // each outer row; the general executor reads every plan.
        debug_assert!(keep || pulls(model));
        let params: &'a [Datum<'a>] = arena.alloc_slice_copy(params).map_err(|_| arena_full())?;
        let scope = model.scope(written);
        let layout = JoinLayout::new(model, scope, arena)?;
        let output = OutputRow::new(model, written, arena)?;
        let block = instrument::current_block();
        let paths = model.plan.paths.len();
        let pool = storage.plan_build();
        // The general executor's nodes keep their lists in its arena.
        let space = if keep {
            Space::Arena(arena)
        } else {
            Space::Leases(Arc::clone(pool))
        };
        let mut nodes = pool.lease::<PathNodes>(paths).map_err(node_pool_full)?;
        for id in 0..paths {
            nodes
                .push_reserved(PathNodes::of(model, id as u32))
                .map_err(node_pool_full)?;
        }
        // The inputs a merge join reads whole into its store but counts as
        // far as it fetched (`ExecMergeJoin` reading its inner index scan
        // in part) count no row as they are read.
        let counted_in_part = |id: u32| {
            instrument::active()
                && model.plan.paths.iter().any(|path| {
                    matches!(path.kind, PathKind::MergeJoin { inner, inner_sort, .. }
                        if inner == id && inner_sort.as_slice().is_empty())
                })
        };
        // Only the paths the plan runs get a record; the search's other
        // paths stay idle, holding no lease.
        let reached = arena
            .alloc_slice_with(paths, |_| false)
            .map_err(|_| arena_full())?;
        mark_reached(model, model.plan.top, reached);
        let mut records = pool.lease::<Record<'a>>(paths).map_err(node_pool_full)?;
        for id in 0..paths {
            let record = match model.plan.paths[id].kind {
                _ if !reached[id] => Record::Idle,
                PathKind::Scan { rel, .. } if model.rels[usize::from(rel)].lateral.is_some() => {
                    Record::Lateral {
                        store: MaterialStore::new(&space)?,
                        filled: false,
                        at: 0,
                    }
                }
                PathKind::Scan { rel, path } => {
                    let modeled = model.rels[usize::from(rel)];
                    let scan = modeled.scan(storage, txid, path, params, arena)?;
                    let _block =
                        instrument::enter_block(block.child(scan_segment(usize::from(rel))));
                    let _suspended = counted_in_part(id as u32).then(instrument::suspend);
                    let mut node = super::super::scan::PathScan::new(
                        storage,
                        txid,
                        modeled.from,
                        scan.condition,
                        scan.path,
                        arena,
                        params,
                        hooks,
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
                PathKind::HashJoin { .. } => Record::HashJoin(HashRecord::new(&space)?),
                PathKind::Unique { input, method, .. }
                    if method != super::super::index_paths::joins::UniqueMethod::Noop =>
                {
                    Record::Unique {
                        rows: OrderedRows::new(
                            &space,
                            relation_count(model.plan.path(input).relids, scope.n),
                        )?,
                        at: 0,
                        mark: None,
                    }
                }
                PathKind::Memoize { .. } => Record::Memoize(memo::MemoRecord::new(space.clone())),
                PathKind::MergeJoin {
                    outer,
                    inner,
                    outer_sort,
                    ..
                } => {
                    let mut record = merge::MergeRecord::new(
                        &space,
                        (
                            relation_count(model.plan.path(outer).relids, scope.n),
                            !outer_sort.as_slice().is_empty(),
                        ),
                        relation_count(model.plan.path(inner).relids, scope.n),
                        scope.n,
                        arena,
                    )?;
                    if counted_in_part(inner) {
                        let cx = JoinContext::new(
                            storage, scope, txid, model, arena, params, hooks, &layout,
                        );
                        record.count_inner_in_part(cx, &space, block, inner)?;
                    }
                    Record::MergeJoin(record)
                }
                PathKind::Material { input } => Record::Material {
                    store: MaterialStore::new(&space)?,
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
            model: owned,
            params,
            layout,
            output,
            records,
            nodes,
            block,
            keep,
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
        let model = self.model.borrow();
        let scope = model.scope(written);
        model
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
                Record::Lateral { store, filled, at } => {
                    store.clear();
                    *filled = false;
                    *at = 0;
                }
                Record::NestLoop { phase, matched } => {
                    *phase = LoopPhase::Outer;
                    *matched = false;
                }
                // A store whose input was not read to its end is read
                // again from the first row.
                Record::Material {
                    store,
                    filled,
                    at,
                    mark,
                    ..
                } => {
                    if !*filled {
                        store.clear();
                    }
                    *at = 0;
                    *mark = None;
                }
                Record::HashJoin(record) => record.restart(false),
                Record::Unique { rows, at, mark } => {
                    if !rows.filled {
                        rows.clear();
                    }
                    *at = 0;
                    *mark = None;
                }
                Record::Memoize(record) => record.rescan(),
                Record::MergeJoin(record) => {
                    record.restart(false, false);
                }
            }
        }
        for nodes in self.nodes.iter_mut() {
            nodes.execution = Execution::Fresh;
            nodes.building = None;
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
        let catalog = super::super::storage_catalog(storage, work, self.txid);
        let hooks = EvalHooks {
            catalog: Some(&catalog),
            ..crate::sql::eval::NO_HOOKS
        };
        self.advance(
            storage,
            written,
            state_arena,
            CursorScratch::Reader(buffers),
            &hooks,
        )
    }

    /// The general executor's [`Self::prepare_row`]: the scans hold the
    /// storage's shared contexts only during each call, and `hooks` are the
    /// statement's.
    pub(super) fn next_row(
        &mut self,
        storage: &Storage,
        written: &QueryScope<'a>,
        arena: &'a Arena,
        hooks: &EvalHooks<'_, 'a>,
    ) -> Result<bool, SqlError> {
        debug_assert!(self.keep);
        self.advance(storage, written, arena, CursorScratch::Statement, hooks)
    }

    fn advance(
        &mut self,
        storage: &Storage,
        written: &QueryScope<'a>,
        arena: &'a Arena,
        scratch: CursorScratch<'_>,
        hooks: &EvalHooks<'_, 'a>,
    ) -> Result<bool, SqlError> {
        if self.current {
            return Ok(true);
        }
        if self.done {
            return Ok(false);
        }
        if self.base.is_none() {
            self.base = Some(arena.mark());
        }
        let Self {
            txid,
            model,
            params,
            layout,
            output,
            records,
            nodes,
            block,
            keep,
            bound,
            ..
        } = self;
        let model: &ModeledJoin<'a> = (*model).borrow();
        let scope = model.scope(written);
        let cx = JoinContext::new(storage, scope, *txid, model, arena, params, hooks, layout);
        // A join condition's subquery counts in the join's block.
        let _block = instrument::enter_block(*block);
        let mut step = Step {
            cx,
            records,
            nodes,
            bound,
            scratch,
            keep: *keep,
            block: *block,
        };
        let found = step.next(model.plan.top)?;
        // An expression of several relations that failed while the last
        // rows were compared fails the read, also at the join's end.
        cx.check()?;
        if !found {
            self.done = true;
            return Ok(false);
        }
        output.fill(model, written, layout.nulls, bound);
        self.current = true;
        Ok(true)
    }

    /// Ends every node's open execution: the reader read its last row.
    /// Under `EXPLAIN ANALYZE` this closes their loops.
    pub(super) fn finish(&mut self) -> Result<(), SqlError> {
        let model = self.model.borrow();
        for id in 0..model.plan.paths.len() {
            end_execution(model, &mut self.records, &mut self.nodes, id as u32);
            if let Record::Lateral { store, filled, at } = &self.records[id]
                && lateral_stopped(store, *filled, *at)
            {
                return Err(lateral_stopped_error());
            }
        }
        Ok(())
    }

    /// The current joined row over `written`, its values in the arena.
    pub(super) fn row<'s>(&'s self, written: &'s QueryScope<'a>) -> JoinRow<'s, 'a, 'a> {
        debug_assert!(self.current);
        self.output.row(self.model.borrow(), written)
    }

    /// The current joined row over `written`.
    pub(crate) fn with_row<'work, R>(
        &'work self,
        written: &'work QueryScope<'a>,
        each: impl FnOnce(&JoinRow<'_, 'work, '_>) -> Result<R, SqlError>,
    ) -> Result<R, SqlError> {
        debug_assert!(self.current);
        each(&self.output.row(self.model.borrow(), written))
    }
}

/// Whether a `LATERAL` subquery ran to its end under `EXPLAIN ANALYZE`
/// while its join stopped reading its rows: its nodes counted more than
/// PostgreSQL's, which stop there.
fn lateral_stopped(store: &MaterialStore<'_>, filled: bool, at: usize) -> bool {
    filled && at < store.entries.len() && instrument::active()
}

fn lateral_stopped_error() -> SqlError {
    crate::sql_err!(
        crate::sql::eval::sqlstate::FEATURE_NOT_SUPPORTED,
        "EXPLAIN ANALYZE of a LATERAL subquery its join stops reading is not supported"
    )
}

/// Marks path `id` and every path under it in `reached`: the paths the
/// plan runs, of every path the search built.
fn mark_reached(model: &ModeledJoin<'_>, id: u32, reached: &mut [bool]) {
    reached[id as usize] = true;
    match model.plan.path(id).kind {
        PathKind::Material { input }
        | PathKind::Unique { input, .. }
        | PathKind::Memoize { input, .. } => mark_reached(model, input, reached),
        PathKind::NestLoop { outer, inner, .. }
        | PathKind::HashJoin { outer, inner, .. }
        | PathKind::MergeJoin { outer, inner, .. } => {
            mark_reached(model, outer, reached);
            mark_reached(model, inner, reached);
        }
        PathKind::Scan { .. } | PathKind::Dummy => {}
    }
}

/// One call's view of the nodes.
struct Step<'s, 'r, 'a> {
    cx: JoinContext<'r, 'a>,
    records: &'s mut [Record<'a>],
    nodes: &'s mut [PathNodes],
    bound: &'s mut [Option<BaseRow<'a>>],
    scratch: CursorScratch<'s>,
    /// The general executor reads the plan: no row is released.
    keep: bool,
    /// The block the join's nodes count in.
    block: instrument::BlockId,
}

/// Ends path `id`'s execution if it is open: its loop closes, and a merge
/// join counts what it fetched from the inputs it read whole.
fn end_execution(
    model: &ModeledJoin<'_>,
    records: &mut [Record<'_>],
    nodes: &mut [PathNodes],
    id: u32,
) {
    let path_nodes = &mut nodes[id as usize];
    if !matches!(path_nodes.execution, Execution::Open(_)) {
        return;
    }
    if let (PathKind::MergeJoin { .. }, Record::MergeJoin(record)) =
        (model.plan.path(id).kind, &mut records[id as usize])
    {
        record.count_execution(model, id, path_nodes);
    }
    path_nodes.execution = Execution::Ended;
}

impl<'a> Step<'_, '_, 'a> {
    /// Moves path `id` to its next row, binding its relations' rows. Its
    /// first read after it started opens its execution; its end closes it.
    fn next(&mut self, id: u32) -> Result<bool, SqlError> {
        super::super::check_timeout()?;
        let nodes = &mut self.nodes[id as usize];
        if matches!(nodes.execution, Execution::Fresh) {
            nodes.execution = Execution::Open(nodes.rows.start());
        }
        let found = self.advance(id)?;
        let nodes = &self.nodes[id as usize];
        if let Record::Memoize(record) = &self.records[id as usize] {
            nodes.rows.memoize(record.stats);
        }
        if found {
            nodes.rows.row();
        } else {
            end_execution(self.cx.model, self.records, self.nodes, id);
        }
        Ok(found)
    }

    fn advance(&mut self, id: u32) -> Result<bool, SqlError> {
        let path = *self.cx.model.plan.path(id);
        match path.kind {
            PathKind::Dummy => Ok(false),
            PathKind::Scan { rel, .. }
                if self.cx.model.rels[usize::from(rel)].lateral.is_some() =>
            {
                self.lateral(id, usize::from(rel))
            }
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
                let join = self.nodes[id as usize].rows;
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
                                    join.filtered();
                                }
                                continue;
                            }
                            if !self.cx.equal(self.bound, &join_quals)? {
                                join.join_filtered();
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
                            join.filtered();
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
            PathKind::Unique {
                input,
                keys,
                key_count,
                method,
            } => {
                if method == super::super::index_paths::joins::UniqueMethod::Noop {
                    return self.next(input);
                }
                self.unique(
                    id,
                    input,
                    method,
                    self.cx.model.plan.keys(keys, key_count),
                    path.rows,
                )
            }
            PathKind::Memoize {
                input,
                keys,
                key_count,
                single_row,
                binary,
            } => self.memoize(
                id,
                input,
                self.cx.model.plan.keys(keys, key_count),
                single_row,
                binary,
            ),
            PathKind::MergeJoin {
                jointype,
                outer,
                inner,
                merge,
                outer_sort,
                inner_sort,
                clauses,
                inner_unique,
                descending,
                nulls_first,
                materialize_inner,
            } => {
                let outer_relids = self.cx.model.plan.path(outer).relids;
                let (join_quals, other_quals) =
                    self.cx.split(&clauses, path.relids, jointype, &merge);
                // `ExecInitMergeJoin`: a unique inner relation or a semi
                // join stops each outer row at its first match; without
                // other join clauses such a join never restores its mark.
                let skip_mark_restore = (inner_unique
                    || matches!(jointype, JoinType::Semi | JoinType::Anti))
                    && clauses.as_slice().len() == merge.as_slice().len();
                self.merge_join(
                    id,
                    &merge::MergeJoin {
                        jointype,
                        outer,
                        inner,
                        outer_sort,
                        inner_sort,
                        keys: self.cx.sides(&merge, outer_relids),
                        join_quals: &join_quals,
                        other_quals: &other_quals,
                        single_match: inner_unique || jointype == JoinType::Semi,
                        skip_mark_restore,
                        materialize_inner,
                        descending,
                        nulls_first,
                    },
                )
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
        end_execution(self.cx.model, self.records, self.nodes, id);
        self.nodes[id as usize].execution = Execution::Fresh;
        match self.cx.model.plan.path(id).kind {
            PathKind::Scan { .. } => match &mut self.records[id as usize] {
                Record::Scan { record, mark } => {
                    record.node.rescan();
                    *mark = None;
                }
                Record::Lateral { store, filled, at } => {
                    if lateral_stopped(store, *filled, *at) {
                        self.cx.fail(lateral_stopped_error());
                    }
                    *filled = false;
                    *at = 0;
                    store.clear();
                }
                _ => {}
            },
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
            PathKind::Unique { input, method, .. } => {
                if method == super::super::index_paths::joins::UniqueMethod::Noop {
                    self.rescan(input);
                    return;
                }
                // A `Sort` or `HashAggregate` returns its rows again; one
                // whose input reads an outer row above, or was not read to
                // its end, reads it again.
                let reread = self.cx.model.plan.path(input).param != Relids::EMPTY;
                let Record::Unique { rows, at, mark } = &mut self.records[id as usize] else {
                    unreachable!("a unique's record");
                };
                *at = 0;
                *mark = None;
                if reread || !rows.filled {
                    rows.clear();
                    self.rescan(input);
                }
            }
            // The input is read again at the next miss.
            PathKind::Memoize { .. } => {
                if let Record::Memoize(record) = &mut self.records[id as usize] {
                    record.rescan();
                }
            }
            PathKind::MergeJoin { outer, inner, .. } => {
                let reread = |path: u32| self.cx.model.plan.path(path).param != Relids::EMPTY;
                let (reread_outer, reread_inner) = (reread(outer), reread(inner));
                let Record::MergeJoin(record) = &mut self.records[id as usize] else {
                    unreachable!("a merge join's record");
                };
                // An outer input in merge order is read again each time.
                let (outer_read, inner_read) = record.restart(reread_outer, reread_inner);
                if outer_read {
                    self.rescan(outer);
                }
                if inner_read {
                    self.rescan(inner);
                }
            }
            PathKind::Dummy => {}
        }
    }

    /// Reads path `input`'s rows to their end into the store `rows`
    /// selects in path `id`'s record; a wait resumes with the rows read so
    /// far kept.
    fn fill(
        &mut self,
        id: u32,
        input: u32,
        rows: for<'r> fn(&'r mut Record<'a>) -> &'r mut OrderedRows<'a>,
    ) -> Result<(), SqlError> {
        let cx = self.cx;
        let relids = cx.model.plan.path(input).relids;
        while !rows(&mut self.records[id as usize]).filled {
            if !self.next(input)? {
                rows(&mut self.records[id as usize]).filled = true;
                break;
            }
            rows(&mut self.records[id as usize])
                .store
                .save(cx, self.bound, relids)?;
        }
        Ok(())
    }

    /// A sorted or hashed `Unique`: its input read whole, then the first
    /// row of each run or group of equal compared columns in the order its
    /// sort or hash table returns them.
    fn unique(
        &mut self,
        id: u32,
        input: u32,
        method: super::super::index_paths::joins::UniqueMethod,
        keys: &[Member],
        groups: f64,
    ) -> Result<bool, SqlError> {
        let cx = self.cx;
        self.fill(id, input, |record| {
            let Record::Unique { rows, .. } = record else {
                unreachable!("a unique's record");
            };
            rows
        })?;
        let Record::Unique { rows, at, mark } = &mut self.records[id as usize] else {
            unreachable!("a unique's record");
        };
        let nodes = &self.nodes[id as usize];
        let hashed = method == super::super::index_paths::joins::UniqueMethod::Hash;
        if !rows.arranged {
            let (members, count) = JoinContext::unique_members(keys);
            let members = &members[..count];
            if hashed {
                let mut figures = None;
                rows.arrange(cx, members, input, |held, _| {
                    cx.unique_hashed_order(input, held, members, groups)
                        .map(|(order, counted)| {
                            figures = Some(counted);
                            order
                        })
                })?;
                if let Some(figures) = figures {
                    nodes.rows.hash_agg_figures(figures);
                }
            } else {
                let mut space = None;
                rows.arrange(cx, members, input, |held, sizes| {
                    cx.unique_order(input, held, sizes, members)
                        .map(|(order, sorted)| {
                            space = Some(sorted);
                            order
                        })
                })?;
                if let Some(space) = space {
                    nodes.sides[0].sorted(space);
                }
            }
        }
        // Each execution's first fetch reads the `Sort` under a `Unique`
        // whole: it returns every input row.
        if !hashed && *at == 0 {
            let _sort = nodes.sides[0].start();
            nodes.sides[0].rows((rows.store.entries.len() / rows.width) as u64);
        }
        Self::release(cx.arena, self.keep, mark);
        if *at >= rows.len() {
            return Ok(false);
        }
        rows.load(*at, cx, self.bound)?;
        *at += 1;
        Ok(true)
    }

    /// Releases the bytes of a path's previous row and marks where its next
    /// one starts; the general executor (`keep`) releases none.
    fn release(arena: &Arena, keep: bool, mark: &mut Option<ArenaMark>) {
        if keep {
            return;
        }
        if let Some(previous) = mark.take() {
            // SAFETY: the previous row and everything read after it are no
            // longer bound: the caller moves this path on.
            unsafe { arena.rewind_to(previous) };
        }
        *mark = Some(arena.mark());
    }

    /// A `LATERAL` subquery's next row for the bound outer row: its query
    /// runs whole with the outer columns it reads bound at the first read
    /// after a rescan, its nodes counting in the relation's block as
    /// `EXPLAIN` plans them.
    fn lateral(&mut self, id: u32, rel: usize) -> Result<bool, SqlError> {
        let cx = self.cx;
        let lateral = cx.model.rels[rel].lateral.as_ref().expect("a LATERAL item");
        let Record::Lateral { store, filled, at } = &mut self.records[id as usize] else {
            unreachable!("a LATERAL scan's record");
        };
        if !*filled {
            let (values, offsets, forms) = cx.outer_values(self.bound, lateral.relids)?;
            let outer_row = JoinRow {
                scope: cx.scope,
                values: JoinRowValues::Flat {
                    values,
                    offsets,
                    forms: Some(forms),
                },
                rowids: &[],
                visible: cx.scope.n,
            };
            let _block = instrument::enter_block(self.block.child(scan_segment(rel)));
            super::super::select_into_rows(
                cx.storage,
                cx.txid,
                lateral.select,
                cx.arena,
                cx.params,
                Some(&outer_row),
                cx.hooks.sequences,
                &mut |values, forms| {
                    let mark = cx.arena.mark();
                    let pushed = crate::sql::exec::encode_projected_formed(values, forms, cx.arena)
                        .and_then(|encoded| store.push_bytes(encoded));
                    if store.releases() {
                        // SAFETY: the encoding was copied into the store.
                        unsafe { cx.arena.rewind_to(mark) };
                    }
                    let (start, len) = pushed?;
                    store.push_entry(StoredRel {
                        rel: rel as u8,
                        start,
                        len,
                        rowid: None,
                    })
                },
            )?;
            *filled = true;
        }
        let Some(&entry) = store.entries.as_slice().get(*at) else {
            return Ok(false);
        };
        *at += 1;
        let encoded = cx
            .arena
            .alloc_slice_copy(
                &store.bytes.as_slice()[entry.start as usize..(entry.start + entry.len) as usize],
            )
            .map_err(|_| arena_full())?;
        let columns = lateral.widths.len();
        let values = cx
            .arena
            .alloc_slice_with(columns + cx.model.expressions.len(), |_| Datum::Null)
            .map_err(|_| arena_full())?;
        for (column, value) in values.iter_mut().take(columns).enumerate() {
            *value = crate::sql::exec::decode_projected_col_record(encoded, column, cx.arena)?;
        }
        let forms = cx
            .arena
            .alloc_slice_with(columns, |column| {
                crate::sql::exec::projected_stored_form(encoded, column)
            })
            .map_err(|_| arena_full())?;
        self.bound[rel] = Some(BaseRow {
            values,
            forms,
            rowid: None,
        });
        Ok(true)
    }

    fn scan(&mut self, id: u32, rel: usize) -> Result<bool, SqlError> {
        let cx = self.cx;
        let Record::Scan { record, mark } = &mut self.records[id as usize] else {
            unreachable!("a scan's record");
        };
        Self::release(cx.arena, self.keep, mark);
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
        // The scan's nodes, and the expressions its rows compute, count in
        // its relation's block.
        let _block = instrument::enter_block(self.block.child(scan_segment(rel)));
        if !node.next(
            cx.storage,
            self.scratch.reborrow(),
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

    /// A `Materialize`: its input's rows, kept for every rescan
    /// (`ExecMaterial`, `ExecReScanMaterial`). A retained reader reads them
    /// whole at the first row, encoded in the node's store; the general
    /// executor reads its input only as far as a loop fetches and keeps the
    /// rows bound as they were read. Under `EXPLAIN ANALYZE` its tuplestore
    /// counts each row as it enters.
    fn material(&mut self, id: u32, input: u32) -> Result<bool, SqlError> {
        if self.keep {
            return self.material_kept(id, input);
        }
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
        Self::release(cx.arena, self.keep, mark);
        let first = *at * *width;
        if first >= store.entries.len() {
            return Ok(false);
        }
        *at += 1;
        store.load(first, *width, cx, self.bound)?;
        Ok(true)
    }

    /// The general executor's `Materialize`: the next kept row, or the
    /// input's next row, kept.
    fn material_kept(&mut self, id: u32, input: u32) -> Result<bool, SqlError> {
        let cx = self.cx;
        let relids = cx.model.plan.path(input).relids;
        let node = self.nodes[id as usize].rows;
        // The tuplestore `EXPLAIN ANALYZE` reports, even when empty.
        node.store_opened();
        let Record::Material {
            store,
            width,
            at,
            filled,
            ..
        } = &mut self.records[id as usize]
        else {
            unreachable!("a materialize's record");
        };
        let first = *at * *width;
        if first < store.entries.len() {
            *at += 1;
            store.load(first, *width, cx, self.bound)?;
            return Ok(true);
        }
        if *filled {
            return Ok(false);
        }
        if !self.next(input)? {
            if let Record::Material { filled, .. } = &mut self.records[id as usize] {
                *filled = true;
            }
            return Ok(false);
        }
        if node.measured() {
            node.store_row(cx.tuple_len(self.bound, id).ok());
        }
        let Record::Material { store, at, .. } = &mut self.records[id as usize] else {
            unreachable!("a materialize's record");
        };
        store.save(cx, self.bound, relids)?;
        *at += 1;
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
                        let mut table = hash::BatchStore::new(&record.space, sizing)?;
                        // `ExecHashBuildSkewHash`: the outer key's most common
                        // values, when the join hashes one key of a stored
                        // column into several batches.
                        if sizing.batches > 1 && cx.skewed(&keys) {
                            cx.build_skew(&mut table, keys[0].0, sizing)?;
                        }
                        record.table = Some(table);
                        let nodes = &mut self.nodes[id as usize];
                        nodes.building = Some(nodes.sides[0].start());
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
                    let nodes = &mut self.nodes[id as usize];
                    nodes.sides[0].rows(table.total as u64);
                    nodes.sides[0].hash_table(table.figures());
                    nodes.building = None;
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
                    Self::release(cx.arena, self.keep, &mut record.inner_mark);
                    let count = relation_count(inner_relids, cx.scope.n);
                    record.store.load(row as usize, count, cx, self.bound)?;
                    cx.null_extend(self.bound, outer_relids);
                    if cx.equal(self.bound, &other_quals)? {
                        return Ok(true);
                    }
                    self.nodes[id as usize].rows.filtered();
                }
                HashPhase::NextBatch => {
                    let table = record.table.as_mut().expect("a table of several batches");
                    // A later batch's table, once its rows were probed.
                    if table.current > 0 {
                        self.nodes[id as usize].sides[0].hash_table(table.figures());
                    }
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
                        Self::release(cx.arena, self.keep, &mut record.outer_mark);
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
        let join = self.nodes[id as usize].rows;
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
                join.filtered();
                continue;
            };
            Self::release(cx.arena, self.keep, &mut record.inner_mark);
            let count = relation_count(inner_relids, cx.scope.n);
            record
                .store
                .load(candidate.hashed.row as usize, count, cx, self.bound)?;
            if cx.has_null(self.bound, keys, false)
                || cx.compare_sides(self.bound, self.bound, keys)? != Ordering::Equal
            {
                continue;
            }
            if !cx.equal(self.bound, join_quals)? {
                join.join_filtered();
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
            join.filtered();
        }
    }
}
