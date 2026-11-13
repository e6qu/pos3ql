//! COPY FROM's multi-insert buffering, as far as clients can observe it.
//!
//! PostgreSQL (`copyfrom.c`) converts each input row and checks its NOT NULL,
//! CHECK and partition constraints at once, but buffers rows that can be
//! batched and inserts their index entries only when the buffers flush: after
//! 1000 rows, after 64 KB of input lines, before a row goes to a partition
//! that cannot batch, and at the end of the data. So a later row's conversion
//! or CHECK error in the same window wins over an earlier duplicate key, and
//! an index error names only `COPY t, line N` because the input line has
//! moved on. pos3ql stores each row at once, so it holds the first index
//! error back until the flush PostgreSQL would perform.

use crate::sql::eval::{Diagnostic, SqlError};
use crate::util::StackStr;

/// `MAX_BUFFERED_TUPLES`.
const MAX_BUFFERED_TUPLES: usize = 1000;
/// `MAX_BUFFERED_BYTES`: input line bytes, end-of-line excluded.
const MAX_BUFFERED_BYTES: usize = 65535;
/// `MAX_PARTITION_BUFFERS`: partition buffers a flush keeps.
const MAX_PARTITION_BUFFERS: usize = 32;
/// Buffers alive during one window: the kept ones plus one per row.
const MAX_LIVE_BUFFERS: usize = MAX_PARTITION_BUFFERS + MAX_BUFFERED_TUPLES;

/// `CopyInsertMethod`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum CopyInsertMethod {
    /// Every row is inserted, with its index entries, as it is read.
    Single,
    /// Rows of the (unpartitioned) target are buffered.
    Multi,
    /// Rows routed to a partition that can batch are buffered.
    MultiConditional,
}

/// The first index error of the window, in flush order.
struct DeferredIndexError {
    error: SqlError,
    diagnostic: Option<Diagnostic>,
    line: u64,
    /// The failing partition buffer's position in flush order.
    buffer: usize,
}

/// The insert method and the current buffering window of one COPY FROM.
pub(crate) struct CopyBatch {
    method: CopyInsertMethod,
    tuples: usize,
    bytes: usize,
    /// Partition buffers in the order a flush writes them out.
    buffers: [u32; MAX_LIVE_BUFFERS],
    buffer_count: usize,
    previous_leaf: Option<usize>,
    /// The leaf whose buffer holds the current row, once routed.
    current: Option<usize>,
    deferred: Option<DeferredIndexError>,
    /// A flush before routing the current row failed; the row stops there.
    failed: Option<FlushFailure>,
}

/// A flush found a buffered row's index error; the caller raises it.
pub(crate) struct FlushFailure {
    error: SqlError,
    diagnostic: Option<Diagnostic>,
    line: u64,
}

impl CopyBatch {
    pub(crate) const fn new(method: CopyInsertMethod) -> Self {
        Self {
            method,
            tuples: 0,
            bytes: 0,
            buffers: [0; MAX_LIVE_BUFFERS],
            buffer_count: 0,
            previous_leaf: None,
            current: None,
            deferred: None,
            failed: None,
        }
    }

    /// Starts a new input row.
    pub(crate) fn begin_row(&mut self) {
        self.current = None;
    }

    /// Routes the row to `leaf`; `leaf_batches` says the leaf has no BEFORE
    /// or INSTEAD OF row trigger and is not a foreign table. Returns whether
    /// the row is buffered. A conditional COPY that moves to a leaf that
    /// cannot batch flushes first; when that reports an earlier row's error,
    /// this returns `None` and [`Self::take_failure`] holds the error.
    pub(crate) fn route(&mut self, leaf: usize, leaf_batches: bool) -> Option<bool> {
        let buffered = match self.method {
            CopyInsertMethod::Single => false,
            CopyInsertMethod::Multi => true,
            CopyInsertMethod::MultiConditional => {
                if self.previous_leaf != Some(leaf) {
                    self.previous_leaf = Some(leaf);
                    if !leaf_batches
                        && self.tuples > 0
                        && let Err(failure) = self.flush(Some(leaf))
                    {
                        self.failed = Some(failure);
                        return None;
                    }
                }
                leaf_batches
            }
        };
        if buffered {
            let slot = u32::try_from(leaf).expect("table slots fit u32");
            if !self.buffers[..self.buffer_count].contains(&slot) {
                // A window adds at most one buffer per row to the kept ones.
                self.buffers[self.buffer_count] = slot;
                self.buffer_count += 1;
            }
            self.current = Some(leaf);
        }
        Some(buffered)
    }

    /// The failed flush [`Self::route`] reported.
    pub(crate) fn take_failure(&mut self) -> Option<FlushFailure> {
        self.failed.take()
    }

    /// Holds back a buffered row's index error until the flush, keeping the
    /// one PostgreSQL meets first: buffers flush in order, each in row order.
    pub(crate) fn defer(&mut self, error: SqlError, diagnostic: Option<Diagnostic>, line: u64) {
        let leaf = self
            .current
            .expect("a deferred index error belongs to a buffered row");
        let slot = u32::try_from(leaf).expect("table slots fit u32");
        let buffer = self.buffers[..self.buffer_count]
            .iter()
            .position(|&candidate| candidate == slot)
            .expect("a buffered row's leaf has a buffer");
        if self
            .deferred
            .as_ref()
            .is_none_or(|deferred| buffer < deferred.buffer)
        {
            self.deferred = Some(DeferredIndexError {
                error,
                diagnostic,
                line,
                buffer,
            });
        }
    }

    /// Accounts the finished row; `line_bytes` is its input line without the
    /// end-of-line (zero in binary format). Flushes when the window is full.
    pub(crate) fn finish_row(&mut self, line_bytes: usize) -> Result<(), FlushFailure> {
        let Some(leaf) = self.current.take() else {
            return Ok(());
        };
        self.tuples += 1;
        self.bytes = self.bytes.saturating_add(line_bytes);
        if self.tuples >= MAX_BUFFERED_TUPLES || self.bytes >= MAX_BUFFERED_BYTES {
            self.flush(Some(leaf))?;
        }
        Ok(())
    }

    /// The flush at the end of the data.
    pub(crate) fn finish(&mut self) -> Result<(), FlushFailure> {
        if self.tuples > 0 {
            self.flush(None)?;
        }
        Ok(())
    }

    /// `CopyMultiInsertInfoFlush`: writes every buffer out, then keeps the
    /// newest `MAX_PARTITION_BUFFERS`, never dropping `current`'s.
    fn flush(&mut self, current: Option<usize>) -> Result<(), FlushFailure> {
        if let Some(deferred) = self.deferred.take() {
            return Err(FlushFailure {
                error: deferred.error,
                diagnostic: deferred.diagnostic,
                line: deferred.line,
            });
        }
        self.tuples = 0;
        self.bytes = 0;
        let current = current.and_then(|leaf| u32::try_from(leaf).ok());
        while self.buffer_count > MAX_PARTITION_BUFFERS {
            if Some(self.buffers[0]) == current {
                self.buffers[..self.buffer_count].rotate_left(1);
            }
            self.buffers.copy_within(1..self.buffer_count, 0);
            self.buffer_count -= 1;
        }
        Ok(())
    }
}

impl FlushFailure {
    /// The error, reported with the failing row's line but not its text.
    pub(crate) fn raise(self, relation: &str, binary: bool) -> SqlError {
        let frame = crate::sql::error_context::CopyFrame::new(self.line, binary);
        frame.relation.set(StackStr::from_str(relation));
        crate::sql::error_context::reset();
        if let Some(diagnostic) = self.diagnostic {
            crate::sql::eval::stash_full_diagnostic(diagnostic);
        }
        let error = self.error;
        crate::sql::error_context::within(&frame, || Err::<(), _>(error))
            .expect_err("a deferred error stays an error")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sql::eval::sqlstate;

    fn duplicate(text: &str) -> SqlError {
        crate::sql_err!(sqlstate::UNIQUE_VIOLATION, "{}", text)
    }

    fn failure(result: Result<(), FlushFailure>) -> (u64, StackStr<192>) {
        let failure = result.expect_err("flush fails");
        (failure.line, failure.error.message)
    }

    #[test]
    fn a_window_reports_its_first_index_error_when_it_fills() {
        let mut batch = CopyBatch::new(CopyInsertMethod::Multi);
        for line in 1..MAX_BUFFERED_TUPLES as u64 {
            batch.begin_row();
            assert_eq!(batch.route(7, true), Some(true));
            if line == 3 || line == 5 {
                batch.defer(
                    duplicate(if line == 3 { "first" } else { "second" }),
                    None,
                    line,
                );
            }
            assert!(batch.finish_row(1).is_ok());
        }
        batch.begin_row();
        assert_eq!(batch.route(7, true), Some(true));
        let (line, message) = failure(batch.finish_row(1));
        assert_eq!((line, message.as_str()), (3, "first"));
    }

    #[test]
    fn input_bytes_fill_a_window_and_the_end_flushes_the_rest() {
        let mut batch = CopyBatch::new(CopyInsertMethod::Multi);
        batch.begin_row();
        assert_eq!(batch.route(1, true), Some(true));
        assert!(batch.finish_row(MAX_BUFFERED_BYTES - 1).is_ok());
        batch.begin_row();
        assert_eq!(batch.route(1, true), Some(true));
        batch.defer(duplicate("late"), None, 2);
        assert_eq!(failure(batch.finish_row(1)).0, 2);
        let mut batch = CopyBatch::new(CopyInsertMethod::Multi);
        batch.begin_row();
        assert_eq!(batch.route(1, true), Some(true));
        batch.defer(duplicate("end"), None, 1);
        assert!(batch.finish_row(10).is_ok());
        assert_eq!(failure(batch.finish()).0, 1);
    }

    #[test]
    fn partitions_flush_in_buffer_order_and_unbatched_leaves_flush_first() {
        let mut batch = CopyBatch::new(CopyInsertMethod::MultiConditional);
        for (line, leaf) in [(1, 10), (2, 11), (3, 11), (4, 10)] {
            batch.begin_row();
            assert_eq!(batch.route(leaf, true), Some(true));
            if line >= 3 {
                batch.defer(duplicate("dup"), None, line);
            }
            assert!(batch.finish_row(1).is_ok());
        }
        // Leaf 10's buffer flushes before leaf 11's.
        batch.begin_row();
        assert_eq!(batch.route(12, false), None);
        assert_eq!(batch.take_failure().map(|failure| failure.line), Some(4));
        let mut single = CopyBatch::new(CopyInsertMethod::Single);
        single.begin_row();
        assert_eq!(single.route(3, true), Some(false));
        assert!(single.finish_row(5).is_ok() && single.finish().is_ok());
    }

    #[test]
    fn a_flush_keeps_the_newest_buffers_and_the_current_one() {
        let mut batch = CopyBatch::new(CopyInsertMethod::MultiConditional);
        for leaf in 0..40 {
            batch.begin_row();
            assert_eq!(batch.route(leaf, true), Some(true));
            assert!(batch.finish_row(1).is_ok());
        }
        // Leaves 0 to 2 go, 3 moves behind the newest, then 4 to 8 go.
        assert!(batch.flush(Some(3)).is_ok());
        assert_eq!(batch.buffer_count, MAX_PARTITION_BUFFERS);
        assert_eq!(batch.buffers[0], 9);
        assert_eq!(batch.buffers[MAX_PARTITION_BUFFERS - 1], 3);
    }
}
