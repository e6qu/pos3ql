//! The memory of a `WindowAgg`'s partition buffer, as `EXPLAIN ANALYZE`
//! reports it (`Storage: Memory  Maximum Storage`).
//!
//! `nodeWindowAgg.c` spools a partition's rows into a tuplestore as its
//! window functions read them and trims the rows no read pointer needs
//! (`tuplestore_trim`), so the most the buffer holds depends on how far each
//! function's pointers lag the current row. This replays that: the spooling
//! (`spool_tuples`), each function's mark and read pointers
//! (`WinSetMarkPosition`, `window_gettupleslot`), the aggregates' incremental
//! frame (`eval_windowaggregates`) and the frame tail of a `RANGE` frame
//! (`update_frametailpos`) over the rows' sizes, for the frames and functions
//! it models. Sources (REL_18_6): `executor/nodeWindowAgg.c`,
//! `utils/sort/tuplestore.c`, `utils/adt/windowfuncs.c`.

use super::postgres_memory::Tuplestore;
use crate::sql::eval::{SqlError, sqlstate};
use crate::sql_err;

/// The most read pointers a modelled window holds: the current row, the
/// aggregates', the frame tail's, and a mark and a read pointer for each of
/// the window functions.
pub(crate) const MAX_POINTERS: usize = 16;

/// A window function that is not an aggregate.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Function {
    RowNumber,
    Rank,
    DenseRank,
}

/// The frame a modelled window runs.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Frame {
    /// `ROWS BETWEEN UNBOUNDED PRECEDING AND CURRENT ROW`.
    Rows,
    /// The default `RANGE BETWEEN UNBOUNDED PRECEDING AND CURRENT ROW`.
    Range,
}

/// What a `WindowAgg` computes.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Window<'f> {
    pub(crate) frame: Frame,
    /// The window has an `ORDER BY` (`ordNumCols != 0`).
    pub(crate) ordered: bool,
    pub(crate) functions: &'f [Function],
    /// At least one plain aggregate is computed.
    pub(crate) aggregates: bool,
}

#[derive(Clone, Copy)]
struct Pointer {
    current: usize,
    eof: bool,
}

/// A window function's `WindowObject` pointers.
#[derive(Clone, Copy)]
struct Object {
    mark: Option<usize>,
    read: usize,
    markpos: i64,
    seekpos: i64,
}

/// The partition buffer of one `WindowAgg`, across its partitions.
pub(crate) struct Buffer<'f> {
    window: Window<'f>,
    store: Tuplestore,
    pointers: [Pointer; MAX_POINTERS],
    used: usize,
    active: usize,
    current: usize,
    aggregates: Object,
    functions: [Object; MAX_POINTERS],
    tail: Option<usize>,
    // The partition being run.
    deleted: usize,
    base: usize,
    spooled: usize,
    partition_spooled: bool,
    /// The partition outgrew `work_mem`: the rest is read in one go and
    /// nothing is trimmed.
    on_disk: bool,
}

/// One partition's rows: each one's `t_len` and its peer group, in order.
#[derive(Clone, Copy)]
pub(crate) struct Partition<'r> {
    pub(crate) lengths: &'r [usize],
    pub(crate) groups: &'r [i64],
}

impl<'f> Buffer<'f> {
    /// `prepare_tuplestore`: the read pointers a window of this shape holds.
    pub(crate) fn new(window: Window<'f>) -> Result<Self, SqlError> {
        if window.functions.len() * 2 + 3 > MAX_POINTERS {
            return Err(sql_err!(
                sqlstate::FEATURE_NOT_SUPPORTED,
                "EXPLAIN ANALYZE of a window with this many functions is not supported"
            ));
        }
        let none = Object {
            mark: None,
            read: 0,
            markpos: -1,
            seekpos: -1,
        };
        let mut buffer = Self {
            window,
            store: if window.functions.is_empty() {
                Tuplestore::new()
            } else {
                Tuplestore::new().backward()
            },
            pointers: [Pointer {
                current: 0,
                eof: false,
            }; MAX_POINTERS],
            used: 1,
            active: 0,
            current: 0,
            aggregates: none,
            functions: [none; MAX_POINTERS],
            tail: None,
            deleted: 0,
            base: 0,
            spooled: 0,
            partition_spooled: false,
            on_disk: false,
        };
        if window.aggregates {
            buffer.aggregates.read = buffer.allocate();
        }
        for at in 0..window.functions.len() {
            let mark = buffer.allocate();
            let read = buffer.allocate();
            buffer.functions[at].mark = Some(mark);
            buffer.functions[at].read = read;
        }
        if window.frame == Frame::Range && window.ordered {
            buffer.tail = Some(buffer.allocate());
        }
        Ok(buffer)
    }

    fn allocate(&mut self) -> usize {
        self.used += 1;
        self.used - 1
    }

    /// The most bytes the buffer held and whether it spilled
    /// (`tuplestore_get_stats`).
    pub(crate) fn finish(self) -> (bool, usize) {
        self.store.finish()
    }

    fn count(&self) -> usize {
        self.store.count()
    }

    // `tuplestore_gettuple`, `tuplestore_skiptuples` and `tuplestore_advance`
    // over the active pointer of an in-memory store.

    fn get_forward(&mut self) -> bool {
        let count = self.count();
        let pointer = &mut self.pointers[self.active];
        if pointer.eof {
            return false;
        }
        if pointer.current < count {
            pointer.current += 1;
            return true;
        }
        pointer.eof = true;
        false
    }

    fn get_backward(&mut self) -> bool {
        let count = self.count();
        let deleted = self.deleted;
        let pointer = &mut self.pointers[self.active];
        if pointer.eof {
            pointer.current = count;
            pointer.eof = false;
        } else {
            if pointer.current <= deleted {
                return false;
            }
            pointer.current -= 1;
        }
        pointer.current > deleted
    }

    fn skip_forward(&mut self, tuples: usize) -> bool {
        if tuples == 0 {
            return true;
        }
        let count = self.count();
        let pointer = &mut self.pointers[self.active];
        if pointer.eof {
            return false;
        }
        if count - pointer.current >= tuples {
            pointer.current += tuples;
            return true;
        }
        pointer.current = count;
        pointer.eof = true;
        false
    }

    fn skip_backward(&mut self, mut tuples: usize) -> bool {
        if tuples == 0 {
            return true;
        }
        let count = self.count();
        let deleted = self.deleted;
        let pointer = &mut self.pointers[self.active];
        if pointer.eof {
            pointer.current = count;
            pointer.eof = false;
            tuples -= 1;
        }
        if pointer.current - deleted > tuples {
            pointer.current -= tuples;
            return true;
        }
        pointer.current = deleted;
        false
    }

    /// `tuplestore_puttupleslot` of the partition's next row.
    fn put(&mut self, partition: Partition<'_>) -> Result<(), SqlError> {
        let count = self.count();
        for (at, pointer) in self.pointers[..self.used].iter_mut().enumerate() {
            if pointer.eof && at != self.active {
                pointer.eof = false;
                pointer.current = count;
            }
        }
        self.store.put(partition.lengths[self.spooled]);
        self.on_disk = self.store.spilled();
        self.spooled += 1;
        Ok(())
    }

    /// `spool_tuples`: reads rows up to and including `to`, or the whole
    /// partition.
    fn spool(&mut self, partition: Partition<'_>, to: Option<usize>) -> Result<(), SqlError> {
        if self.partition_spooled {
            return Ok(());
        }
        // A buffer on disk reads the whole partition in one go.
        let to = to.filter(|_| !self.on_disk);
        while to.is_none_or(|to| self.spooled <= to) {
            if self.spooled == partition.lengths.len() {
                self.partition_spooled = true;
                break;
            }
            self.put(partition)?;
        }
        Ok(())
    }

    /// `window_gettupleslot` over `object`'s read pointer; `false` past the
    /// partition.
    fn fetch(
        &mut self,
        partition: Partition<'_>,
        function: Option<usize>,
        position: usize,
    ) -> Result<bool, SqlError> {
        self.spool(partition, Some(position))?;
        if position >= self.spooled {
            return Ok(false);
        }
        let object = function.map_or(self.aggregates, |at| self.functions[at]);
        self.active = object.read;
        let position = position as i64;
        let mut seek = object.seekpos;
        if seek < position - 1 {
            self.skip_forward((position - 1 - seek) as usize);
            seek = position - 1;
        } else if seek > position + 1 {
            self.skip_backward((seek - (position + 1)) as usize);
            seek = position + 1;
        } else if seek == position {
            self.get_forward();
            seek += 1;
        }
        if seek > position {
            self.get_backward();
            seek -= 1;
        } else {
            self.get_forward();
            seek += 1;
        }
        let object = match function {
            Some(at) => &mut self.functions[at],
            None => &mut self.aggregates,
        };
        object.seekpos = seek;
        Ok(true)
    }

    /// `WinSetMarkPosition`.
    fn set_mark(&mut self, function: usize, mark: usize) {
        let mark = mark as i64;
        let object = self.functions[function];
        if mark > object.markpos {
            self.active = object.mark.expect("a function's mark pointer");
            self.skip_forward((mark - object.markpos) as usize);
            self.functions[function].markpos = mark;
        }
        self.active = object.read;
        if mark > object.seekpos {
            self.skip_forward((mark - object.seekpos) as usize);
            self.functions[function].seekpos = mark;
        }
    }

    fn peers(&self, partition: Partition<'_>, left: usize, right: usize) -> bool {
        !self.window.ordered || partition.groups[left] == partition.groups[right]
    }

    /// `rank_up`'s reads and mark for the function at `at`, on row `row`.
    fn rank_up(&mut self, partition: Partition<'_>, at: usize, row: usize) -> Result<(), SqlError> {
        if row > 0 && self.window.ordered {
            self.fetch(partition, Some(at), row - 1)?;
            self.fetch(partition, Some(at), row)?;
        }
        self.set_mark(at, row);
        Ok(())
    }

    /// `eval_windowaggregates` for a frame that starts unbounded: it keeps
    /// reading rows into the frame up to the first one past it.
    fn aggregate(
        &mut self,
        partition: Partition<'_>,
        row: usize,
        upto: &mut usize,
        held: &mut bool,
    ) -> Result<(), SqlError> {
        // The current row lies within the frame read so far, which ends at
        // the current row or its last peer.
        if *upto > row {
            return Ok(());
        }
        loop {
            if !*held {
                if !self.fetch(partition, None, *upto)? {
                    break;
                }
                *held = true;
            }
            let beyond = match self.window.frame {
                Frame::Rows => *upto > row,
                Frame::Range => *upto > row && !self.peers(partition, *upto, row),
            };
            if beyond {
                break;
            }
            *upto += 1;
            *held = false;
        }
        Ok(())
    }

    /// `update_frametailpos` of a `RANGE` frame ending at the current row
    /// with an `ORDER BY`.
    fn frame_tail(
        &mut self,
        partition: Partition<'_>,
        row: usize,
        position: &mut usize,
        held: &mut bool,
    ) -> Result<(), SqlError> {
        self.active = self.tail.expect("a frame tail pointer");
        if *position == 0 && !*held {
            self.get_forward();
            *held = true;
        }
        while *held {
            if *position > row && !self.peers(partition, *position, row) {
                break;
            }
            *position += 1;
            self.spool(partition, Some(*position))?;
            if !self.get_forward() {
                *held = false;
            }
        }
        Ok(())
    }

    /// `tuplestore_trim`.
    fn trim(&mut self, partition: Partition<'_>) {
        if self.on_disk {
            return;
        }
        let count = self.count();
        let oldest = self.pointers[..self.used]
            .iter()
            .filter(|pointer| !pointer.eof)
            .fold(count, |oldest, pointer| oldest.min(pointer.current));
        let removed = oldest.saturating_sub(1);
        if removed == 0 {
            return;
        }
        self.store.update_most();
        for row in self.deleted..removed {
            self.store.release(partition.lengths[self.base + row]);
        }
        self.deleted = removed;
        if removed < count / 8 {
            return;
        }
        self.store.slide(removed);
        self.base += removed;
        self.deleted = 0;
        for pointer in &mut self.pointers[..self.used] {
            if !pointer.eof {
                pointer.current -= removed;
            }
        }
    }

    /// Runs one partition: `ExecWindowAgg`'s loop over its rows, then
    /// `release_partition`.
    pub(crate) fn run(&mut self, partition: Partition<'_>) -> Result<(), SqlError> {
        let rows = partition.lengths.len();
        self.deleted = 0;
        self.base = 0;
        self.spooled = 0;
        self.partition_spooled = false;
        self.on_disk = false;
        for pointer in &mut self.pointers[..self.used] {
            *pointer = Pointer {
                current: 0,
                eof: false,
            };
        }
        let none = Object {
            mark: None,
            read: 0,
            markpos: -1,
            seekpos: -1,
        };
        self.aggregates = Object {
            read: self.aggregates.read,
            ..none
        };
        for at in 0..self.window.functions.len() {
            let held = self.functions[at];
            self.functions[at] = Object {
                mark: held.mark,
                read: held.read,
                ..none
            };
        }
        let (mut upto, mut held) = (0usize, false);
        let (mut tail_position, mut tail_held) = (0usize, false);
        if rows > 0 {
            // `begin_partition` stores the first row.
            self.put(partition)?;
        }
        for row in 0..rows {
            self.current = row;
            self.spool(partition, Some(row))?;
            self.active = 0;
            self.get_forward();
            for (at, function) in self.window.functions.iter().enumerate() {
                match function {
                    Function::RowNumber => self.set_mark(at, row),
                    Function::Rank | Function::DenseRank => self.rank_up(partition, at, row)?,
                }
            }
            if self.window.aggregates {
                self.aggregate(partition, row, &mut upto, &mut held)?;
            }
            if self.tail.is_some() {
                self.frame_tail(partition, row, &mut tail_position, &mut tail_held)?;
            }
            self.trim(partition);
        }
        self.spool(partition, Some(rows))?;
        self.store.clear();
        Ok(())
    }
}

#[cfg(all(test, not(feature = "sql-fixture-suite")))]
mod tests {
    use super::*;

    fn run(window: Window<'_>, lengths: &[usize], groups: &[i64]) -> (bool, usize) {
        let mut buffer = Buffer::new(window).unwrap();
        buffer.run(Partition { lengths, groups }).unwrap();
        buffer.finish()
    }

    #[test]
    fn ranking_functions_keep_only_the_rows_near_the_mark() {
        // 3000 rows of two integers: PostgreSQL reports 17kB, the pointer
        // array of 2048 slots and a few rows.
        let lengths = [24usize; 3000];
        let groups: Vec<i64> = (0..3000).collect();
        let (disk, bytes) = run(
            Window {
                frame: Frame::Rows,
                ordered: true,
                functions: &[Function::RowNumber],
                aggregates: false,
            },
            &lengths,
            &groups,
        );
        assert!(!disk);
        assert_eq!(bytes.div_ceil(1024), 17);
    }

    #[test]
    fn an_unordered_aggregate_holds_its_whole_partition() {
        let lengths = [24usize; 430];
        let groups = [0i64; 430];
        let (disk, bytes) = run(
            Window {
                frame: Frame::Range,
                ordered: false,
                functions: &[],
                aggregates: true,
            },
            &lengths,
            &groups,
        );
        assert!(!disk);
        assert_eq!(bytes, 16_392 + 430 * 32);
    }
}
