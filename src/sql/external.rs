//! Bounded external sorting over the provider-neutral block grid.
//!
//! A run is an ordinary immutable SST. Rows are sorted in one block-sized
//! startup buffer, written through `BlockStore`, and combined with a fixed
//! eight-way merge. Binary carry across levels keeps only seven runs per
//! level, so the amount of resident state is fixed while the durable tier
//! supplies the capacity. RAM and local disk see exactly the same blocks as
//! caches; no provider identity reaches this module.

use crate::sql::query::instrument::{SortMethod, SortSpace};
use crate::sql::query::postgres_memory::{MAX_MERGE_ORDER, TuplesortSpace};
use core::cmp::Ordering;

use crate::mem::budget::{Budget, BudgetError};
use crate::sql::eval::{SqlError, sqlstate};
use crate::sql::exec::{encode_projected_by_into, projected_row_len_by};
use crate::sql::types::Datum;
use crate::sql_err;
use crate::store::{
    BlockStore, MAX_INLINE_ROW, MAX_PAYLOAD, SstCursor, SstError, SstHandle, SstWriter,
};

const MERGE_FAN_IN: usize = 8;
const MERGE_LEVELS: usize = 16;
const ROWS_PER_CHUNK: usize = 8192;
const ORDINAL_BYTES: usize = 8;

#[derive(Clone, Copy)]
struct BufferedRow {
    offset: u32,
    length: u32,
    ordinal: u64,
}

const EMPTY_BUFFERED_ROW: BufferedRow = BufferedRow {
    offset: 0,
    length: 0,
    ordinal: 0,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct ExternalRun {
    handle: SstHandle,
    rows: u64,
}

impl ExternalRun {
    pub(crate) fn from_checkpoint(handle: SstHandle, rows: u64) -> Self {
        Self { handle, rows }
    }

    pub(crate) fn rows(self) -> u64 {
        self.rows
    }

    pub(crate) fn cursor(self) -> SstCursor {
        SstCursor::new(self.handle)
    }
}

struct MergeReader {
    cursor: Option<SstCursor>,
    index: Box<[u8]>,
    data: Box<[u8]>,
    bounce: Box<[u8]>,
    current: Box<[u8]>,
    current_len: usize,
}

impl MergeReader {
    fn new() -> Self {
        Self {
            cursor: None,
            index: vec![0u8; MAX_PAYLOAD].into_boxed_slice(),
            data: vec![0u8; MAX_PAYLOAD].into_boxed_slice(),
            bounce: vec![0u8; MAX_PAYLOAD].into_boxed_slice(),
            current: vec![0u8; MAX_PAYLOAD].into_boxed_slice(),
            current_len: 0,
        }
    }

    fn start(&mut self, run: ExternalRun, store: &mut dyn BlockStore) -> Result<(), SstError> {
        self.cursor = Some(SstCursor::new(run.handle));
        self.advance(store)
    }

    fn advance(&mut self, store: &mut dyn BlockStore) -> Result<(), SstError> {
        self.current_len = match self.cursor.as_mut().expect("started").next_copy(
            store,
            &mut self.index,
            &mut self.data,
            &mut self.bounce,
            &mut self.current,
        )? {
            Some((_, length)) => length,
            None => 0,
        };
        Ok(())
    }

    fn row(&self) -> Option<&[u8]> {
        (self.current_len > 0).then_some(&self.current[..self.current_len])
    }
}

/// A leased, startup-allocated cursor over one immutable external run.
///
/// Readers live independently from [`ExternalSorter`], so a consumer may
/// stream one completed run while a nested producer reuses the sorter to build
/// another. Only encoded bytes cross that lifetime boundary; statement-arena
/// references never do.
pub(crate) struct ExternalRunReader {
    reader: MergeReader,
    previous: Box<[u8]>,
    previous_len: usize,
    boundary: Box<[u8]>,
    output: Box<[u8]>,
}

pub(crate) struct ExternalRunContext<'a> {
    pub(crate) row: &'a [u8],
    pub(crate) previous: Option<&'a [u8]>,
    pub(crate) boundary: &'a mut [u8],
    pub(crate) output: &'a mut [u8],
}

impl ExternalRunReader {
    pub(crate) fn budget_bytes() -> usize {
        core::mem::size_of::<crate::mem::mutex::Mutex<Self>>() + 7 * MAX_PAYLOAD
    }

    pub(crate) fn new() -> Self {
        Self {
            reader: MergeReader::new(),
            previous: vec![0u8; MAX_PAYLOAD].into_boxed_slice(),
            previous_len: 0,
            boundary: vec![0u8; MAX_PAYLOAD].into_boxed_slice(),
            output: vec![0u8; MAX_PAYLOAD].into_boxed_slice(),
        }
    }

    pub(crate) fn start(
        &mut self,
        store: &mut dyn BlockStore,
        run: ExternalRun,
    ) -> Result<(), SqlError> {
        self.prepare(run);
        self.advance(store)
    }

    pub(crate) fn prepare(&mut self, run: ExternalRun) {
        self.previous_len = 0;
        self.reader.current_len = 0;
        self.reader.cursor = Some(run.cursor());
    }

    pub(crate) fn row(&self) -> Option<&[u8]> {
        self.reader.row().map(|prefixed| &prefixed[ORDINAL_BYTES..])
    }

    pub(crate) fn prefixed_row(&self) -> Option<&[u8]> {
        self.reader.row()
    }

    pub(crate) fn context(&mut self) -> Option<ExternalRunContext<'_>> {
        let Self {
            reader,
            previous,
            previous_len,
            boundary,
            output,
        } = self;
        let row = reader.row().map(|prefixed| &prefixed[ORDINAL_BYTES..])?;
        let prior = (*previous_len > 0).then_some(&previous[..*previous_len]);
        Some(ExternalRunContext {
            row,
            previous: prior,
            boundary,
            output,
        })
    }

    pub(crate) fn output(&self, length: usize) -> &[u8] {
        &self.output[..length]
    }

    /// Copies the current immutable row into reader-owned staging so a merge
    /// may advance this cursor while retaining the run key.
    pub(crate) fn stage_current(&mut self) -> Option<usize> {
        let row = self
            .reader
            .row()
            .map(|prefixed| &prefixed[ORDINAL_BYTES..])?;
        self.output[..row.len()].copy_from_slice(row);
        Some(row.len())
    }

    pub(crate) fn advance(&mut self, store: &mut dyn BlockStore) -> Result<(), SqlError> {
        if let Some(prefixed) = self.reader.row() {
            let row = &prefixed[ORDINAL_BYTES..];
            self.previous[..row.len()].copy_from_slice(row);
            self.previous_len = row.len();
        }
        self.reader.advance(store).map_err(run_error)
    }
}

/// The storage that owns a sorter, which only reads it while sizing rows.
#[derive(Clone, Copy)]
struct StorageRef(core::ptr::NonNull<crate::storage::Storage>);

// SAFETY: a shared reference to the storage, which is `Sync`, and the storage
// outlives the sorter it owns.
unsafe impl Send for StorageRef {}
unsafe impl Sync for StorageRef {}

/// Scratch for ordering one chunk's rows as `tuplesort.c` does: a chunk
/// holds at most `ROWS_PER_CHUNK` rows, so every array is sized from that.
struct ExactScratch {
    held: Box<[BufferedRow]>,
    order: Box<[u32]>,
    moved: Box<[u32]>,
    bounds: Box<[u32]>,
    heap: Box<[u32]>,
    cursor: Box<[u32]>,
}

impl ExactScratch {
    fn budget_bytes() -> usize {
        ROWS_PER_CHUNK * core::mem::size_of::<BufferedRow>()
            + (2 * ROWS_PER_CHUNK + 2 * (ROWS_PER_CHUNK + 1) + 2 * MAX_MERGE_ORDER)
                * core::mem::size_of::<u32>()
    }

    fn new() -> Self {
        Self {
            held: vec![EMPTY_BUFFERED_ROW; ROWS_PER_CHUNK].into_boxed_slice(),
            order: vec![0u32; ROWS_PER_CHUNK].into_boxed_slice(),
            moved: vec![0u32; ROWS_PER_CHUNK].into_boxed_slice(),
            bounds: vec![0u32; 2 * (ROWS_PER_CHUNK + 1)].into_boxed_slice(),
            heap: vec![0u32; MAX_MERGE_ORDER].into_boxed_slice(),
            cursor: vec![0u32; MAX_MERGE_ORDER].into_boxed_slice(),
        }
    }
}

pub(crate) struct ExternalSorter {
    chunk: Box<[u8]>,
    chunk_len: usize,
    rows: Box<[BufferedRow]>,
    row_count: usize,
    writer: SstWriter,
    levels: [[Option<ExternalRun>; MERGE_FAN_IN]; MERGE_LEVELS],
    level_counts: [usize; MERGE_LEVELS],
    readers: [MergeReader; MERGE_FAN_IN],
    next_ordinal: u64,
    /// The bytes of the runs written before the input ended.
    spilled_bytes: usize,
    /// How the last finished sort ordered its rows.
    space: Option<SortSpace>,
    /// Whether [`ExternalSorter::keep_least`] dropped rows of this input.
    bounded: bool,
    /// The tuples PostgreSQL's sort of these rows would hold, which size
    /// the space `EXPLAIN ANALYZE` reports.
    postgres: Option<crate::sql::query::instrument::SortTuples>,
    /// The limit's row count a bounded sort keeps.
    bound: Option<usize>,
    /// The storage whose catalog sizes the rows' values, and the
    /// transaction. Set by [`ExternalSorter::size_as_postgres`]; the storage
    /// owns this sorter, so it outlives every use.
    source: Option<(StorageRef, u32)>,
    /// PostgreSQL's sort of the rows pushed so far, once one was.
    model: Option<crate::sql::query::postgres_memory::TuplesortModel>,
    /// The row counts of the runs the model's `dumptuples` wrote, in
    /// order.
    pg_runs: Box<[u32]>,
    pg_run_count: usize,
    /// The rows were put in PostgreSQL's order already.
    ordered: bool,
    /// Scratch for ordering a chunk as PostgreSQL's tuplesort does.
    exact: ExactScratch,
    /// What the rows of a bounded heap charge, once [`ExternalSorter::keep_least`]
    /// kept them.
    heap_charge: Option<usize>,
    /// Scratch positions of a bounded heap.
    heap: Box<[usize]>,
}

impl ExternalSorter {
    pub(crate) fn budget_bytes() -> usize {
        core::mem::size_of::<Self>()
            + MAX_PAYLOAD
            + ROWS_PER_CHUNK * core::mem::size_of::<BufferedRow>()
            + SstWriter::budget_bytes()
            + MERGE_FAN_IN * 4 * MAX_PAYLOAD
            + ROWS_PER_CHUNK * core::mem::size_of::<usize>()
            + ROWS_PER_CHUNK * core::mem::size_of::<u32>()
            + ExactScratch::budget_bytes()
    }

    /// Allocates every run/merge buffer at startup.
    pub(crate) fn new(budget: &mut Budget) -> Result<Self, BudgetError> {
        budget.draw(Self::budget_bytes(), "external query runs")?;
        Ok(Self {
            chunk: vec![0u8; MAX_PAYLOAD].into_boxed_slice(),
            chunk_len: 0,
            rows: vec![EMPTY_BUFFERED_ROW; ROWS_PER_CHUNK].into_boxed_slice(),
            row_count: 0,
            writer: SstWriter::new(),
            levels: [[None; MERGE_FAN_IN]; MERGE_LEVELS],
            level_counts: [0; MERGE_LEVELS],
            readers: core::array::from_fn(|_| MergeReader::new()),
            next_ordinal: 0,
            spilled_bytes: 0,
            space: None,
            bounded: false,
            postgres: None,
            bound: None,
            source: None,
            model: None,
            pg_runs: vec![0u32; ROWS_PER_CHUNK].into_boxed_slice(),
            pg_run_count: 0,
            ordered: false,
            exact: ExactScratch::new(),
            heap_charge: None,
            heap: vec![0usize; ROWS_PER_CHUNK].into_boxed_slice(),
        })
    }

    pub(crate) fn reset(&mut self) {
        self.chunk_len = 0;
        self.row_count = 0;
        self.levels = [[None; MERGE_FAN_IN]; MERGE_LEVELS];
        self.level_counts = [0; MERGE_LEVELS];
        self.writer.reset();
        self.next_ordinal = 0;
        self.spilled_bytes = 0;
        self.space = None;
        self.bounded = false;
        self.postgres = None;
        self.bound = None;
        self.source = None;
        self.model = None;
        self.pg_run_count = 0;
        self.ordered = false;
        self.heap_charge = None;
        for reader in &mut self.readers {
            reader.cursor = None;
            reader.current_len = 0;
        }
    }

    /// Sizes the space `EXPLAIN ANALYZE` reports for this input as
    /// PostgreSQL's sort of `tuples` would hold it, keeping the first `bound`
    /// rows when a limit bounds it; set after [`Self::reset`].
    pub(crate) fn size_as_postgres(
        &mut self,
        tuples: Option<crate::sql::query::instrument::SortTuples>,
        bound: Option<usize>,
        source: crate::sql::query::postgres_memory::SizeSource<'_>,
    ) {
        self.postgres = tuples;
        self.bound = bound;
        self.source = Some((
            StorageRef(core::ptr::NonNull::from(source.storage)),
            source.txid,
        ));
    }

    fn buffered_row(&self, position: usize) -> &[u8] {
        let entry = &self.rows[position];
        &self.chunk[entry.offset as usize + ORDINAL_BYTES..(entry.offset + entry.length) as usize]
    }

    /// Feeds the row just buffered at `offset` to PostgreSQL's sort.
    fn size_row(&mut self, offset: usize, total: usize) -> Result<(), SqlError> {
        let Some(tuples) = self.postgres else {
            return Ok(());
        };
        let row = &self.chunk[offset + ORDINAL_BYTES..offset + total];
        let model = self
            .model
            .get_or_insert_with(|| tuples.model(Some(row), self.bound));
        let (storage, txid) = self.source.expect("sized rows name their catalog");
        // SAFETY: the storage owns this sorter and outlives it.
        let source = crate::sql::query::postgres_memory::SizeSource {
            storage: unsafe { storage.0.as_ref() },
            txid,
        };
        if let Some(rows) = tuples.push(source, model, Some(row), row)?
            && let Some(slot) = self.pg_runs.get_mut(self.pg_run_count)
        {
            *slot = rows as u32;
            self.pg_run_count += 1;
        }
        Ok(())
    }

    /// What PostgreSQL's sort of the rows reports, `None` when no sort was
    /// described.
    fn postgres_space(
        &self,
    ) -> Result<Option<crate::sql::query::postgres_memory::TuplesortSpace>, SqlError> {
        // A sort that read no row still holds its initial array.
        let Some(model) = self
            .model
            .or_else(|| self.postgres.map(|tuples| tuples.model(None, self.bound)))
        else {
            return Ok(None);
        };
        if model.bounded() && self.heap_charge.is_none() {
            // Only an analyzed statement reports the figure.
            if !crate::sql::query::instrument::active() {
                return Ok(None);
            }
            return Err(sql_err!(
                sqlstate::FEATURE_NOT_SUPPORTED,
                "EXPLAIN ANALYZE of a bounded sort whose rows outgrew a chunk is not supported"
            ));
        }
        Ok(Some(model.space(self.heap_charge)))
    }

    /// Encodes and appends one self-describing projected row without touching
    /// the statement arena.
    pub(crate) fn push_projected_by<'a>(
        &mut self,
        store: &mut dyn BlockStore,
        columns: usize,
        mut value_at: impl FnMut(usize) -> Datum<'a>,
        compare: &mut impl FnMut(&[u8], &[u8]) -> Result<Ordering, SqlError>,
    ) -> Result<(), SqlError> {
        let projected_len = projected_row_len_by(columns, &mut value_at)?;
        let total = ORDINAL_BYTES
            .checked_add(projected_len)
            .ok_or_else(row_too_large)?;
        if total > MAX_INLINE_ROW {
            return Err(row_too_large());
        }
        if self.row_count == self.rows.len() || self.chunk_len + total > self.chunk.len() {
            self.flush_chunk(store, compare, false)?;
        }
        let offset = self.chunk_len;
        self.chunk[offset..offset + ORDINAL_BYTES]
            .copy_from_slice(&self.next_ordinal.to_le_bytes());
        encode_projected_by_into(
            columns,
            &mut value_at,
            &mut self.chunk[offset + ORDINAL_BYTES..offset + total],
        )?;
        self.finish_push(offset, total)
    }

    /// Appends one already-encoded projected row. This is the common spool
    /// seam for set multisets, recursive work tables, subquery collectors,
    /// window partitions, and outer-join match identifiers.
    pub(crate) fn push_encoded(
        &mut self,
        store: &mut dyn BlockStore,
        row: &[u8],
        compare: &mut impl FnMut(&[u8], &[u8]) -> Result<Ordering, SqlError>,
    ) -> Result<(), SqlError> {
        let total = ORDINAL_BYTES
            .checked_add(row.len())
            .ok_or_else(row_too_large)?;
        if total > MAX_INLINE_ROW {
            return Err(row_too_large());
        }
        if self.row_count == self.rows.len() || self.chunk_len + total > self.chunk.len() {
            self.flush_chunk(store, compare, false)?;
        }
        let offset = self.chunk_len;
        self.chunk[offset..offset + ORDINAL_BYTES]
            .copy_from_slice(&self.next_ordinal.to_le_bytes());
        self.chunk[offset + ORDINAL_BYTES..offset + total].copy_from_slice(row);
        self.finish_push(offset, total)
    }

    fn finish_push(&mut self, offset: usize, total: usize) -> Result<(), SqlError> {
        self.rows[self.row_count] = BufferedRow {
            offset: offset as u32,
            length: total as u32,
            ordinal: self.next_ordinal,
        };
        self.row_count += 1;
        self.chunk_len += total;
        self.size_row(offset, total)?;
        self.next_ordinal = self.next_ordinal.checked_add(1).ok_or_else(|| {
            sql_err!(
                sqlstate::PROGRAM_LIMIT_EXCEEDED,
                "external query row ordinal exhausted"
            )
        })?;
        Ok(())
    }

    pub(crate) fn finish(
        &mut self,
        store: &mut dyn BlockStore,
        compare: &mut impl FnMut(&[u8], &[u8]) -> Result<Ordering, SqlError>,
    ) -> Result<Option<ExternalRun>, SqlError> {
        // Input that fits one chunk is sorted in memory before its single
        // run is written for reading; more input merges the runs it spilled.
        let spilled = self.level_counts.iter().any(|&count| count != 0);
        let chunk_bytes = self.chunk_len + self.row_count * core::mem::size_of::<BufferedRow>();
        let heap = self.model.is_some_and(|model| model.bounded());
        let postgres = self.postgres_space()?;
        self.flush_chunk(store, compare, true)?;
        self.space = Some(match postgres {
            Some(TuplesortSpace::Memory(bytes)) => SortSpace {
                method: if heap {
                    SortMethod::TopNHeapsort
                } else {
                    SortMethod::Quicksort
                },
                bytes,
            },
            Some(TuplesortSpace::Disk(bytes)) => SortSpace {
                method: SortMethod::ExternalMerge,
                bytes,
            },
            None if spilled => SortSpace {
                method: SortMethod::ExternalMerge,
                bytes: self.spilled_bytes,
            },
            None => SortSpace {
                method: if self.bounded {
                    SortMethod::TopNHeapsort
                } else {
                    SortMethod::Quicksort
                },
                bytes: chunk_bytes,
            },
        });
        let mut carry: Option<ExternalRun> = None;
        for level in 0..MERGE_LEVELS {
            let mut runs = [None; MERGE_FAN_IN];
            let mut count = 0usize;
            if let Some(run) = carry.take() {
                runs[count] = Some(run);
                count += 1;
            }
            for run in &self.levels[level][..self.level_counts[level]] {
                runs[count] = *run;
                count += 1;
            }
            self.level_counts[level] = 0;
            self.levels[level] = [None; MERGE_FAN_IN];
            carry = match count {
                0 => None,
                1 => runs[0],
                _ => Some(self.merge_runs(store, &runs[..count], compare)?),
            };
        }
        if self.level_counts.iter().any(|&count| count != 0) {
            unreachable!("finish drained every merge level");
        }
        Ok(carry)
    }

    /// Called before [`ExternalSorter::finish`] under a limit of `bound`
    /// rows: when the input fits one chunk and holds more than twice that,
    /// keeps only the `bound` rows a stable sort puts first, as PostgreSQL's
    /// top-N heapsort does.
    pub(crate) fn keep_least(
        &mut self,
        bound: usize,
        compare: &mut impl FnMut(&[u8], &[u8]) -> Result<Ordering, SqlError>,
    ) -> Result<(), SqlError> {
        if self.level_counts.iter().any(|&count| count != 0) {
            return Ok(());
        }
        // The rows are still in arrival order, which PostgreSQL's bounded
        // heap charges by.
        if let Some(tuples) = self.postgres
            && let Some(model) = self.model.filter(|model| model.bounded())
        {
            let mut positions = core::mem::take(&mut self.heap);
            let charge = {
                let this = &*self;
                tuples.heap_charge(
                    // SAFETY: the storage owns this sorter and outlives it.
                    crate::sql::query::postgres_memory::SizeSource {
                        storage: unsafe { this.source.expect("sized rows").0.0.as_ref() },
                        txid: this.source.expect("sized rows").1,
                    },
                    this.row_count,
                    model.heap_at(),
                    &|position| this.buffered_row(position),
                    &mut positions[..bound.min(ROWS_PER_CHUNK)],
                    &mut |left: &[u8], right: &[u8]| compare(left, right),
                )
            };
            self.heap = positions;
            self.heap_charge = Some(charge?);
            // A sort that kept a heap returns its rows in the heap's order.
            return self.order_heap_as_postgres(bound, compare);
        }
        if self.row_count <= bound.saturating_mul(2) {
            return Ok(());
        }
        let chunk = &self.chunk;
        crate::util::try_heap_select_by(&mut self.rows[..self.row_count], bound, |a, b| {
            let row = |entry: &BufferedRow| {
                let start = entry.offset as usize + ORDINAL_BYTES;
                let end = entry.offset as usize + entry.length as usize;
                &chunk[start..end]
            };
            Ok(compare(row(a), row(b))?.then_with(|| a.ordinal.cmp(&b.ordinal)))
        })?;
        self.row_count = bound;
        self.bounded = true;
        Ok(())
    }

    /// How the last [`ExternalSorter::finish`] ordered its rows.
    pub(crate) fn space(&self) -> Option<SortSpace> {
        self.space
    }

    /// Sorts a complete run in startup memory when no chunk has spilled.
    /// Callers must consume these rows before resetting or reusing the sorter.
    pub(crate) fn in_memory_rows(
        &mut self,
        compare: &mut impl FnMut(&[u8], &[u8]) -> Result<Ordering, SqlError>,
    ) -> Result<Option<usize>, SqlError> {
        if self.level_counts.iter().any(|&count| count != 0) {
            return Ok(None);
        }
        self.sort_chunk(compare, true)?;
        Ok(Some(self.row_count))
    }

    pub(crate) fn in_memory_row(&self, position: usize) -> &[u8] {
        let entry = &self.rows[position];
        let start = entry.offset as usize + ORDINAL_BYTES;
        let end = entry.offset as usize + entry.length as usize;
        &self.chunk[start..end]
    }

    /// Sorts the buffered rows and writes them as a run; `last` names the
    /// input's final chunk.
    fn flush_chunk(
        &mut self,
        store: &mut dyn BlockStore,
        compare: &mut impl FnMut(&[u8], &[u8]) -> Result<Ordering, SqlError>,
        last: bool,
    ) -> Result<(), SqlError> {
        if self.row_count == 0 {
            return Ok(());
        }
        self.sort_chunk(compare, last)?;
        self.writer.reset();
        self.spilled_bytes = self.spilled_bytes.saturating_add(self.chunk_len);
        let rows = self.row_count as u64;
        for (position, entry) in self.rows[..self.row_count].iter().enumerate() {
            let start = entry.offset as usize;
            let end = start + entry.length as usize;
            self.writer
                .append(store, position as u64 + 1, &self.chunk[start..end])
                .map_err(run_error)?;
        }
        let handle = self
            .writer
            .finish(store)
            .map_err(run_error)?
            .expect("non-empty run");
        self.chunk_len = 0;
        self.row_count = 0;
        self.add_run(store, ExternalRun { handle, rows }, 0, compare)
    }

    fn sort_chunk(
        &mut self,
        compare: &mut impl FnMut(&[u8], &[u8]) -> Result<Ordering, SqlError>,
        last: bool,
    ) -> Result<(), SqlError> {
        if self.ordered {
            return Ok(());
        }
        // The whole input sits in this chunk, so PostgreSQL's order of rows
        // that compare equal is reproducible.
        if last && self.model.is_some() && self.spilled_bytes == 0 {
            return self.order_as_postgres(compare);
        }
        let chunk = &self.chunk;
        crate::util::try_sort_unstable_by(&mut self.rows[..self.row_count], |a, b| {
            let row = |entry: &BufferedRow| {
                let start = entry.offset as usize + ORDINAL_BYTES;
                let end = entry.offset as usize + entry.length as usize;
                &chunk[start..end]
            };
            Ok(compare(row(a), row(b))?.then_with(|| a.ordinal.cmp(&b.ordinal)))
        })
    }

    /// Keeps the `bound` rows PostgreSQL's bounded heap keeps, in its order.
    fn order_heap_as_postgres(
        &mut self,
        bound: usize,
        compare: &mut impl FnMut(&[u8], &[u8]) -> Result<Ordering, SqlError>,
    ) -> Result<(), SqlError> {
        let count = self.row_count;
        let Self {
            chunk, rows, exact, ..
        } = self;
        exact.held[..count].copy_from_slice(&rows[..count]);
        for (at, slot) in exact.order[..count].iter_mut().enumerate() {
            *slot = at as u32;
        }
        let held = &exact.held;
        let kept = crate::sql::pg_qsort::pg_tuplesort(
            &mut exact.order[..count],
            Some(bound),
            &mut |left: u32, right: u32| {
                let row = |entry: &BufferedRow| {
                    let start = entry.offset as usize + ORDINAL_BYTES;
                    let end = entry.offset as usize + entry.length as usize;
                    &chunk[start..end]
                };
                compare(row(&held[left as usize]), row(&held[right as usize]))
            },
        )?;
        for (slot, &at) in rows[..kept].iter_mut().zip(&exact.order[..kept]) {
            *slot = exact.held[at as usize];
        }
        self.row_count = kept;
        self.bounded = true;
        self.ordered = true;
        Ok(())
    }

    /// Orders the buffered rows, in arrival order, as PostgreSQL's tuplesort
    /// does: its runs where `dumptuples` wrote them, merged as `mergeruns`
    /// does, or one `pg_qsort`.
    fn order_as_postgres(
        &mut self,
        compare: &mut impl FnMut(&[u8], &[u8]) -> Result<Ordering, SqlError>,
    ) -> Result<(), SqlError> {
        let count = self.row_count;
        let mut model = self.model.expect("sized rows");
        let mut runs = self.pg_run_count;
        if let Some(rows) = model.finish_run()
            && let Some(slot) = self.pg_runs.get_mut(runs)
        {
            *slot = rows as u32;
            runs += 1;
        }
        let Self {
            chunk,
            rows,
            exact,
            pg_runs,
            ..
        } = self;
        exact.held[..count].copy_from_slice(&rows[..count]);
        for (at, slot) in exact.order[..count].iter_mut().enumerate() {
            *slot = at as u32;
        }
        let held = &exact.held;
        let mut compare_at = |left: u32, right: u32| {
            let row = |entry: &BufferedRow| {
                let start = entry.offset as usize + ORDINAL_BYTES;
                let end = entry.offset as usize + entry.length as usize;
                &chunk[start..end]
            };
            compare(row(&held[left as usize]), row(&held[right as usize]))
        };
        if let Some(merge_order) = model.merge_order() {
            crate::sql::pg_qsort::pg_tuplesort_runs(
                &mut exact.order[..count],
                &pg_runs[..runs],
                merge_order,
                crate::sql::pg_qsort::RunScratch {
                    moved: &mut exact.moved,
                    bounds: &mut exact.bounds,
                    heap: &mut exact.heap[..merge_order],
                    cursor: &mut exact.cursor[..merge_order],
                },
                &mut compare_at,
            )?;
        } else {
            crate::sql::pg_qsort::pg_tuplesort(&mut exact.order[..count], None, &mut compare_at)?;
        }
        for (slot, &at) in rows[..count].iter_mut().zip(&exact.order[..count]) {
            *slot = exact.held[at as usize];
        }
        self.ordered = true;
        Ok(())
    }

    fn add_run(
        &mut self,
        store: &mut dyn BlockStore,
        run: ExternalRun,
        level: usize,
        compare: &mut impl FnMut(&[u8], &[u8]) -> Result<Ordering, SqlError>,
    ) -> Result<(), SqlError> {
        if level == MERGE_LEVELS {
            return Err(run_error(SstError::TooManyBlocks));
        }
        let count = self.level_counts[level];
        if count < MERGE_FAN_IN - 1 {
            self.levels[level][count] = Some(run);
            self.level_counts[level] += 1;
            return Ok(());
        }
        let mut runs = self.levels[level];
        runs[MERGE_FAN_IN - 1] = Some(run);
        self.levels[level] = [None; MERGE_FAN_IN];
        self.level_counts[level] = 0;
        let merged = self.merge_runs(store, &runs, compare)?;
        self.add_run(store, merged, level + 1, compare)
    }

    fn merge_runs(
        &mut self,
        store: &mut dyn BlockStore,
        runs: &[Option<ExternalRun>],
        compare: &mut impl FnMut(&[u8], &[u8]) -> Result<Ordering, SqlError>,
    ) -> Result<ExternalRun, SqlError> {
        let rows = runs
            .iter()
            .map(|run| run.expect("merge run").rows)
            .try_fold(0u64, u64::checked_add)
            .ok_or_else(|| {
                sql_err!(
                    sqlstate::PROGRAM_LIMIT_EXCEEDED,
                    "external query row count exhausted"
                )
            })?;
        for (reader, run) in self.readers.iter_mut().zip(runs) {
            reader
                .start(run.expect("merge run"), store)
                .map_err(run_error)?;
        }
        self.writer.reset();
        let mut output = 1u64;
        loop {
            let mut best: Option<usize> = None;
            for index in 0..runs.len() {
                let Some(candidate) = self.readers[index].row() else {
                    continue;
                };
                best = match best {
                    None => Some(index),
                    Some(current) => {
                        let current_row = self.readers[current].row().expect("live");
                        let order =
                            compare(&candidate[ORDINAL_BYTES..], &current_row[ORDINAL_BYTES..])?
                                .then_with(|| {
                                    let candidate_ordinal =
                                        u64::from_le_bytes(candidate[..8].try_into().unwrap());
                                    let current_ordinal =
                                        u64::from_le_bytes(current_row[..8].try_into().unwrap());
                                    candidate_ordinal.cmp(&current_ordinal)
                                });
                        Some(if order.is_lt() { index } else { current })
                    }
                };
            }
            let Some(best) = best else {
                break;
            };
            let length = self.readers[best].current_len;
            self.writer
                .append(store, output, &self.readers[best].current[..length])
                .map_err(run_error)?;
            output += 1;
            self.readers[best].advance(store).map_err(run_error)?;
        }
        let handle = self
            .writer
            .finish(store)
            .map_err(run_error)?
            .expect("merged run is non-empty");
        Ok(ExternalRun { handle, rows })
    }
}

fn row_too_large() -> SqlError {
    sql_err!(
        sqlstate::PROGRAM_LIMIT_EXCEEDED,
        "one projected row exceeds the external-run block capacity"
    )
}

fn run_error(error: SstError) -> SqlError {
    match error {
        SstError::Store(crate::store::StoreError::NotReady) => {
            sql_err!(sqlstate::INTERNAL_IO_WAIT, "durable block read in progress")
        }
        SstError::Store(crate::store::StoreError::ReadOwnersExhausted) => {
            sql_err!(
                sqlstate::PROGRAM_LIMIT_EXCEEDED,
                "object read consumers exceed max_connections and max_subscriptions"
            )
        }
        other => sql_err!(sqlstate::IO_ERROR, "external query run failed: {:?}", other),
    }
}

#[cfg(all(
    test,
    not(all(miri, feature = "miri-ownership")),
    not(feature = "sql-fixture-suite")
))]
mod tests {
    use super::*;
    use crate::mem::budget::Budget;
    use crate::store::MemoryBlockStore;

    #[test]
    fn bounded_rows_sort_without_publishing_a_run() {
        let mut budget = Budget::new(96 << 20);
        let mut store =
            MemoryBlockStore::new(&mut budget, "external test blocks", 32 << 20, 4096).unwrap();
        let mut sorter = ExternalSorter::new(&mut budget).unwrap();
        let mut compare = |left: &[u8], right: &[u8]| Ok(left.cmp(right));
        for row in [b"b".as_slice(), b"a", b"b", b"c"] {
            sorter.push_encoded(&mut store, row, &mut compare).unwrap();
        }
        assert_eq!(sorter.in_memory_rows(&mut compare).unwrap(), Some(4));
        assert_eq!(store.len(), 0);
        assert_eq!(
            (0..4)
                .map(|index| sorter.in_memory_row(index))
                .collect::<Vec<_>>(),
            [b"a".as_slice(), b"b", b"b", b"c"]
        );

        sorter.reset();
        for _ in 0..ROWS_PER_CHUNK + 1 {
            sorter.push_encoded(&mut store, b"x", &mut compare).unwrap();
        }
        assert_eq!(sorter.in_memory_rows(&mut compare).unwrap(), None);
        assert!(store.len() > 0);
        assert_eq!(
            sorter
                .finish(&mut store, &mut compare)
                .unwrap()
                .unwrap()
                .rows(),
            (ROWS_PER_CHUNK + 1) as u64
        );
    }

    #[test]
    fn fan_in_carry_preserves_stable_order() {
        let mut budget = Budget::new(96 << 20);
        let mut store =
            MemoryBlockStore::new(&mut budget, "external test blocks", 32 << 20, 4096).unwrap();
        let mut sorter = ExternalSorter::new(&mut budget).unwrap();
        let mut compare = |left: &[u8], right: &[u8]| {
            let left = crate::sql::exec::decode_projected_pub(left, 0);
            let right = crate::sql::exec::decode_projected_pub(right, 0);
            crate::sql::eval::compare_datums(&left, &right)
        };

        // Force 65 independent runs: run 64 carries through two complete
        // base-eight levels and run 65 leaves a lower-level remainder for
        // `finish` to combine with it.
        let input: Vec<(i64, i64)> = (0..65i64).map(|ordinal| (ordinal % 3, ordinal)).collect();
        for &(key, ordinal) in &input {
            sorter
                .push_projected_by(
                    &mut store,
                    2,
                    |column| {
                        if column == 0 {
                            Datum::Int8(key)
                        } else {
                            Datum::Int8(ordinal)
                        }
                    },
                    &mut compare,
                )
                .unwrap();
            sorter.flush_chunk(&mut store, &mut compare, true).unwrap();
        }
        let run = sorter
            .finish(&mut store, &mut compare)
            .unwrap()
            .expect("rows produce a run");
        let mut seen = Vec::new();
        let mut reader = ExternalRunReader::new();
        reader.start(&mut store, run).unwrap();
        while let Some(row) = reader.row() {
            let key = crate::sql::exec::decode_projected_pub(row, 0);
            let ordinal = crate::sql::exec::decode_projected_pub(row, 1);
            let (Datum::Int8(key), Datum::Int8(ordinal)) = (key, ordinal) else {
                panic!("integer test row")
            };
            seen.push((key, ordinal));
            reader.advance(&mut store).unwrap();
        }
        let mut expected = input;
        expected.sort_by_key(|&(key, _)| key);
        assert_eq!(seen, expected);

        sorter.reset();
        let oversized = "x".repeat(MAX_INLINE_ROW);
        assert!(
            sorter
                .push_projected_by(&mut store, 1, |_| Datum::Text(&oversized), &mut compare,)
                .is_err(),
            "a run row that would require an SST overflow chain must fail loudly"
        );
    }
}
