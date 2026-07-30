//! Bounded external sorting over the provider-neutral block grid.
//!
//! A run is an ordinary immutable SST. Rows are sorted in one block-sized
//! startup buffer, written through `BlockStore`, and combined with a fixed
//! eight-way merge. Binary carry across levels keeps only seven runs per
//! level, so the amount of resident state is fixed while the durable tier
//! supplies the capacity. RAM and local disk see exactly the same blocks as
//! caches; no provider identity reaches this module.

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

#[derive(Clone, Copy)]
pub(crate) struct ExternalRun {
    handle: SstHandle,
    rows: u64,
}

impl ExternalRun {
    pub(crate) fn rows(self) -> u64 {
        self.rows
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
        core::mem::size_of::<std::cell::RefCell<Self>>() + 7 * MAX_PAYLOAD
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
        self.previous_len = 0;
        self.reader.start(run, store).map_err(run_error)
    }

    pub(crate) fn row(&self) -> Option<&[u8]> {
        self.reader.row().map(|prefixed| &prefixed[ORDINAL_BYTES..])
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

    pub(crate) fn advance(&mut self, store: &mut dyn BlockStore) -> Result<(), SqlError> {
        if let Some(prefixed) = self.reader.row() {
            let row = &prefixed[ORDINAL_BYTES..];
            self.previous[..row.len()].copy_from_slice(row);
            self.previous_len = row.len();
        }
        self.reader.advance(store).map_err(run_error)
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
}

impl ExternalSorter {
    pub(crate) fn budget_bytes() -> usize {
        core::mem::size_of::<Self>()
            + MAX_PAYLOAD
            + ROWS_PER_CHUNK * core::mem::size_of::<BufferedRow>()
            + SstWriter::budget_bytes()
            + MERGE_FAN_IN * 4 * MAX_PAYLOAD
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
        })
    }

    pub(crate) fn reset(&mut self) {
        self.chunk_len = 0;
        self.row_count = 0;
        self.levels = [[None; MERGE_FAN_IN]; MERGE_LEVELS];
        self.level_counts = [0; MERGE_LEVELS];
        self.writer.reset();
        self.next_ordinal = 0;
        for reader in &mut self.readers {
            reader.cursor = None;
            reader.current_len = 0;
        }
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
            self.flush_chunk(store, compare)?;
        }
        let offset = self.chunk_len;
        self.chunk[offset..offset + ORDINAL_BYTES]
            .copy_from_slice(&self.next_ordinal.to_le_bytes());
        encode_projected_by_into(
            columns,
            &mut value_at,
            &mut self.chunk[offset + ORDINAL_BYTES..offset + total],
        )?;
        self.rows[self.row_count] = BufferedRow {
            offset: offset as u32,
            length: total as u32,
            ordinal: self.next_ordinal,
        };
        self.row_count += 1;
        self.chunk_len += total;
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
        self.flush_chunk(store, compare)?;
        let mut carry: Option<ExternalRun> = None;
        for level in 0..MERGE_LEVELS {
            let mut runs = [None; MERGE_FAN_IN];
            let mut count = 0usize;
            if let Some(run) = carry.take() {
                runs[count] = Some(run);
                count += 1;
            }
            for slot in 0..self.level_counts[level] {
                runs[count] = self.levels[level][slot];
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

    fn flush_chunk(
        &mut self,
        store: &mut dyn BlockStore,
        compare: &mut impl FnMut(&[u8], &[u8]) -> Result<Ordering, SqlError>,
    ) -> Result<(), SqlError> {
        if self.row_count == 0 {
            return Ok(());
        }
        let chunk = &self.chunk;
        let mut comparison_error = None;
        self.rows[..self.row_count].sort_unstable_by(|a, b| {
            if comparison_error.is_some() {
                return Ordering::Equal;
            }
            let row = |entry: &BufferedRow| {
                let start = entry.offset as usize + ORDINAL_BYTES;
                let end = entry.offset as usize + entry.length as usize;
                &chunk[start..end]
            };
            match compare(row(a), row(b)) {
                Ok(order) => order.then_with(|| a.ordinal.cmp(&b.ordinal)),
                Err(error) => {
                    comparison_error = Some(error);
                    Ordering::Equal
                }
            }
        });
        if let Some(error) = comparison_error {
            return Err(error);
        }
        self.writer.reset();
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
    sql_err!(sqlstate::IO_ERROR, "external query run failed: {:?}", error)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mem::budget::Budget;
    use crate::store::MemoryBlockStore;

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
            sorter.flush_chunk(&mut store, &mut compare).unwrap();
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
