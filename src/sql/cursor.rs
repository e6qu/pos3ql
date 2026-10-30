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
pub(crate) use query::{CursorLocal, CursorQuery, CursorStatement};

/// One FETCH/MOVE motion, normalized by the parser: positive counts move
/// forward, negative backward (`NEXT` is `Count(1)`, `PRIOR` is `Count(-1)`,
/// `FIRST` is `Absolute(1)`, `LAST` is `Absolute(-1)`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FetchMotion {
    Count(i32),
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

pub struct CursorPool {
    slots: Vec<CursorSlot>,
    /// Row indexes selected by the last [`Self::fetch`], in emission order.
    emit: Vec<u32>,
    next_unnamed: u64,
}

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
            })
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
    rows: FixedBuf,
    spans: Vec<(u32, u32)>,
    /// PostgreSQL's cursor position: 0 before the first row, `1..=n` on a
    /// row, `n + 1` after the last.
    position: i64,
}

impl CursorSlot {
    fn validate_motion(&self, motion: FetchMotion, move_only: bool) -> Result<(), SqlError> {
        if self.failed {
            return Err(sql_err!(
                sqlstate::OBJECT_NOT_IN_PREREQUISITE_STATE,
                "portal \"{}\" cannot be run",
                self.name.as_str()
            ));
        }
        let on_row = self.position >= 1 && self.position <= self.spans.len() as i64;
        let backward = matches!(motion, FetchMotion::Count(k) | FetchMotion::Relative(k) if k < 0)
            || matches!(motion, FetchMotion::BackwardAll)
            || matches!(motion, FetchMotion::Absolute(k) if k < 0 || (i64::from(k) <= self.position && self.position != 0))
            || (!move_only
                && on_row
                && matches!(motion, FetchMotion::Count(0) | FetchMotion::Relative(0)));
        if self.scroll == CursorScroll::NoScroll && backward {
            return Err(sql_err!(
                sqlstate::OBJECT_NOT_IN_PREREQUISITE_STATE,
                "cursor can only scan forward"
            ));
        }
        Ok(())
    }
}

// Each saved row has a four-byte length and a two-byte column count.
const MIN_CURSOR_ROW_BYTES: usize = 6;

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

fn seal_rows(rows: &FixedBuf, columns: usize, spans: &mut Vec<(u32, u32)>) -> Result<(), SqlError> {
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
        if spans.len() == spans.capacity() {
            return Err(sql_err!(
                sqlstate::PROGRAM_LIMIT_EXCEEDED,
                "cursor row index exceeds cursor_bytes-derived capacity of {} rows",
                spans.capacity()
            ));
        }
        spans.push((start as u32, length as u32));
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

/// Extracts names and type OIDs from one captured RowDescription for typed
/// PL/pgSQL record assignment at the cursor-pool boundary.
pub(crate) fn description_columns<'a>(
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

impl CursorPool {
    pub fn budget_bytes(config: &Config) -> usize {
        let row_capacity = cursor_row_capacity(config);
        config.max_cursors
            * (config.cursor_bytes
                + config.cursor_query_bytes
                + DESCRIPTION_BYTES
                + core::mem::size_of::<CursorSlot>()
                + row_capacity * core::mem::size_of::<(u32, u32)>())
            + row_capacity * core::mem::size_of::<u32>()
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
                spans: Vec::with_capacity(row_capacity),
                position: 0,
            });
        }
        budget.draw_array(row_capacity, core::mem::size_of::<u32>(), "cursor_emit")?;
        Ok(Self {
            slots,
            emit: Vec::with_capacity(row_capacity),
            next_unnamed: 1,
        })
    }

    /// Closes every cursor — used when a connection slot is recycled for a new
    /// client so no cursor leaks across sessions.
    pub fn clear(&mut self) {
        for s in &mut self.slots {
            s.query.clear();
            s.active = false;
        }
        self.emit.clear();
        self.next_unnamed = 1;
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
    ) -> Result<usize, SqlError> {
        if self.find(name).is_some() {
            return Err(sql_err!(
                sqlstate::DUPLICATE_CURSOR,
                "cursor \"{}\" already exists",
                name
            ));
        }
        let Some(at) = self.slots.iter().position(|s| !s.active) else {
            return Err(sql_err!(
                sqlstate::PROGRAM_LIMIT_EXCEEDED,
                "too many open cursors (limit {})",
                self.slots.len()
            ));
        };
        let slot = &mut self.slots[at];
        slot.name = SqlName::parse(name)?;
        slot.statement = StackStr::from_str(statement);
        if slot.statement.is_truncated() {
            return Err(sql_err!(
                sqlstate::PROGRAM_LIMIT_EXCEEDED,
                "cursor statement exceeds {} bytes",
                CURSOR_STATEMENT_BYTES
            ));
        }
        slot.created_at = crate::sql::datetime::now_micros();
        slot.scroll = scroll;
        slot.hold = hold;
        slot.binary = binary;
        slot.tentative = true;
        slot.failed = false;
        slot.query.clear();
        slot.description.clear();
        slot.rows.clear();
        slot.spans.clear();
        slot.position = 0;
        Ok(at)
    }

    /// Description and typed-row storage for the cursor producer.
    pub fn result_buffers(&mut self, at: usize) -> (&mut FixedBuf, &mut FixedBuf) {
        let slot = &mut self.slots[at];
        (&mut slot.description, &mut slot.rows)
    }

    pub(crate) fn bind_query(
        &mut self,
        at: usize,
        sql: &str,
        params: &[Datum],
        locals: &[CursorLocal],
    ) -> Result<(), SqlError> {
        self.slots[at].query.bind(sql, params, locals)
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
        slot.validate_motion(motion, move_only)?;
        if slot.query.retained.is_none() {
            return Ok(None);
        }
        let needed = match motion {
            FetchMotion::All => usize::MAX,
            FetchMotion::Count(k) | FetchMotion::Relative(k) => {
                slot.position.saturating_add(i64::from(k)) as usize
            }
            FetchMotion::Absolute(k) => k as usize,
            FetchMotion::BackwardAll => unreachable!("backward motion was rejected"),
        };
        Ok(Some((at, needed)))
    }

    pub(crate) fn cached_rows(&self, at: usize) -> usize {
        self.slots[at].spans.len()
    }

    pub(crate) fn index_new_rows(&mut self, at: usize) -> Result<(), SqlError> {
        let slot = &mut self.slots[at];
        let mut types = [0; crate::pg::respond::MAX_RESULT_COLS];
        let columns = description_type_oids(slot.description.readable(), &mut types)?;
        seal_rows(&slot.rows, columns, &mut slot.spans)
    }

    pub(crate) fn fail(&mut self, name: &str) {
        if let Some(at) = self.find(name) {
            let slot = &mut self.slots[at];
            if slot.failed {
                return;
            }
            slot.failed = true;
            slot.query.clear();
        }
    }

    /// Drops a half-built slot after a failed DECLARE.
    pub fn abandon(&mut self, at: usize) {
        self.slots[at].query.clear();
        self.slots[at].active = false;
        self.slots[at].tentative = false;
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
        slot.validate_motion(motion, move_only)?;
        let n = slot.spans.len() as i64;
        self.emit.clear();
        let out_rows = &mut self.emit;
        let push_row = |row: i64, out: &mut Vec<u32>| {
            if row >= 1 && row <= n {
                out.push((row - 1) as u32);
            }
        };
        match motion {
            FetchMotion::Count(0) => {
                // Re-fetch the current row, position unchanged.
                push_row(slot.position, out_rows);
            }
            FetchMotion::Count(k) if k > 0 => {
                for _ in 0..k {
                    if slot.position > n {
                        break;
                    }
                    slot.position += 1;
                    push_row(slot.position, out_rows);
                }
                slot.position = slot.position.min(n + 1);
            }
            FetchMotion::Count(k) => {
                for _ in 0..-i64::from(k) {
                    if slot.position < 1 {
                        break;
                    }
                    slot.position -= 1;
                    push_row(slot.position, out_rows);
                }
                slot.position = slot.position.max(0);
            }
            FetchMotion::All => {
                while slot.position <= n {
                    slot.position += 1;
                    push_row(slot.position, out_rows);
                }
                slot.position = n + 1;
            }
            FetchMotion::BackwardAll => {
                while slot.position >= 1 {
                    slot.position -= 1;
                    push_row(slot.position, out_rows);
                }
                slot.position = 0;
            }
            FetchMotion::Absolute(k) => {
                let target = if k >= 0 {
                    i64::from(k)
                } else {
                    n + 1 + i64::from(k)
                };
                slot.position = target.clamp(0, n + 1);
                push_row(slot.position, out_rows);
            }
            FetchMotion::Relative(k) => {
                slot.position = (slot.position + i64::from(k)).clamp(0, n + 1);
                push_row(slot.position, out_rows);
            }
        }
        Ok(self.emit.len())
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
        }
    }

    pub(crate) fn has_uncommitted_hold_cursor(&self) -> bool {
        self.slots
            .iter()
            .any(|slot| slot.active && slot.tentative && slot.hold)
    }

    /// Transaction commit: cursors become holdable or die.
    pub fn on_commit(&mut self) {
        for s in &mut self.slots {
            if s.active {
                if s.hold {
                    s.tentative = false;
                } else {
                    s.query.clear();
                    s.active = false;
                }
            }
        }
    }

    /// Transaction rollback: everything created in the transaction dies,
    /// WITH HOLD included; an already-held cursor survives.
    pub fn on_rollback(&mut self) {
        for s in &mut self.slots {
            if s.active && (s.tentative || !s.hold) {
                s.query.clear();
                s.active = false;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
                }
                if cursor_row_capacity(&config) != 0 {
                    let mut short = Budget::new(bytes - 1);
                    let error = match CursorPool::new(&config, &mut short) {
                        Ok(_) => panic!("cursor pool accepted an incomplete reservation"),
                        Err(error) => error,
                    };
                    assert_eq!(error.what, "cursor_emit");
                    assert_eq!(error.requested, error.remaining + 1);
                }
            }
        }
    }
}
