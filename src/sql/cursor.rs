//! SQL cursors (DECLARE / FETCH / MOVE / CLOSE). A cursor materializes its
//! whole result at DECLARE into a fixed per-connection pool — which is
//! exactly PostgreSQL's *insensitive* cursor semantics: the rows are a
//! snapshot as of DECLARE, blind to later changes, and a materialized buffer
//! serves SCROLL (backward, absolute) positioning trivially. A non-SCROLL
//! cursor still refuses backward motion, as PostgreSQL does.

use crate::config::Config;
use crate::mem::budget::{Budget, BudgetError};
use crate::mem::buffer::FixedBuf;
use crate::pg::respond::ResultFmt;
use crate::sql_err;
use crate::storage::SqlName;

use super::eval::{SqlError, sqlstate};

/// One FETCH/MOVE motion, normalized by the parser: positive counts move
/// forward, negative backward (`NEXT` is `Count(1)`, `PRIOR` is `Count(-1)`,
/// `FIRST` is `Absolute(1)`, `LAST` is `Absolute(-1)`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FetchMotion {
    Count(i64),
    All,
    BackwardAll,
    Absolute(i64),
    Relative(i64),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CursorScroll {
    Default,
    Scroll,
    NoScroll,
}

pub struct CursorWireParts<'a> {
    pub description: &'a [u8],
    pub text: &'a [u8],
    pub text_spans: &'a [(u32, u32)],
    pub binary: &'a [u8],
    pub binary_spans: &'a [(u32, u32)],
    pub declared_binary: bool,
}

pub struct CursorPool {
    slots: Vec<CursorSlot>,
    /// Row indexes selected by the last [`Self::fetch`], in emission order.
    emit: Vec<u32>,
}

struct CursorSlot {
    active: bool,
    name: SqlName,
    scroll: CursorScroll,
    hold: bool,
    binary: bool,
    /// Created in the still-open transaction: a rollback closes it even WITH
    /// HOLD (holdability begins at commit, as PostgreSQL has it).
    tentative: bool,
    description_text: FixedBuf,
    description_binary: FixedBuf,
    rows_text: FixedBuf,
    rows_binary: FixedBuf,
    spans_text: Vec<(u32, u32)>,
    spans_binary: Vec<(u32, u32)>,
    /// PostgreSQL's cursor position: 0 before the first row, `1..=n` on a
    /// row, `n + 1` after the last.
    position: i64,
}

/// How many rows one cursor may hold (the span index's capacity).
const MAX_CURSOR_ROWS: usize = 65536;

fn seal_capture(
    rows: &FixedBuf,
    description: &mut FixedBuf,
    spans: &mut Vec<(u32, u32)>,
) -> Result<(), SqlError> {
    let bytes = rows.readable();
    let mut cursor = 0usize;
    let mut captured_description: Option<(usize, usize)> = None;
    while cursor + 5 <= bytes.len() {
        let kind = bytes[cursor];
        let len = u32::from_be_bytes(bytes[cursor + 1..cursor + 5].try_into().unwrap()) as usize;
        let total = 1 + len;
        if cursor + total > bytes.len() {
            return Err(sql_err!(
                sqlstate::INTERNAL_ERROR,
                "cursor captured a torn wire message"
            ));
        }
        match kind {
            b'T' => captured_description = Some((cursor, total)),
            b'D' => {
                if spans.len() == MAX_CURSOR_ROWS {
                    return Err(sql_err!(
                        sqlstate::PROGRAM_LIMIT_EXCEEDED,
                        "cursor holds more than {} rows",
                        MAX_CURSOR_ROWS
                    ));
                }
                spans.push((cursor as u32, total as u32));
            }
            _ => {}
        }
        cursor += total;
    }
    if cursor != bytes.len() {
        return Err(sql_err!(
            sqlstate::INTERNAL_ERROR,
            "cursor captured a torn wire message"
        ));
    }
    if let Some((offset, total)) = captured_description
        && (total > description.capacity() || !description.append(&bytes[offset..offset + total]))
    {
        return Err(sql_err!(
            sqlstate::PROGRAM_LIMIT_EXCEEDED,
            "cursor row description exceeds its buffer"
        ));
    }
    Ok(())
}

impl CursorPool {
    pub fn budget_bytes(config: &Config) -> usize {
        config.max_cursors
            * (config.cursor_bytes * 2
                + 2048
                + MAX_CURSOR_ROWS * 2 * core::mem::size_of::<(u32, u32)>())
    }

    pub fn new(config: &Config, budget: &mut Budget) -> Result<Self, BudgetError> {
        let mut slots = Vec::with_capacity(config.max_cursors);
        for _ in 0..config.max_cursors {
            slots.push(CursorSlot {
                active: false,
                name: SqlName::parse("").expect("empty fits"),
                scroll: CursorScroll::Default,
                hold: false,
                binary: false,
                tentative: false,
                description_text: FixedBuf::new(budget, "cursor_text_description", 1024)?,
                description_binary: FixedBuf::new(budget, "cursor_binary_description", 1024)?,
                rows_text: FixedBuf::new(budget, "cursor_text_rows", config.cursor_bytes)?,
                rows_binary: FixedBuf::new(budget, "cursor_binary_rows", config.cursor_bytes)?,
                spans_text: Vec::with_capacity(MAX_CURSOR_ROWS),
                spans_binary: Vec::with_capacity(MAX_CURSOR_ROWS),
                position: 0,
            });
        }
        Ok(Self {
            slots,
            emit: Vec::with_capacity(MAX_CURSOR_ROWS),
        })
    }

    /// Closes every cursor — used when a connection slot is recycled for a new
    /// client so no cursor leaks across sessions.
    pub fn clear(&mut self) {
        for s in &mut self.slots {
            s.active = false;
        }
        self.emit.clear();
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
        slot.scroll = scroll;
        slot.hold = hold;
        slot.binary = binary;
        slot.tentative = true;
        slot.description_text.clear();
        slot.description_binary.clear();
        slot.rows_text.clear();
        slot.rows_binary.clear();
        slot.spans_text.clear();
        slot.spans_binary.clear();
        slot.position = 0;
        Ok(at)
    }

    /// The result buffer of a slot being filled at DECLARE.
    pub fn result_buffers(&mut self, at: usize) -> (&mut FixedBuf, &mut FixedBuf) {
        let slot = &mut self.slots[at];
        (&mut slot.rows_text, &mut slot.rows_binary)
    }

    /// Splits the captured wire output into the description and per-row
    /// spans, activating the cursor. The buffer holds whole messages
    /// (RowDescription, DataRows, CommandComplete); anything else is a
    /// protocol invariant violation and errors loudly.
    pub fn seal(&mut self, at: usize) -> Result<(), SqlError> {
        let slot = &mut self.slots[at];
        seal_capture(
            &slot.rows_text,
            &mut slot.description_text,
            &mut slot.spans_text,
        )?;
        seal_capture(
            &slot.rows_binary,
            &mut slot.description_binary,
            &mut slot.spans_binary,
        )?;
        if slot.spans_text.len() != slot.spans_binary.len() {
            return Err(sql_err!(
                sqlstate::INTERNAL_ERROR,
                "cursor wire representations have different row counts"
            ));
        }
        slot.active = true;
        Ok(())
    }

    /// Drops a half-built slot after a failed DECLARE.
    pub fn abandon(&mut self, at: usize) {
        self.slots[at].active = false;
        self.slots[at].tentative = false;
    }

    pub fn exists(&self, name: &str) -> bool {
        self.find(name).is_some()
    }

    /// The declaration-time RowDescription of a live SQL cursor. PostgreSQL
    /// exposes an SQL cursor as a describable protocol portal, so drivers can
    /// install result decoders before their first FETCH.
    pub fn description(&self, name: &str) -> Option<&[u8]> {
        let at = self.find(name)?;
        let slot = &self.slots[at];
        Some(if slot.binary {
            slot.description_binary.readable()
        } else {
            slot.description_text.readable()
        })
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
        Some((slot.description_text.readable(), formats))
    }

    /// Applies one FETCH/MOVE motion: returns the spans of the rows to emit
    /// (in emission order) plus the description bytes, updating the cursor's
    /// position. Backward motion on a non-SCROLL cursor is PostgreSQL's
    /// 55000.
    pub fn fetch(&mut self, name: &str, motion: FetchMotion) -> Result<usize, SqlError> {
        let Some(at) = self.find(name) else {
            return Err(sql_err!(
                sqlstate::UNDEFINED_CURSOR,
                "cursor \"{}\" does not exist",
                name
            ));
        };
        let slot = &mut self.slots[at];
        let n = slot.spans_text.len() as i64;
        let target = match motion {
            FetchMotion::Absolute(k) if k >= 0 => k.clamp(0, n + 1),
            FetchMotion::Absolute(k) => (n + 1 + k).clamp(0, n + 1),
            FetchMotion::Relative(k) => (slot.position + k).clamp(0, n + 1),
            _ => slot.position,
        };
        let backward = matches!(motion, FetchMotion::Count(k) if k < 0)
            || matches!(motion, FetchMotion::BackwardAll)
            || matches!(motion, FetchMotion::Absolute(k) if k < 0 || target < slot.position)
            || matches!(motion, FetchMotion::Relative(k) if k < 0);
        if slot.scroll == CursorScroll::NoScroll && backward {
            return Err(sql_err!(
                sqlstate::OBJECT_NOT_IN_PREREQUISITE_STATE,
                "cursor can only scan forward"
            ));
        }
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
                for _ in 0..-k {
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
                let target = if k >= 0 { k } else { n + 1 + k };
                slot.position = target.clamp(0, n + 1);
                push_row(slot.position, out_rows);
            }
            FetchMotion::Relative(k) => {
                slot.position = (slot.position + k).clamp(0, n + 1);
                push_row(slot.position, out_rows);
            }
        }
        Ok(self.emit.len())
    }

    /// The row indexes selected by the last [`Self::fetch`].
    pub fn emitted(&self) -> &[u32] {
        &self.emit
    }

    /// The stored representations used to assemble FETCH output.
    pub fn wire_parts(&self, name: &str) -> Option<CursorWireParts<'_>> {
        let at = self.find(name)?;
        let slot = &self.slots[at];
        Some(CursorWireParts {
            description: slot.description_text.readable(),
            text: slot.rows_text.readable(),
            text_spans: &slot.spans_text,
            binary: slot.rows_binary.readable(),
            binary_spans: &slot.spans_binary,
            declared_binary: slot.binary,
        })
    }

    /// CLOSE name — false when no such cursor exists.
    pub fn close(&mut self, name: &str) -> bool {
        match self.find(name) {
            Some(at) => {
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
                s.active = false;
            }
        }
    }
}
