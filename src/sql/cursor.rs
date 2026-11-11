//! SQL cursor positions and startup-reserved result storage. Snapshot retention
//! and execution timing are separate: an insensitive cursor keeps its selected
//! row versions; when expressions run and rows lock depends on execution.

use crate::config::Config;
use crate::mem::budget::{Budget, BudgetError};
use crate::mem::buffer::FixedBuf;
use crate::pg::respond::ResultFmt;
use crate::sql_err;
use crate::storage::SqlName;
use crate::util::StackStr;

use core::cell::Cell;

use super::eval::{SqlError, sqlstate};
use super::types::Datum;
use crate::mem::arena::Arena;

mod query;
mod scroll;
mod stream;
pub(crate) use query::{CursorLocal, CursorQuery, CursorStatement};
pub(crate) use scroll::{Revisit, ScrollAction, ScrollInput, ScrollScan, VisitOutcome};
pub(crate) use stream::{
    FetchStream, ResultStream, ResultStreams, STREAM_HEADER_BYTES, StreamProtocol, StreamState,
};

/// One FETCH/MOVE motion, normalized by the parser: positive counts move
/// forward, negative backward (`NEXT` is `Count(1)`, `PRIOR` is `Count(-1)`,
/// `FIRST` is `Absolute(1)`, `LAST` is `Absolute(-1)`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FetchMotion {
    Count(i32),
    /// `BACKWARD 0`: like `Count(0)`, except that off a row PostgreSQL still
    /// asks a NO SCROLL cursor to scan backward.
    BackwardZero,
    All,
    BackwardAll,
    Absolute(i32),
    Relative(i32),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CursorScroll {
    Default,
    Scroll,
    NoScroll,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct CursorOpenSite {
    pub owner: super::type_input_plans::Owner,
    pub source: super::type_inputs::Source,
    pub position: usize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct CursorOpenAttempt {
    index: usize,
    generation: core::num::NonZeroU64,
}

#[derive(Clone, Copy)]
pub(crate) struct CursorCreationMark(core::num::NonZeroU64);

impl CursorOpenAttempt {
    pub(crate) const fn index(self) -> usize {
        self.index
    }
}

pub struct CursorPool {
    slots: Vec<CursorSlot>,
    /// Row indexes selected by the last [`Self::fetch`], in emission order.
    emit: Vec<u32>,
    next_unnamed: u64,
    next_plan_generation: u64,
    /// The tables each of this connection's held result streams reads. A
    /// suspended portal keeps them open between statements, and a running
    /// stream's routine calls run nested statements, so DDL on them must see
    /// them.
    stream_tables: Box<[StreamTables]>,
}

/// The physical tables one result stream reads.
pub(crate) type StreamTables = [Option<usize>; crate::sql::query::MAX_RETAINED_SOURCES];
pub(crate) const NO_STREAM_TABLES: StreamTables = [None; crate::sql::query::MAX_RETAINED_SOURCES];

const CURSOR_STATEMENT_BYTES: usize = 4096;
pub(crate) const DESCRIPTION_BYTES: usize = 1024;

thread_local! {
    static ACTIVE_CURSORS: Cell<*const CursorPool> = const { Cell::new(core::ptr::null()) };
}

pub(crate) struct ActiveCursorGuard(*const CursorPool);

impl Drop for ActiveCursorGuard {
    fn drop(&mut self) {
        ACTIVE_CURSORS.with(|active| active.set(self.0));
    }
}

/// Makes the current connection's startup-bounded cursor pool visible to the
/// `pg_cursor()` catalog SRF while a statement executes. Nested routine
/// statements restore the outer pool on return.
pub(crate) fn enter_active(pool: *const CursorPool) -> ActiveCursorGuard {
    let prior = ACTIVE_CURSORS.with(|active| active.replace(pool));
    ActiveCursorGuard(prior)
}

pub(crate) fn with_active<R>(f: impl FnOnce(Option<&CursorPool>) -> R) -> R {
    ACTIVE_CURSORS.with(|active| {
        let pool = active.get();
        // SAFETY: `enter_active` is guarded by synchronous statement execution;
        // its guard restores the pointer before the connection-owned pool can
        // be dropped. Catalog evaluation never mutates the pool while this
        // shared reference is live.
        f((!pool.is_null()).then(|| unsafe { &*pool }))
    })
}

pub(crate) fn require_table_idle(table: usize, name: &str, command: &str) -> Result<(), SqlError> {
    let busy = with_active(|pool| {
        pool.is_some_and(|pool| {
            pool.slots.iter().any(|slot| {
                slot.active
                    && slot
                        .query
                        .retained
                        .as_ref()
                        .is_some_and(|query| query.uses_table(table))
            }) || pool
                .stream_tables
                .iter()
                .any(|tables| tables.contains(&Some(table)))
        })
    });
    if busy {
        return Err(sql_err!(
            sqlstate::OBJECT_IN_USE,
            "cannot {} \"{}\" because it is being used by active queries in this session",
            command,
            name
        ));
    }
    Ok(())
}

#[derive(Clone, Copy)]
pub(crate) struct CursorInfo<'a> {
    pub name: &'a str,
    pub statement: &'a str,
    pub holdable: bool,
    pub binary: bool,
    pub scrollable: bool,
    pub created_at: i64,
}

struct CursorSlot {
    active: bool,
    plan_generation: Option<core::num::NonZeroU64>,
    open_site: Option<CursorOpenSite>,
    name: SqlName,
    statement: StackStr<CURSOR_STATEMENT_BYTES>,
    created_at: i64,
    scroll: CursorScroll,
    hold: bool,
    binary: bool,
    /// Created in the still-open transaction: a rollback closes it even WITH
    /// HOLD (holdability begins at commit, as PostgreSQL has it).
    tentative: bool,
    failed: bool,
    query: CursorQuery,
    description: FixedBuf,
    /// Typed output rows grow from the front; saved input rows of a
    /// scrollable or sorted source grow from the back of the same bytes.
    rows: FixedBuf,
    spans: RowIndex,
    /// PostgreSQL motion over saved inputs, for a scrollable source whose
    /// revisited rows are evaluated again.
    scan: Option<ScrollScan>,
    /// A held scroll cursor is saving its rows at commit.
    holding: bool,
    locking: RowLocking,
    /// Logical input rows before the first saved row of the current fetch.
    row_base: usize,
    produced: usize,
    /// An I/O retry must keep the current fetch's saved rows.
    request: Option<(FetchMotion, bool)>,
    /// A windowed scroll cursor rereading its input from the first row:
    /// rows `lo..=hi` are saved for the current fetch.
    refill: Option<(usize, usize)>,
    /// The input's row count once it has ended; a windowed cursor that read
    /// its input again may have produced fewer rows since.
    input_rows: Option<usize>,
    /// PostgreSQL's cursor position: 0 before the first row, `1..=n` on a
    /// row, `n + 1` after the last.
    position: i64,
}

impl CursorSlot {
    fn reset_rows(&mut self) {
        self.rows.clear();
        self.rows.clear_tail();
        self.spans.clear();
        self.spans.clear_inputs();
        self.scan = None;
        self.holding = false;
        self.locking = RowLocking::default();
        self.refill = None;
        self.input_rows = None;
    }

    /// Rows the cursor knows of: all of them once the input has ended.
    fn known_rows(&self) -> i64 {
        self.input_rows.unwrap_or(self.produced).max(self.produced) as i64
    }

    fn require_runnable(&self) -> Result<(), SqlError> {
        if self.failed {
            return Err(sql_err!(
                sqlstate::OBJECT_NOT_IN_PREREQUISITE_STATE,
                "portal \"{}\" cannot be run",
                self.name.as_str()
            ));
        }
        Ok(())
    }

    fn validate_motion(
        &self,
        motion: FetchMotion,
        move_only: bool,
        input_finished: bool,
    ) -> Result<(), SqlError> {
        self.require_runnable()?;
        let on_row = self.position >= 1 && self.position <= self.known_rows();
        let backward = matches!(motion, FetchMotion::Count(k) | FetchMotion::Relative(k) if k < 0)
            || (!move_only && motion == FetchMotion::BackwardZero)
            || (matches!(motion, FetchMotion::BackwardAll) && !(move_only && self.position == 0))
            // A negative absolute position first consumes the forward input;
            // its final backward step is rejected after that work.
            || matches!(motion, FetchMotion::Absolute(k) if (k < 0 && input_finished)
                || (k >= 0 && i64::from(k) <= self.position && self.position != 0))
            || (!move_only
                && on_row
                && matches!(motion, FetchMotion::Count(0) | FetchMotion::Relative(0)));
        if self.scroll == CursorScroll::NoScroll && backward {
            return Err(forward_only());
        }
        Ok(())
    }
}

// Each saved row has a four-byte length and a two-byte column count; a saved
// input has a 17-byte row identity and version and a two-byte column count.
const MIN_CURSOR_ROW_BYTES: usize = 6;

/// Spans of output rows (from the front) and saved inputs (from the back) in
/// one startup-sized index. Both live in `cursor_bytes` and each takes at
/// least [`MIN_CURSOR_ROW_BYTES`], so together they fit the index.
struct RowIndex {
    entries: Box<[(u32, u32)]>,
    outputs: usize,
    inputs: usize,
}

impl RowIndex {
    fn new(capacity: usize) -> Self {
        Self {
            entries: vec![(0, 0); capacity].into_boxed_slice(),
            outputs: 0,
            inputs: 0,
        }
    }

    fn capacity(&self) -> usize {
        self.entries.len()
    }

    fn len(&self) -> usize {
        self.outputs
    }

    fn is_empty(&self) -> bool {
        self.outputs == 0
    }

    fn clear(&mut self) {
        self.outputs = 0;
    }

    fn truncate(&mut self, outputs: usize) {
        self.outputs = self.outputs.min(outputs);
    }

    fn get(&self, index: usize) -> Option<&(u32, u32)> {
        self.entries[..self.outputs].get(index)
    }

    fn last(&self) -> Option<&(u32, u32)> {
        self.entries[..self.outputs].last()
    }

    fn full(&self) -> bool {
        self.outputs + self.inputs == self.entries.len()
    }

    fn push(&mut self, span: (u32, u32)) -> Result<(), SqlError> {
        if self.full() {
            return Err(sql_err!(
                sqlstate::PROGRAM_LIMIT_EXCEEDED,
                "cursor row index exceeds cursor_bytes-derived capacity of {} rows",
                self.capacity()
            ));
        }
        self.entries[self.outputs] = span;
        self.outputs += 1;
        Ok(())
    }

    fn input(&self, index: usize) -> Option<(u32, u32)> {
        (index < self.inputs).then(|| self.entries[self.entries.len() - 1 - index])
    }

    fn push_input(&mut self, span: (u32, u32)) -> Result<(), SqlError> {
        if self.full() {
            return Err(history_full());
        }
        self.inputs += 1;
        let at = self.entries.len() - self.inputs;
        self.entries[at] = span;
        Ok(())
    }

    fn inputs_len(&self) -> usize {
        self.inputs
    }

    /// Saved inputs, last first: input `i` is at `len - 1 - i`.
    fn inputs_mut(&mut self) -> &mut [(u32, u32)] {
        let len = self.entries.len();
        &mut self.entries[len - self.inputs..]
    }

    fn clear_inputs(&mut self) {
        self.inputs = 0;
    }
}

/// Progress of a cursor that locks each row it fetches.
#[derive(Clone, Copy, Default)]
struct RowLocking {
    /// The next sorted input, once every qualifying row is saved and sorted.
    sorted: Option<usize>,
    /// Where the pending row's projected output starts while it waits for
    /// its lock.
    output: Option<usize>,
    /// The pending row is a newer version that must pass the filter again.
    recheck: bool,
}

/// Applies one FETCH/MOVE motion to a cursor position over `n` known rows:
/// visits each row position the motion steps onto, in order, and returns the
/// new position (0 before the first row, `n + 1` after the last).
fn motion_rows(mut position: i64, n: i64, motion: FetchMotion, mut visit: impl FnMut(i64)) -> i64 {
    match motion {
        FetchMotion::Count(0) | FetchMotion::BackwardZero => {
            // Re-fetch the current row, position unchanged.
            visit(position);
        }
        FetchMotion::Count(k) if k > 0 => {
            for _ in 0..k {
                if position > n {
                    break;
                }
                position += 1;
                visit(position);
            }
            position = position.min(n + 1);
        }
        FetchMotion::Count(k) => {
            for _ in 0..-i64::from(k) {
                if position < 1 {
                    break;
                }
                position -= 1;
                visit(position);
            }
            position = position.max(0);
        }
        FetchMotion::All => {
            while position <= n {
                position += 1;
                visit(position);
            }
            position = n + 1;
        }
        FetchMotion::BackwardAll => {
            while position >= 1 {
                position -= 1;
                visit(position);
            }
            position = 0;
        }
        FetchMotion::Absolute(k) => {
            let target = if k >= 0 {
                i64::from(k)
            } else {
                n + 1 + i64::from(k)
            };
            position = target.clamp(0, n + 1);
            visit(position);
        }
        FetchMotion::Relative(k) => {
            position = (position + i64::from(k)).clamp(0, n + 1);
            visit(position);
        }
    }
    position
}

/// PostgreSQL's error for backward motion on a NO SCROLL cursor.
pub(crate) fn forward_only() -> SqlError {
    super::eval::stash_diagnostic(
        None,
        Some(StackStr::from_str(
            "Declare it with SCROLL option to enable backward scan.",
        )),
    );
    sql_err!(
        sqlstate::OBJECT_NOT_IN_PREREQUISITE_STATE,
        "cursor can only scan forward"
    )
}

pub(crate) fn history_full() -> SqlError {
    sql_err!(
        sqlstate::PROGRAM_LIMIT_EXCEEDED,
        "cursor saved input rows exceed cursor_bytes; raise it or narrow the query"
    )
}

const fn cursor_row_capacity(config: &Config) -> usize {
    config.cursor_bytes / MIN_CURSOR_ROW_BYTES
}

fn seal_description(description: &mut FixedBuf) -> Result<usize, SqlError> {
    let malformed = || {
        sql_err!(
            sqlstate::INTERNAL_ERROR,
            "cursor captured a torn description"
        )
    };
    let bytes = description.readable();
    let mut at = 0usize;
    let mut captured = None;
    while at < bytes.len() {
        let header = bytes.get(at..at.saturating_add(5)).ok_or_else(malformed)?;
        let length = u32::from_be_bytes(header[1..5].try_into().unwrap()) as usize;
        if length < 4 {
            return Err(malformed());
        }
        let end = at
            .checked_add(1)
            .and_then(|at| at.checked_add(length))
            .ok_or_else(malformed)?;
        if end > bytes.len() {
            return Err(malformed());
        }
        if header[0] == b'T' {
            if captured.is_some() {
                return Err(sql_err!(
                    sqlstate::INTERNAL_ERROR,
                    "cursor captured two descriptions"
                ));
            }
            captured = Some((at, end));
        }
        at = end;
    }
    let (start, end) = captured.ok_or_else(|| {
        sql_err!(
            sqlstate::INTERNAL_ERROR,
            "cursor did not capture a description"
        )
    })?;
    let mut types = [0; crate::pg::respond::MAX_RESULT_COLS];
    let columns = description_type_oids(&bytes[start..end], &mut types)?;
    description.filled_mut().copy_within(start..end, 0);
    description.truncate_to(end - start);
    Ok(columns)
}

fn seal_rows(rows: &FixedBuf, columns: usize, spans: &mut RowIndex) -> Result<(), SqlError> {
    let malformed = || sql_err!(sqlstate::INTERNAL_ERROR, "cursor captured a torn typed row");
    let bytes = rows.readable();
    let mut at = spans
        .last()
        .map_or(0, |&(offset, length)| offset as usize + length as usize);
    while at < bytes.len() {
        let header = bytes.get(at..at.saturating_add(4)).ok_or_else(malformed)?;
        let length = u32::from_le_bytes(header.try_into().unwrap()) as usize;
        let start = at.checked_add(4).ok_or_else(malformed)?;
        let end = start.checked_add(length).ok_or_else(malformed)?;
        let row = bytes.get(start..end).ok_or_else(malformed)?;
        if row.len() < 2 || super::exec::projected_row_width(row) != columns {
            return Err(malformed());
        }
        spans.push((start as u32, length as u32))?;
        at = end;
    }
    Ok(())
}

/// Extracts type OIDs from one captured RowDescription. FETCH uses this at
/// execution time so an unavailable binary send function is reported after
/// Bind and Describe, matching PostgreSQL's extended-protocol boundary.
pub(crate) fn description_type_oids(
    description: &[u8],
    output: &mut [i32],
) -> Result<usize, SqlError> {
    let mut names = [None; crate::pg::respond::MAX_RESULT_COLS];
    description_columns(description, &mut names, output)
}

/// Decodes one protocol description. SQL records use the cursor's separately
/// retained declared type identities rather than these wire type OIDs.
fn description_columns<'a>(
    description: &'a [u8],
    names: &mut [Option<&'a str>],
    type_oids: &mut [i32],
) -> Result<usize, SqlError> {
    let corrupt = || {
        sql_err!(
            sqlstate::INTERNAL_ERROR,
            "cursor row description is malformed"
        )
    };
    if description.len() < 7 || description[0] != b'T' {
        return Err(corrupt());
    }
    let declared = u32::from_be_bytes(description[1..5].try_into().unwrap()) as usize;
    if declared + 1 != description.len() {
        return Err(corrupt());
    }
    let count = u16::from_be_bytes(description[5..7].try_into().unwrap()) as usize;
    if count > names.len() || count > type_oids.len() {
        return Err(sql_err!(
            sqlstate::PROGRAM_LIMIT_EXCEEDED,
            "cursor row description has more than {} columns",
            names.len().min(type_oids.len())
        ));
    }
    let mut at = 7usize;
    for index in 0..count {
        let name_len = description
            .get(at..)
            .and_then(|bytes| bytes.iter().position(|byte| *byte == 0))
            .ok_or_else(corrupt)?;
        names[index] =
            Some(core::str::from_utf8(&description[at..at + name_len]).map_err(|_| corrupt())?);
        at += name_len + 1;
        let fields = description.get(at..at + 18).ok_or_else(corrupt)?;
        type_oids[index] = i32::from_be_bytes(fields[6..10].try_into().unwrap());
        at += 18;
    }
    if at != description.len() {
        return Err(corrupt());
    }
    Ok(count)
}

fn push_emit(emit: &mut Vec<u32>, index: u32) -> Result<(), SqlError> {
    if emit.len() == emit.capacity() {
        return Err(sql_err!(
            sqlstate::PROGRAM_LIMIT_EXCEEDED,
            "cursor fetch exceeds its cursor_bytes-derived row capacity of {} rows",
            emit.capacity()
        ));
    }
    emit.push(index);
    Ok(())
}

impl CursorPool {
    pub(crate) fn creation_mark(&self) -> CursorCreationMark {
        CursorCreationMark(
            core::num::NonZeroU64::new(self.next_plan_generation)
                .expect("cursor identities never wrap"),
        )
    }

    pub(crate) fn rollback_creations(&mut self, mark: CursorCreationMark) {
        for index in 0..self.slots.len() {
            if self.slots[index]
                .plan_generation
                .is_some_and(|generation| generation >= mark.0)
            {
                self.abandon(index);
            }
        }
    }
    pub fn budget_bytes(config: &Config) -> usize {
        let row_capacity = cursor_row_capacity(config);
        config
            .max_cursors
            .saturating_mul(
                config
                    .cursor_bytes
                    .saturating_add(config.cursor_query_bytes)
                    .saturating_add(DESCRIPTION_BYTES)
                    .saturating_add(core::mem::size_of::<CursorSlot>())
                    .saturating_add(
                        row_capacity.saturating_mul(core::mem::size_of::<(u32, u32)>()),
                    ),
            )
            .saturating_add(row_capacity.saturating_mul(core::mem::size_of::<u32>()))
            .saturating_add(
                config
                    .result_stream_slots
                    .saturating_mul(core::mem::size_of::<StreamTables>()),
            )
    }

    pub fn new(config: &Config, budget: &mut Budget) -> Result<Self, BudgetError> {
        let row_capacity = cursor_row_capacity(config);
        budget.draw_array(
            config.max_cursors,
            core::mem::size_of::<CursorSlot>(),
            "cursor_slots",
        )?;
        let mut slots = Vec::with_capacity(config.max_cursors);
        for _ in 0..config.max_cursors {
            budget.draw_array(
                row_capacity,
                core::mem::size_of::<(u32, u32)>(),
                "cursor_row_spans",
            )?;
            slots.push(CursorSlot {
                active: false,
                plan_generation: None,
                open_site: None,
                name: SqlName::parse("").expect("empty fits"),
                statement: StackStr::new(),
                created_at: 0,
                scroll: CursorScroll::Default,
                hold: false,
                binary: false,
                tentative: false,
                failed: false,
                query: CursorQuery::new(budget, config.cursor_query_bytes)?,
                description: FixedBuf::new(budget, "cursor_description", DESCRIPTION_BYTES)?,
                rows: FixedBuf::new(budget, "cursor_typed_rows", config.cursor_bytes)?,
                spans: RowIndex::new(row_capacity),
                scan: None,
                holding: false,
                locking: RowLocking::default(),
                row_base: 0,
                produced: 0,
                request: None,
                refill: None,
                input_rows: None,
                position: 0,
            });
        }
        budget.draw_array(row_capacity, core::mem::size_of::<u32>(), "cursor_emit")?;
        budget.draw_array(
            config.result_stream_slots,
            core::mem::size_of::<StreamTables>(),
            "result_stream_tables",
        )?;
        Ok(Self {
            slots,
            emit: Vec::with_capacity(row_capacity),
            next_unnamed: 1,
            next_plan_generation: 1,
            stream_tables: vec![NO_STREAM_TABLES; config.result_stream_slots].into_boxed_slice(),
        })
    }

    /// Closes every cursor — used when a connection slot is recycled for a new
    /// client so no cursor leaks across sessions.
    pub fn clear(&mut self) {
        for s in &mut self.slots {
            s.query.clear();
            s.active = false;
            s.plan_generation = None;
            s.open_site = None;
        }
        self.emit.clear();
        self.next_unnamed = 1;
        self.stream_tables.fill(NO_STREAM_TABLES);
    }

    /// Records the tables a newly held result stream reads, or none once the
    /// stream is released.
    pub(crate) fn hold_stream_tables(&mut self, stream: usize, tables: StreamTables) {
        self.stream_tables[stream] = tables;
    }

    fn find(&self, name: &str) -> Option<usize> {
        self.slots
            .iter()
            .position(|s| s.active && s.name.as_str() == name)
    }

    /// Reserves a slot for a fresh cursor, handing back its index; the caller
    /// fills the buffers and then calls [`Self::seal`].
    pub fn open(
        &mut self,
        name: &str,
        statement: &str,
        scroll: CursorScroll,
        hold: bool,
        binary: bool,
        created_at: i64,
    ) -> Result<usize, SqlError> {
        if self.slots.iter().any(|slot| {
            (slot.active || slot.plan_generation.is_some()) && slot.name.as_str() == name
        }) {
            return Err(sql_err!(
                sqlstate::DUPLICATE_CURSOR,
                "cursor \"{}\" already exists",
                name
            ));
        }
        let Some(at) = self
            .slots
            .iter()
            .position(|s| !s.active && s.plan_generation.is_none())
        else {
            return Err(sql_err!(
                sqlstate::PROGRAM_LIMIT_EXCEEDED,
                "too many open cursors (limit {})",
                self.slots.len()
            ));
        };
        let name = SqlName::parse(name)?;
        let statement = StackStr::<CURSOR_STATEMENT_BYTES>::from_str(statement);
        if statement.is_truncated() {
            return Err(sql_err!(
                sqlstate::PROGRAM_LIMIT_EXCEEDED,
                "cursor statement exceeds {} bytes",
                CURSOR_STATEMENT_BYTES
            ));
        }
        let exhausted = || {
            sql_err!(
                sqlstate::PROGRAM_LIMIT_EXCEEDED,
                "cursor plan identities exhausted"
            )
        };
        let generation =
            core::num::NonZeroU64::new(self.next_plan_generation).ok_or_else(exhausted)?;
        let next_generation = self
            .next_plan_generation
            .checked_add(1)
            .ok_or_else(exhausted)?;
        self.next_plan_generation = next_generation;
        let slot = &mut self.slots[at];
        slot.plan_generation = Some(generation);
        slot.open_site = None;
        slot.name = name;
        slot.statement = statement;
        slot.created_at = created_at;
        slot.scroll = scroll;
        slot.hold = hold;
        slot.binary = binary;
        slot.tentative = true;
        slot.failed = false;
        slot.query.clear();
        slot.description.clear();
        slot.reset_rows();
        slot.row_base = 0;
        slot.produced = 0;
        slot.request = None;
        slot.position = 0;
        Ok(at)
    }

    #[expect(
        clippy::too_many_arguments,
        reason = "cursor opening retains its explicit source identity, SQL options, and creation time"
    )]
    pub(crate) fn open_at(
        &mut self,
        site: CursorOpenSite,
        name: &str,
        statement: &str,
        scroll: CursorScroll,
        hold: bool,
        binary: bool,
        created_at: i64,
    ) -> Result<CursorOpenAttempt, SqlError> {
        if let Some((index, slot)) = self.slots.iter().enumerate().find(|(_, slot)| {
            !slot.active && slot.plan_generation.is_some() && slot.open_site == Some(site)
        }) {
            if slot.name.as_str() != name
                || slot.statement.as_str() != statement
                || slot.scroll != scroll
                || slot.hold != hold
                || slot.binary != binary
            {
                return Err(sql_err!(
                    sqlstate::SERIALIZATION_FAILURE,
                    "cursor opening definition changed while waiting"
                ));
            }
            // Resuming keeps the creation time of the first reservation.
            return Ok(CursorOpenAttempt {
                index,
                generation: slot.plan_generation.expect("selected opening generation"),
            });
        }
        let index = self.open(name, statement, scroll, hold, binary, created_at)?;
        let slot = &mut self.slots[index];
        slot.open_site = Some(site);
        Ok(CursorOpenAttempt {
            index,
            generation: slot.plan_generation.expect("fresh opening generation"),
        })
    }

    pub(crate) fn opening_query_bound(&self, at: usize) -> bool {
        let slot = &self.slots[at];
        !slot.active && slot.query.is_bound()
    }

    pub(crate) fn abandon_attempt(&mut self, attempt: CursorOpenAttempt) -> Result<(), SqlError> {
        if !self
            .slots
            .get(attempt.index)
            .is_some_and(|slot| slot.plan_generation == Some(attempt.generation))
        {
            return Err(sql_err!(
                sqlstate::OBJECT_NOT_IN_PREREQUISITE_STATE,
                "cursor opening source is no longer retained"
            ));
        }
        self.abandon(attempt.index);
        Ok(())
    }

    pub(crate) fn restart_opening_output(
        &mut self,
        attempt: CursorOpenAttempt,
    ) -> Result<(), SqlError> {
        let slot = self
            .slots
            .get_mut(attempt.index)
            .filter(|slot| !slot.active && slot.plan_generation == Some(attempt.generation))
            .ok_or_else(|| {
                sql_err!(
                    sqlstate::OBJECT_NOT_IN_PREREQUISITE_STATE,
                    "cursor opening source is no longer retained"
                )
            })?;
        slot.description.clear();
        slot.reset_rows();
        slot.row_base = 0;
        slot.produced = 0;
        Ok(())
    }

    pub(crate) fn plan_owner(&self, at: usize) -> Result<super::type_input_plans::Owner, SqlError> {
        self.slots
            .get(at)
            .filter(|slot| !slot.failed)
            .and_then(|slot| slot.plan_generation)
            .map(|generation| super::type_input_plans::Owner::Cursor(generation.get()))
            .ok_or_else(|| {
                sql_err!(
                    sqlstate::OBJECT_NOT_IN_PREREQUISITE_STATE,
                    "cursor plan source is no longer retained"
                )
            })
    }

    pub(crate) fn contains_plan_owner(&self, generation: u64) -> bool {
        self.slots.iter().any(|slot| {
            !slot.failed
                && slot
                    .plan_generation
                    .is_some_and(|current| current.get() == generation)
        })
    }

    /// Description and typed-row storage for the cursor producer.
    pub fn result_buffers(&mut self, at: usize) -> (&mut FixedBuf, &mut FixedBuf) {
        let slot = &mut self.slots[at];
        (&mut slot.description, &mut slot.rows)
    }

    #[expect(
        clippy::too_many_arguments,
        reason = "cursor binding retains SQL, values, local types and checked inputs together"
    )]
    pub(crate) fn bind_query(
        &mut self,
        at: usize,
        sql: &str,
        params: &[Datum],
        locals: &[CursorLocal],
        inputs: &[super::type_inputs::Binding<'_>],
        storage: &crate::storage::Storage,
        txid: u32,
    ) -> Result<(), SqlError> {
        let owner = self.plan_owner(at)?;
        self.slots[at]
            .query
            .bind(sql, params, locals, inputs, owner, |parser| {
                super::type_inputs::validate_restored(parser, storage, txid)
            })
    }

    pub(crate) fn execution_parts(
        &mut self,
        at: usize,
    ) -> (&mut CursorQuery, &mut FixedBuf, &mut FixedBuf) {
        let slot = &mut self.slots[at];
        (&mut slot.query, &mut slot.description, &mut slot.rows)
    }

    /// Validates the description and indexes complete typed rows.
    pub fn seal(&mut self, at: usize) -> Result<(), SqlError> {
        let slot = &mut self.slots[at];
        let columns = seal_description(&mut slot.description)?;
        seal_rows(&slot.rows, columns, &mut slot.spans)?;
        slot.produced = slot.spans.len();
        if slot.scroll == CursorScroll::Default
            && let Some(source) = slot.query.retained.as_ref()
        {
            slot.scroll = if source.default_scrollable() {
                CursorScroll::Scroll
            } else {
                CursorScroll::NoScroll
            };
        }
        if slot.scroll == CursorScroll::Scroll
            && let Some(source) = slot.query.retained.as_mut()
        {
            source.keep_finished_input();
        }
        slot.scan = slot
            .query
            .retained
            .as_ref()
            .map(|source| source.revisit())
            .filter(|revisit| {
                slot.scroll == CursorScroll::Scroll && *revisit != Revisit::SavedOutput
            })
            .map(ScrollScan::new);
        slot.active = true;
        Ok(())
    }

    pub(crate) fn read_request(
        &self,
        name: &str,
        motion: FetchMotion,
        move_only: bool,
    ) -> Result<Option<(usize, usize)>, SqlError> {
        let Some(at) = self.find(name) else {
            return Ok(None);
        };
        let slot = &self.slots[at];
        slot.validate_motion(motion, move_only, slot.query.retained.is_none())?;
        if slot.query.retained.is_none() {
            return Ok(None);
        }
        let needed = match motion {
            FetchMotion::All => usize::MAX,
            FetchMotion::Absolute(k) if k < 0 => usize::MAX,
            FetchMotion::Count(k) | FetchMotion::Relative(k) => {
                slot.position.saturating_add(i64::from(k)).max(0) as usize
            }
            FetchMotion::Absolute(k) => k as usize,
            FetchMotion::BackwardAll => 0,
            FetchMotion::BackwardZero => slot.position.max(0) as usize,
        };
        Ok(Some((at, needed)))
    }

    pub(crate) fn produced_rows(&self, at: usize) -> usize {
        self.slots[at].produced
    }

    pub(crate) fn saves_history(&self, at: usize) -> bool {
        self.slots[at].scroll == CursorScroll::Scroll && !self.windowed(at)
    }

    /// A scrollable cursor that keeps only its current fetch's rows: its
    /// output cannot change when evaluated again and its input can return to
    /// the first row, so backward motion rereads the input from the start
    /// (with the cursor's snapshot) instead of saving every visited row.
    pub(crate) fn windowed(&self, at: usize) -> bool {
        let slot = &self.slots[at];
        slot.scroll == CursorScroll::Scroll
            && slot.scan.is_none()
            && slot
                .query
                .retained
                .as_ref()
                .is_some_and(|query| query.rewindable())
    }

    /// The rows the current refill saves, while one runs.
    pub(crate) fn refill(&self, at: usize) -> Option<(usize, usize)> {
        self.slots[at].refill
    }

    /// The rows a windowed cursor's FETCH returns that its window does not
    /// hold, as `(first, last)`; `None` when the window covers them.
    pub(crate) fn window_miss(
        &self,
        at: usize,
        motion: FetchMotion,
        move_only: bool,
    ) -> Option<(usize, usize)> {
        let slot = &self.slots[at];
        if move_only || !self.windowed(at) {
            return None;
        }
        let n = slot.known_rows();
        let (mut lo, mut hi) = (i64::MAX, 0i64);
        motion_rows(slot.position, n, motion, |row| {
            if row >= 1 && row <= n {
                lo = lo.min(row);
                hi = hi.max(row);
            }
        });
        let first = slot.row_base as i64 + 1;
        let last = slot.row_base as i64 + slot.spans.len() as i64;
        (hi != 0 && (lo < first || hi > last)).then_some((lo as usize, hi as usize))
    }

    /// Rereads a windowed cursor's input from the first row, saving rows
    /// `lo..=hi`. The cursor keeps its position.
    pub(crate) fn restart_window(
        &mut self,
        at: usize,
        lo: usize,
        hi: usize,
    ) -> Result<(), SqlError> {
        let slot = &mut self.slots[at];
        slot.query.rewind_source()?;
        slot.rows.clear();
        slot.spans.clear();
        slot.produced = 0;
        slot.row_base = 0;
        slot.refill = Some((lo, hi));
        Ok(())
    }

    /// The named cursor's slot when it moves by [`ScrollScan`].
    pub(crate) fn scroll_slot(&self, name: &str) -> Option<usize> {
        self.find(name)
            .filter(|at| self.slots[*at].scan.is_some() && self.slots[*at].query.retained.is_some())
    }

    /// Plans a FETCH or MOVE, or resumes the one interrupted by a wait. Only
    /// the Limit node's saved row survives from earlier commands.
    pub(crate) fn begin_scroll(
        &mut self,
        at: usize,
        motion: FetchMotion,
        move_only: bool,
    ) -> Result<(), SqlError> {
        let slot = &mut self.slots[at];
        slot.require_runnable()?;
        if let Some(request) = slot.request {
            if request != (motion, move_only) {
                return Err(sql_err!(
                    sqlstate::OBJECT_NOT_IN_PREREQUISITE_STATE,
                    "cursor fetch changed while its input was waiting"
                ));
            }
            return Ok(());
        }
        let scan = slot.scan.as_mut().expect("scroll cursor plan");
        let kept = scan
            .limit_slot()
            .and_then(|index| slot.spans.get(index as usize).copied());
        slot.spans.clear();
        match kept {
            Some((offset, length)) => {
                // The row bytes keep their four-byte length prefix.
                let start = offset as usize - 4;
                slot.rows
                    .move_within(start..offset as usize + length as usize, 0);
                slot.rows.truncate_to(length as usize + 4);
                slot.spans.push((4, length))?;
            }
            None => slot.rows.clear(),
        }
        scan.relocate_limit_slot(kept.map(|_| 0));
        scan.begin(motion, move_only);
        self.emit.clear();
        slot.request = Some((motion, move_only));
        Ok(())
    }

    /// The next step of the current scroll command; emitted rows are added
    /// to [`Self::emitted`].
    pub(crate) fn scroll_next(&mut self, at: usize) -> Result<ScrollAction, SqlError> {
        let emit = &mut self.emit;
        let scan = self.slots[at].scan.as_mut().expect("scroll cursor plan");
        scan.next(&mut |index| push_emit(emit, index))
    }

    pub(crate) fn scroll_complete(
        &mut self,
        at: usize,
        outcome: VisitOutcome,
    ) -> Result<(), SqlError> {
        let emit = &mut self.emit;
        let scan = self.slots[at].scan.as_mut().expect("scroll cursor plan");
        scan.complete(outcome, &mut |index| push_emit(emit, index))
    }

    pub(crate) fn scroll_set_limits(&mut self, at: usize, limits: Option<(u64, u64)>) {
        self.slots[at]
            .scan
            .as_mut()
            .expect("scroll cursor plan")
            .set_limits(limits);
    }

    pub(crate) fn locks_rows(&self, at: usize) -> bool {
        self.slots[at]
            .query
            .source()
            .is_some_and(|source| source.locks_rows())
    }

    /// Whether every qualifying row still has to be saved and sorted.
    pub(crate) fn sort_pending(&self, at: usize) -> bool {
        let slot = &self.slots[at];
        slot.locking.sorted.is_none() && slot.query.source().is_some_and(|source| source.sorted())
    }

    pub(crate) fn sorted_source(&self, at: usize) -> bool {
        self.slots[at].locking.sorted.is_some()
    }

    /// Sorts the saved inputs; fetching then reads them in order.
    pub(crate) fn finish_sorting(
        &mut self,
        at: usize,
        storage: &crate::storage::Storage,
        txid: u32,
    ) -> Result<(), SqlError> {
        let slot = &mut self.slots[at];
        let source = slot
            .query
            .source()
            .ok_or_else(|| sql_err!(sqlstate::INTERNAL_ERROR, "cursor source is not retained"))?;
        let rows = &slot.rows;
        // SAFETY: the comparator only reads saved inputs while the index
        // entries move; no tail byte changes during the sort.
        let bytes = |offset: u32, length: u32| -> &'static [u8] {
            let bytes = rows.tail_bytes(offset as usize, length as usize);
            unsafe { core::slice::from_raw_parts(bytes.as_ptr(), bytes.len()) }
        };
        source.sort_saved_inputs(storage, txid, slot.spans.inputs_mut(), bytes)?;
        slot.locking.sorted = Some(0);
        Ok(())
    }

    /// Makes the next sorted input pending, unless a wait left a row pending.
    pub(crate) fn prepare_sorted_row(&mut self, at: usize) -> Result<bool, SqlError> {
        let slot = &self.slots[at];
        if slot
            .query
            .source()
            .is_some_and(|source| source.has_pending())
        {
            return Ok(true);
        }
        let next = slot.locking.sorted.expect("sorted inputs");
        if next >= slot.spans.inputs_len()
            || slot
                .query
                .source()
                .is_some_and(|source| source.limit_reached())
        {
            return Ok(false);
        }
        self.prepare_saved_input(at, next)?;
        Ok(true)
    }

    /// Moves an ordered cursor past the sorted input it just evaluated.
    pub(crate) fn finish_sorted_row(&mut self, at: usize) {
        if let Some(next) = self.slots[at].locking.sorted.as_mut() {
            *next += 1;
        }
    }

    /// Ends the pending row of a locking cursor.
    pub(crate) fn finish_locked_row(&mut self, at: usize) {
        let locking = &mut self.slots[at].locking;
        if let Some(next) = locking.sorted.as_mut() {
            *next += 1;
        }
        locking.output = None;
        locking.recheck = false;
    }

    pub(crate) fn locked_output(&self, at: usize) -> Option<usize> {
        self.slots[at].locking.output
    }

    pub(crate) fn output_mark(&self, at: usize) -> usize {
        self.slots[at].rows.mark()
    }

    pub(crate) fn set_locked_output(&mut self, at: usize, mark: usize) {
        self.slots[at].locking.output = Some(mark);
    }

    /// Drops the pending row's projected output.
    pub(crate) fn drop_locked_output(&mut self, at: usize) {
        let slot = &mut self.slots[at];
        if let Some(mark) = slot.locking.output.take() {
            slot.rows.truncate_to(mark);
        }
    }

    pub(crate) fn needs_recheck(&self, at: usize) -> bool {
        self.slots[at].locking.recheck
    }

    pub(crate) fn set_recheck(&mut self, at: usize) {
        self.slots[at].locking.recheck = true;
    }

    /// Whether rejected source rows are saved for a later filter.
    pub(crate) fn scroll_rescans(&self, at: usize) -> bool {
        self.slots[at]
            .scan
            .as_ref()
            .is_some_and(|scan| scan.revisit() == Revisit::Rescan)
    }

    /// Ends a completed scroll command.
    pub(crate) fn finish_scroll(&mut self, at: usize) {
        self.slots[at].request = None;
    }

    /// Before a kept output is written: the previous output survives only if
    /// it was emitted (an unemitted one is the replaced Limit row).
    pub(crate) fn release_unemitted_output(&mut self, at: usize) {
        let slot = &mut self.slots[at];
        let last = slot.spans.len().checked_sub(1);
        if let Some(last) = last
            && self.emit.last() != Some(&(last as u32))
        {
            let (offset, _) = *slot.spans.last().expect("last output");
            slot.rows.truncate_to(offset as usize - 4);
            slot.spans.truncate(last);
        }
    }

    /// Indexes the output row just written and returns its index.
    pub(crate) fn seal_output(&mut self, at: usize) -> Result<u32, SqlError> {
        let slot = &mut self.slots[at];
        let mut types = [0; crate::pg::respond::MAX_RESULT_COLS];
        let columns = description_type_oids(slot.description.readable(), &mut types)?;
        let before = slot.spans.len();
        seal_rows(&slot.rows, columns, &mut slot.spans)?;
        if slot.spans.len() != before + 1 {
            return Err(sql_err!(
                sqlstate::INTERNAL_ERROR,
                "cursor output step did not save exactly one row"
            ));
        }
        Ok(before as u32)
    }

    /// Saves the pending source row as the next saved input.
    pub(crate) fn save_pending_input(&mut self, at: usize) -> Result<(), SqlError> {
        let slot = &mut self.slots[at];
        let source =
            slot.query.retained.as_ref().ok_or_else(|| {
                sql_err!(sqlstate::INTERNAL_ERROR, "cursor source is not retained")
            })?;
        let length = source.pending_input_len()?;
        let (offset, bytes) = slot.rows.push_tail(length).ok_or_else(history_full)?;
        source.encode_pending_input(bytes)?;
        slot.spans.push_input((offset as u32, length as u32))
    }

    /// Makes saved input `index` the cursor source's pending row, unless a
    /// wait left it pending already.
    pub(crate) fn prepare_saved_input(&mut self, at: usize, index: usize) -> Result<(), SqlError> {
        let slot = &mut self.slots[at];
        let (offset, length) = slot.spans.input(index).ok_or_else(|| {
            sql_err!(
                sqlstate::INTERNAL_ERROR,
                "cursor saved input index is invalid"
            )
        })?;
        let bytes = slot.rows.tail_bytes(offset as usize, length as usize);
        slot.query.prepare_saved_input(index, bytes)
    }

    #[cfg(test)]
    pub(crate) fn buffered_rows(&self, at: usize) -> usize {
        self.slots[at].spans.len()
    }

    /// Starts a FETCH that may produce rows up to `needed`.
    pub(crate) fn begin_fetch(
        &mut self,
        at: usize,
        motion: FetchMotion,
        move_only: bool,
        needed: usize,
    ) -> Result<(), SqlError> {
        let windowed = self.windowed(at);
        let slot = &mut self.slots[at];
        if let Some(request) = slot.request {
            if request != (motion, move_only) {
                return Err(sql_err!(
                    sqlstate::OBJECT_NOT_IN_PREREQUISITE_STATE,
                    "cursor fetch changed while its input was waiting"
                ));
            }
            return Ok(());
        }
        // A windowed cursor replaces its window when the fetch reads input.
        if slot.scroll == CursorScroll::NoScroll || (windowed && needed > slot.produced) {
            slot.rows.clear();
            slot.spans.clear();
            slot.row_base = slot.produced;
        }
        slot.request = Some((motion, move_only));
        Ok(())
    }

    /// The slot of a cursor whose forward FETCH can stream its rows to the
    /// client as they are evaluated: a retained source that saves neither
    /// history nor locks, with no interrupted FETCH. Other cursors buffer a
    /// FETCH's rows in `cursor_bytes`.
    pub(crate) fn stream_fetch_slot(
        &self,
        name: &str,
        motion: FetchMotion,
        move_only: bool,
    ) -> Option<usize> {
        let at = self.find(name)?;
        let slot = &self.slots[at];
        let forward = matches!(motion, FetchMotion::All)
            || matches!(motion, FetchMotion::Count(count) if count > 0);
        (!move_only
            && forward
            && !slot.failed
            && (slot.scroll != CursorScroll::Scroll
                || (self.windowed(at) && slot.position >= slot.produced as i64))
            && slot.scan.is_none()
            && slot.request.is_none()
            && slot
                .query
                .retained
                .as_ref()
                .is_some_and(|query| !query.locks_rows()))
        .then_some(at)
    }

    /// Starts a streamed FETCH: no row of it is saved.
    pub(crate) fn begin_streamed_fetch(&mut self, at: usize) {
        let slot = &mut self.slots[at];
        slot.rows.clear();
        slot.spans.clear();
        slot.row_base = slot.produced;
    }

    /// A streamed row reached the client: the cursor is now on it.
    pub(crate) fn record_streamed_row(&mut self, at: usize) -> Result<(), SqlError> {
        self.record_row(at, false)?;
        let slot = &mut self.slots[at];
        slot.position = slot.produced as i64;
        Ok(())
    }

    /// A streamed FETCH reached the end of its cursor's rows: the cursor is
    /// after the last row.
    pub(crate) fn finish_streamed_fetch(&mut self, at: usize) {
        let slot = &mut self.slots[at];
        slot.input_rows = Some(slot.produced);
        slot.position = slot.produced as i64 + 1;
    }

    /// The cursor's input has no more rows.
    pub(crate) fn finish_input(&mut self, at: usize) {
        let slot = &mut self.slots[at];
        slot.input_rows = Some(slot.produced);
    }

    pub(crate) fn slot_name(&self, at: usize) -> &str {
        self.slots[at].name.as_str()
    }

    pub(crate) fn record_row(&mut self, at: usize, saved: bool) -> Result<(), SqlError> {
        let slot = &mut self.slots[at];
        if saved {
            let mut types = [0; crate::pg::respond::MAX_RESULT_COLS];
            let columns = description_type_oids(slot.description.readable(), &mut types)?;
            seal_rows(&slot.rows, columns, &mut slot.spans)?;
        } else if !slot.spans.is_empty() {
            return Err(sql_err!(
                sqlstate::INTERNAL_ERROR,
                "cursor skipped input after saving its fetch result"
            ));
        }
        slot.produced = slot
            .produced
            .checked_add(1)
            .filter(|count| i64::try_from(*count).is_ok_and(|count| count < i64::MAX))
            .ok_or_else(|| {
                sql_err!(
                    sqlstate::PROGRAM_LIMIT_EXCEEDED,
                    "cursor row position exhausted"
                )
            })?;
        if !saved {
            slot.row_base = slot.produced;
        }
        Ok(())
    }

    pub(crate) fn fail(&mut self, name: &str) {
        if let Some(at) = self.find(name) {
            let slot = &mut self.slots[at];
            if slot.failed {
                return;
            }
            slot.failed = true;
            slot.query.clear();
            slot.request = None;
            if let Some(scan) = slot.scan.as_mut() {
                scan.abandon();
            }
            // The plan is released, but rollback still needs this cursor's
            // creation identity to remove names opened after its savepoint.
            slot.open_site = None;
        }
    }

    /// Drops a half-built slot after a failed DECLARE.
    pub fn abandon(&mut self, at: usize) {
        self.slots[at].query.clear();
        self.slots[at].active = false;
        self.slots[at].tentative = false;
        self.slots[at].plan_generation = None;
        self.slots[at].open_site = None;
    }

    pub fn exists(&self, name: &str) -> bool {
        self.find(name).is_some()
    }

    pub(crate) fn visit(&self, mut emit: impl FnMut(CursorInfo<'_>)) {
        for slot in &self.slots {
            if !slot.active {
                continue;
            }
            emit(CursorInfo {
                name: slot.name.as_str(),
                statement: slot.statement.as_str(),
                holdable: slot.hold,
                binary: slot.binary,
                scrollable: slot.scroll == CursorScroll::Scroll,
                created_at: slot.created_at,
            });
        }
    }

    pub(crate) fn len(&self) -> usize {
        self.slots.iter().filter(|slot| slot.active).count()
    }

    pub(crate) fn unnamed_name(&mut self) -> Result<StackStr<128>, SqlError> {
        for _ in 0..=self.slots.len() {
            let name = crate::stack_format!(128, "<unnamed portal {}>", self.next_unnamed);
            self.next_unnamed = self.next_unnamed.checked_add(1).ok_or_else(|| {
                sql_err!(
                    sqlstate::PROGRAM_LIMIT_EXCEEDED,
                    "cursor name sequence exhausted"
                )
            })?;
            if !self.exists(name.as_str()) {
                return Ok(name);
            }
        }
        Err(sql_err!(
            sqlstate::PROGRAM_LIMIT_EXCEEDED,
            "too many open cursors (limit {})",
            self.slots.len()
        ))
    }

    /// The declaration-time RowDescription of a live SQL cursor. PostgreSQL
    /// exposes an SQL cursor as a describable protocol portal, so drivers can
    /// install result decoders before their first FETCH.
    pub fn description(&self, name: &str) -> Option<&[u8]> {
        let at = self.find(name)?;
        let slot = &self.slots[at];
        Some(slot.description.readable())
    }

    pub(crate) fn record_columns<'a>(
        &'a self,
        name: &str,
        names: &mut [Option<&'a str>],
        type_oids: &mut [i32],
    ) -> Result<usize, SqlError> {
        let at = self.find(name).ok_or_else(|| {
            sql_err!(
                sqlstate::UNDEFINED_CURSOR,
                "cursor \"{}\" does not exist",
                name
            )
        })?;
        let slot = &self.slots[at];
        let count = description_columns(slot.description.readable(), names, type_oids)?;
        let declared = slot.query.result_type_oids()?.ok_or_else(|| {
            sql_err!(
                sqlstate::INTERNAL_ERROR,
                "cursor SQL result types are missing"
            )
        })?;
        if declared.len() != count {
            return Err(sql_err!(
                sqlstate::INTERNAL_ERROR,
                "cursor SQL result shape changed"
            ));
        }
        type_oids[..count].copy_from_slice(declared);
        Ok(count)
    }

    pub fn fetch_description(
        &self,
        name: &str,
        requested: ResultFmt,
    ) -> Option<(&[u8], ResultFmt)> {
        let at = self.find(name)?;
        let slot = &self.slots[at];
        let formats = if requested.count() == 0 && slot.binary {
            ResultFmt::ALL_BINARY
        } else {
            requested
        };
        Some((slot.description.readable(), formats))
    }

    /// Applies one FETCH/MOVE motion: returns the spans of the rows to emit
    /// (in emission order) plus the description bytes, updating the cursor's
    /// position. Backward motion on a non-SCROLL cursor is PostgreSQL's
    /// 55000.
    pub fn fetch(
        &mut self,
        name: &str,
        motion: FetchMotion,
        move_only: bool,
    ) -> Result<usize, SqlError> {
        let Some(at) = self.find(name) else {
            return Err(sql_err!(
                sqlstate::UNDEFINED_CURSOR,
                "cursor \"{}\" does not exist",
                name
            ));
        };
        let slot = &mut self.slots[at];
        slot.validate_motion(motion, move_only, true)?;
        let n = slot.known_rows();
        self.emit.clear();
        let out_rows = &mut self.emit;
        let row_base = slot.row_base;
        let buffered = slot.spans.len();
        let mut count = 0usize;
        let mut failed = None;
        slot.position = motion_rows(slot.position, n, motion, |row| {
            if failed.is_some() || row < 1 || row > n {
                return;
            }
            count += 1;
            if move_only {
                return;
            }
            match (row as usize)
                .checked_sub(row_base + 1)
                .filter(|local| *local < buffered)
            {
                Some(local) => out_rows.push(local as u32),
                None => {
                    failed = Some(sql_err!(
                        sqlstate::INTERNAL_ERROR,
                        "cursor result is outside its retained fetch"
                    ))
                }
            }
        });
        slot.refill = None;
        if let Some(error) = failed {
            return Err(error);
        }
        slot.request = None;
        Ok(count)
    }

    /// The row indexes selected by the last [`Self::fetch`].
    pub fn emitted(&self) -> &[u32] {
        &self.emit
    }

    /// Copies typed values into the caller's arena so they outlive pool access.
    pub(crate) fn copy_row<'arena>(
        &self,
        name: &str,
        row: u32,
        arena: &'arena Arena,
        values: &mut [Datum<'arena>],
    ) -> Result<usize, SqlError> {
        let at = self.find(name).ok_or_else(|| {
            sql_err!(
                sqlstate::UNDEFINED_CURSOR,
                "cursor \"{}\" does not exist",
                name
            )
        })?;
        let slot = &self.slots[at];
        let (offset, length) = *slot
            .spans
            .get(row as usize)
            .ok_or_else(|| sql_err!(sqlstate::INTERNAL_ERROR, "cursor row index is invalid"))?;
        let bytes = &slot.rows.readable()[offset as usize..offset as usize + length as usize];
        let width = super::exec::projected_row_width(bytes);
        if width > values.len() {
            return Err(sql_err!(
                sqlstate::PROGRAM_LIMIT_EXCEEDED,
                "cursor row has more than {} columns",
                values.len()
            ));
        }
        let bytes = arena
            .alloc_slice_copy(bytes)
            .map_err(|_| super::query::arena_full_pub())?;
        super::exec::decode_projected_row_into(bytes, arena, values)
    }

    /// CLOSE name — false when no such cursor exists.
    pub fn close(&mut self, name: &str) -> bool {
        match self.find(name) {
            Some(at) => {
                self.slots[at].query.clear();
                self.slots[at].active = false;
                self.slots[at].tentative = false;
                self.slots[at].plan_generation = None;
                self.slots[at].open_site = None;
                true
            }
            None => false,
        }
    }

    /// CLOSE ALL.
    pub fn close_all(&mut self) {
        for s in &mut self.slots {
            s.query.clear();
            s.active = false;
            s.tentative = false;
            s.plan_generation = None;
            s.open_site = None;
        }
    }

    pub(crate) fn next_hold_source(&self) -> Option<usize> {
        self.slots.iter().position(|slot| {
            slot.active && slot.tentative && slot.hold && slot.query.retained.is_some()
        })
    }

    pub(crate) fn finish_hold_source(&mut self, at: usize) {
        let slot = &mut self.slots[at];
        slot.query.finish_source();
        slot.request = None;
        slot.refill = None;
        slot.holding = false;
        slot.rows.clear_tail();
        slot.spans.clear_inputs();
        if let Some(scan) = slot.scan.take() {
            // The stored rows now hold the whole result in PostgreSQL's
            // portal position.
            let (position, at_start, at_end) = scan.portal();
            let rows = slot.spans.len() as i64;
            slot.produced = slot.spans.len();
            slot.row_base = 0;
            slot.position = if at_start {
                0
            } else if at_end {
                (position + 1).min(rows + 1)
            } else {
                position.min(rows + 1)
            };
        }
    }

    /// Starts saving a held scroll cursor: every row from the start.
    pub(crate) fn begin_scroll_hold(&mut self, at: usize) -> bool {
        let slot = &mut self.slots[at];
        let Some(scan) = slot.scan.as_mut() else {
            return false;
        };
        if slot.holding {
            // A wait interrupted the save; it resumes where it stopped.
            return true;
        }
        slot.holding = true;
        slot.rows.clear();
        slot.spans.clear();
        scan.relocate_limit_slot(None);
        scan.begin_hold();
        self.emit.clear();
        true
    }

    /// After saving a held scroll cursor, drops a trailing output the Limit
    /// node pulled but did not return.
    pub(crate) fn finish_scroll_hold(&mut self, at: usize) {
        self.release_unemitted_output(at);
    }

    pub(crate) fn has_uncommitted_hold_cursor(&self) -> bool {
        self.slots
            .iter()
            .any(|slot| slot.active && slot.tentative && slot.hold)
    }

    /// Transaction commit: cursors become holdable or die.
    pub fn on_commit(&mut self) {
        for s in &mut self.slots {
            if s.active || s.plan_generation.is_some() {
                if s.active && s.hold {
                    s.tentative = false;
                } else {
                    s.query.clear();
                    s.active = false;
                    s.plan_generation = None;
                    s.open_site = None;
                }
            }
        }
    }

    /// Transaction rollback: everything created in the transaction dies,
    /// WITH HOLD included; an already-held cursor survives.
    pub fn on_rollback(&mut self) {
        for s in &mut self.slots {
            if (s.active || s.plan_generation.is_some()) && (s.tentative || !s.hold) {
                s.query.clear();
                s.active = false;
                s.plan_generation = None;
                s.open_site = None;
            }
        }
    }
}

#[cfg(test)]
mod ownership_tests {
    use super::*;

    #[test]
    fn savepoint_creation_marks_release_failed_and_unfinished_cursors_without_reusing_owners() {
        let mut config = Config::default_dev();
        config.max_cursors = 3;
        config.cursor_bytes = 64;
        config.cursor_query_bytes = 1024;
        let mut budget = Budget::new(CursorPool::budget_bytes(&config));
        let mut pool = CursorPool::new(&config, &mut budget).unwrap();
        crate::mem::guard::forbid_alloc(|| {
            let outer = pool.creation_mark();
            let old = pool
                .open("old", "SELECT 1", CursorScroll::Default, false, false, 1)
                .unwrap();
            let owner = pool.plan_owner(old).unwrap();
            let (description, _) = pool.result_buffers(old);
            crate::pg::respond::Responder::new(description)
                .row_description(&[])
                .unwrap();
            pool.seal(old).unwrap();
            let inner = pool.creation_mark();
            let failed = pool
                .open("failed", "SELECT 2", CursorScroll::Default, false, false, 1)
                .unwrap();
            let (description, _) = pool.result_buffers(failed);
            crate::pg::respond::Responder::new(description)
                .row_description(&[])
                .unwrap();
            pool.seal(failed).unwrap();
            pool.fail("failed");
            let waiting = pool
                .open(
                    "waiting",
                    "SELECT 3",
                    CursorScroll::Default,
                    false,
                    false,
                    1,
                )
                .unwrap();
            pool.rollback_creations(inner);
            assert!(!pool.exists("failed"));
            assert!(pool.plan_owner(waiting).is_err());
            assert_eq!(pool.plan_owner(old).unwrap(), owner);
            assert!(pool.exists("old"));
            pool.rollback_creations(inner);
            let replacement = pool
                .open("failed", "SELECT 4", CursorScroll::Default, false, false, 1)
                .unwrap();
            assert_ne!(pool.plan_owner(replacement).unwrap(), owner);
            pool.rollback_creations(outer);
            assert!(!pool.exists("old"));
            assert!(pool.plan_owner(replacement).is_err());
        });
    }

    #[test]
    fn cursor_open_attempts_resume_only_their_qualified_source_and_keep_bound_queries() {
        let mut config = Config::default_dev();
        config.max_cursors = 1;
        config.cursor_bytes = 64;
        config.cursor_query_bytes = 16384;
        let mut budget = Budget::new(CursorPool::budget_bytes(&config));
        let mut pool = CursorPool::new(&config, &mut budget).unwrap();
        let site = CursorOpenSite {
            owner: super::super::type_input_plans::Owner::Transient {
                generation: 7,
                statement: 0,
            },
            source: super::super::type_inputs::Source::RootCommand { statement: 0 },
            position: 31,
        };
        crate::mem::guard::forbid_alloc(|| {
            let attempt = pool
                .open_at(
                    site,
                    "resume",
                    "SELECT 1",
                    CursorScroll::Default,
                    false,
                    false,
                    100,
                )
                .unwrap();
            let at = attempt.index();
            assert_eq!(pool.slots[at].created_at, 100);
            let owner = pool.plan_owner(at).unwrap();
            assert!(!pool.opening_query_bound(at));
            pool.slots[at]
                .query
                .bind("SELECT 1", &[], &[], &[], owner, |_| Ok(()))
                .unwrap();
            assert!(pool.opening_query_bound(at));
            let retained = pool.slots[at]
                .query
                .with_query(|_, _, arena| arena.used())
                .unwrap();
            let resumed = pool
                .open_at(
                    site,
                    "resume",
                    "SELECT 1",
                    CursorScroll::Default,
                    false,
                    false,
                    200,
                )
                .unwrap();
            assert_eq!(resumed, attempt);
            assert_eq!(pool.slots[at].created_at, 100);
            assert_eq!(pool.plan_owner(at).unwrap(), owner);
            assert!(pool.opening_query_bound(at));
            assert_eq!(
                pool.slots[at]
                    .query
                    .with_query(|_, _, arena| arena.used())
                    .unwrap(),
                retained
            );
            assert_eq!(
                pool.open_at(
                    site,
                    "resume",
                    "SELECT 2",
                    CursorScroll::Default,
                    false,
                    false,
                    300,
                )
                .unwrap_err()
                .sqlstate,
                sqlstate::SERIALIZATION_FAILURE
            );
            let other_site = CursorOpenSite {
                position: 32,
                ..site
            };
            assert_eq!(
                pool.open_at(
                    other_site,
                    "resume",
                    "SELECT 1",
                    CursorScroll::Default,
                    false,
                    false,
                    400,
                )
                .unwrap_err()
                .sqlstate,
                sqlstate::DUPLICATE_CURSOR
            );
            pool.abandon_attempt(attempt).unwrap();
            let replacement = pool
                .open_at(
                    site,
                    "resume",
                    "SELECT 1",
                    CursorScroll::Default,
                    false,
                    false,
                    500,
                )
                .unwrap();
            assert_ne!(replacement, attempt);
            assert_eq!(pool.slots[replacement.index()].created_at, 500);
            assert_eq!(
                pool.abandon_attempt(attempt).unwrap_err().sqlstate,
                sqlstate::OBJECT_NOT_IN_PREREQUISITE_STATE
            );
            assert_eq!(
                pool.plan_owner(replacement.index()).unwrap(),
                super::super::type_input_plans::Owner::Cursor(replacement.generation.get())
            );
            pool.abandon_attempt(replacement).unwrap();
        });
    }

    #[test]
    fn cursor_plan_owners_cover_opening_and_held_frames_without_slot_reuse() {
        let mut config = Config::default_dev();
        config.max_cursors = 1;
        config.cursor_bytes = 64;
        config.cursor_query_bytes = 1024;
        let mut budget = Budget::new(CursorPool::budget_bytes(&config));
        let mut pool = CursorPool::new(&config, &mut budget).unwrap();
        let generation = |owner| match owner {
            super::super::type_input_plans::Owner::Cursor(generation) => generation,
            _ => panic!("cursor owner"),
        };
        crate::mem::guard::forbid_alloc(|| {
            let at = pool
                .open("same", "SELECT 1", CursorScroll::Default, false, false, 1)
                .unwrap();
            let first = generation(pool.plan_owner(at).unwrap());
            assert!(pool.contains_plan_owner(first));
            assert_eq!(
                pool.open("same", "SELECT 2", CursorScroll::Default, false, false, 1)
                    .unwrap_err()
                    .sqlstate,
                sqlstate::DUPLICATE_CURSOR
            );
            assert_eq!(
                pool.open("other", "SELECT 2", CursorScroll::Default, false, false, 1)
                    .unwrap_err()
                    .sqlstate,
                sqlstate::PROGRAM_LIMIT_EXCEEDED
            );
            pool.abandon(at);
            assert!(!pool.contains_plan_owner(first));
            assert!(pool.plan_owner(at).is_err());
            let at = pool
                .open("same", "SELECT 2", CursorScroll::Default, true, false, 1)
                .unwrap();
            let held = generation(pool.plan_owner(at).unwrap());
            assert_ne!(held, first);
            let (description, _) = pool.result_buffers(at);
            crate::pg::respond::Responder::new(description)
                .row_description(&[])
                .unwrap();
            pool.seal(at).unwrap();
            pool.on_commit();
            pool.on_rollback();
            assert!(pool.contains_plan_owner(held));
            assert!(pool.close("same"));
            assert!(!pool.contains_plan_owner(held));
            let at = pool
                .open("same", "SELECT 3", CursorScroll::Default, false, false, 1)
                .unwrap();
            let opening = generation(pool.plan_owner(at).unwrap());
            pool.on_rollback();
            assert!(!pool.contains_plan_owner(opening));
            let at = pool
                .open("same", "SELECT 4", CursorScroll::Default, false, false, 1)
                .unwrap();
            let before_clear = generation(pool.plan_owner(at).unwrap());
            pool.clear();
            let at = pool
                .open("same", "SELECT 5", CursorScroll::Default, false, false, 1)
                .unwrap();
            assert_ne!(generation(pool.plan_owner(at).unwrap()), before_clear);
            pool.on_commit();
            assert!(!pool.contains_plan_owner(before_clear));
            assert!(pool.plan_owner(at).is_err());
        });
    }

    #[test]
    fn terminal_cursor_failure_releases_plan_owner_and_preserves_failed_name() {
        let mut config = Config::default_dev();
        config.max_cursors = 1;
        config.cursor_bytes = 64;
        config.cursor_query_bytes = 1024;
        let mut budget = Budget::new(CursorPool::budget_bytes(&config));
        let mut pool = CursorPool::new(&config, &mut budget).unwrap();
        crate::mem::guard::forbid_alloc(|| {
            let at = pool
                .open("failed", "SELECT 1", CursorScroll::Default, false, false, 1)
                .unwrap();
            let super::super::type_input_plans::Owner::Cursor(generation) =
                pool.plan_owner(at).unwrap()
            else {
                panic!("cursor owner");
            };
            let (description, _) = pool.result_buffers(at);
            crate::pg::respond::Responder::new(description)
                .row_description(&[])
                .unwrap();
            pool.seal(at).unwrap();
            pool.fail("failed");
            assert!(!pool.contains_plan_owner(generation));
            assert!(pool.plan_owner(at).is_err());
            assert!(pool.exists("failed"));
            assert_eq!(
                pool.fetch("failed", FetchMotion::Count(1), false)
                    .unwrap_err()
                    .sqlstate,
                sqlstate::OBJECT_NOT_IN_PREREQUISITE_STATE
            );
            assert_eq!(
                pool.open("failed", "SELECT 2", CursorScroll::Default, false, false, 1)
                    .unwrap_err()
                    .sqlstate,
                sqlstate::DUPLICATE_CURSOR
            );
            pool.fail("failed");
            assert!(pool.close("failed"));
            assert!(!pool.exists("failed"));
        });
    }

    #[test]
    fn cursor_plan_identity_exhaustion_leaves_its_slot_unreserved() {
        let mut config = Config::default_dev();
        config.max_cursors = 1;
        config.cursor_bytes = 0;
        config.cursor_query_bytes = 1024;
        let mut budget = Budget::new(CursorPool::budget_bytes(&config));
        let mut pool = CursorPool::new(&config, &mut budget).unwrap();
        pool.next_plan_generation = u64::MAX;
        crate::mem::guard::forbid_alloc(|| {
            let error = pool
                .open(
                    "exhausted",
                    "SELECT 1",
                    CursorScroll::Default,
                    false,
                    false,
                    1,
                )
                .unwrap_err();
            assert_eq!(error.sqlstate, sqlstate::PROGRAM_LIMIT_EXCEEDED);
            assert_eq!(error.message.as_str(), "cursor plan identities exhausted");
            assert!(pool.plan_owner(0).is_err());
            assert!(!pool.exists("exhausted"));
            assert_eq!(pool.slots[0].name, SqlName::EMPTY);
            assert_eq!(pool.next_plan_generation, u64::MAX);
        });
    }
}

#[cfg(all(test, not(all(miri, feature = "miri-ownership"))))]
mod tests {
    use super::*;

    #[test]
    fn configured_cursor_budget_saturates_before_wrapping() {
        let mut config = Config::default_dev();
        config.max_cursors = usize::MAX;
        assert_eq!(CursorPool::budget_bytes(&config), usize::MAX);
        config.max_cursors = 1;
        config.cursor_bytes = usize::MAX;
        assert_eq!(CursorPool::budget_bytes(&config), usize::MAX);
        config.cursor_bytes = 0;
        config.cursor_query_bytes = usize::MAX;
        assert_eq!(CursorPool::budget_bytes(&config), usize::MAX);
    }

    #[test]
    fn startup_budget_covers_cursor_controls_spans_and_emission() {
        for max_cursors in [0, 1, 3] {
            for cursor_bytes in [0, 6, 7, 31] {
                let mut config = Config::default_dev();
                config.max_cursors = max_cursors;
                config.cursor_bytes = cursor_bytes;
                let bytes = CursorPool::budget_bytes(&config);
                let mut budget = Budget::new(bytes);
                let pool = CursorPool::new(&config, &mut budget).expect("exact cursor budget");
                assert_eq!(budget.remaining(), 0);
                assert_eq!(pool.slots.len(), max_cursors);
                assert_eq!(pool.emit.capacity(), cursor_row_capacity(&config));
                for slot in &pool.slots {
                    assert_eq!(slot.spans.capacity(), cursor_row_capacity(&config));
                    assert!(slot.spans.is_empty());
                }
                if cursor_row_capacity(&config) != 0 {
                    let mut short = Budget::new(bytes - 1);
                    let error = match CursorPool::new(&config, &mut short) {
                        Ok(_) => panic!("cursor pool accepted an incomplete reservation"),
                        Err(error) => error,
                    };
                    assert_eq!(error.what, "result_stream_tables");
                    assert_eq!(error.requested, error.remaining + 1);
                }
            }
        }
    }
}
