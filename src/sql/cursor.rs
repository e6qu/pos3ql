//! SQL cursors (DECLARE / FETCH / MOVE / CLOSE). A cursor materializes its
//! whole result at DECLARE into a fixed per-connection pool — which is
//! exactly PostgreSQL's *insensitive* cursor semantics: the rows are a
//! snapshot as of DECLARE, blind to later changes, and a materialized buffer
//! serves SCROLL (backward, absolute) positioning trivially. A non-SCROLL
//! cursor still refuses backward motion, as PostgreSQL does.

use crate::config::Config;
use crate::mem::budget::{Budget, BudgetError};
use crate::mem::buffer::FixedBuf;
use crate::sql_err;
use crate::storage::SqlName;

use super::eval::{sqlstate, SqlError};

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

pub struct CursorPool {
    slots: Vec<CursorSlot>,
    /// Spans of the rows the last [`Self::fetch`] selected, in emission order.
    emit: Vec<(u32, u32)>,
}

struct CursorSlot {
    active: bool,
    name: SqlName,
    scroll: bool,
    hold: bool,
    /// Created in the still-open transaction: a rollback closes it even WITH
    /// HOLD (holdability begins at commit, as PostgreSQL has it).
    tentative: bool,
    /// The RowDescription message bytes captured at DECLARE.
    description: FixedBuf,
    /// Concatenated DataRow message bytes captured at DECLARE.
    rows: FixedBuf,
    /// Byte span of each DataRow within `rows`, for random access.
    spans: Vec<(u32, u32)>,
    /// PostgreSQL's cursor position: 0 before the first row, `1..=n` on a
    /// row, `n + 1` after the last.
    position: i64,
}

/// How many rows one cursor may hold (the span index's capacity).
const MAX_CURSOR_ROWS: usize = 65536;

impl CursorPool {
    pub fn budget_bytes(config: &Config) -> usize {
        config.max_cursors
            * (config.cursor_bytes
                + 1024
                + MAX_CURSOR_ROWS * core::mem::size_of::<(u32, u32)>())
    }

    pub fn new(config: &Config, budget: &mut Budget) -> Result<Self, BudgetError> {
        let mut slots = Vec::with_capacity(config.max_cursors);
        for _ in 0..config.max_cursors {
            slots.push(CursorSlot {
                active: false,
                name: SqlName::parse("").expect("empty fits"),
                scroll: false,
                hold: false,
                tentative: false,
                description: FixedBuf::new(budget, "cursor_description", 1024)?,
                rows: FixedBuf::new(budget, "cursor_rows", config.cursor_bytes)?,
                spans: Vec::with_capacity(MAX_CURSOR_ROWS),
                position: 0,
            });
        }
        Ok(Self { slots, emit: Vec::with_capacity(MAX_CURSOR_ROWS) })
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
        scroll: bool,
        hold: bool,
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
        slot.tentative = true;
        slot.description.clear();
        slot.rows.clear();
        slot.spans.clear();
        slot.position = 0;
        Ok(at)
    }

    /// The result buffer of a slot being filled at DECLARE.
    pub fn result_buffer(&mut self, at: usize) -> &mut FixedBuf {
        &mut self.slots[at].rows
    }

    /// Splits the captured wire output into the description and per-row
    /// spans, activating the cursor. The buffer holds whole messages
    /// (RowDescription, DataRows, CommandComplete); anything else is a
    /// protocol invariant violation and errors loudly.
    pub fn seal(&mut self, at: usize) -> Result<(), SqlError> {
        let slot = &mut self.slots[at];
        let bytes = slot.rows.readable();
        let mut cursor = 0usize;
        let mut description: Option<(usize, usize)> = None;
        while cursor + 5 <= bytes.len() {
            let kind = bytes[cursor];
            let len =
                u32::from_be_bytes(bytes[cursor + 1..cursor + 5].try_into().unwrap()) as usize;
            let total = 1 + len;
            if cursor + total > bytes.len() {
                return Err(sql_err!(
                    sqlstate::INTERNAL_ERROR,
                    "cursor captured a torn wire message"
                ));
            }
            match kind {
                b'T' => description = Some((cursor, total)),
                b'D' => {
                    if slot.spans.len() == MAX_CURSOR_ROWS {
                        return Err(sql_err!(
                            sqlstate::PROGRAM_LIMIT_EXCEEDED,
                            "cursor holds more than {} rows",
                            MAX_CURSOR_ROWS
                        ));
                    }
                    slot.spans.push((cursor as u32, total as u32));
                }
                // CommandComplete / notices pass through uncaptured.
                _ => {}
            }
            cursor += total;
        }
        if let Some((offset, total)) = description {
            let copied = {
                let mut scratch = [0u8; 1024];
                if total > scratch.len() {
                    return Err(sql_err!(
                        sqlstate::PROGRAM_LIMIT_EXCEEDED,
                        "cursor row description exceeds 1024 bytes"
                    ));
                }
                scratch[..total].copy_from_slice(&slot.rows.readable()[offset..offset + total]);
                (scratch, total)
            };
            slot.description.clear();
            if !slot.description.append(&copied.0[..copied.1]) {
                return Err(sql_err!(
                    sqlstate::PROGRAM_LIMIT_EXCEEDED,
                    "cursor row description exceeds its buffer"
                ));
            }
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
        let n = slot.spans.len() as i64;
        // PostgreSQL refuses backward motion on a NO SCROLL cursor only when
        // the plan cannot scan backward; materialization makes every plan
        // backward-capable, so — by PostgreSQL's own "simple enough" rule —
        // backward motion is always served here. (More lenient than a plan
        // PostgreSQL would refuse; never wrong for one it accepts.)
        self.emit.clear();
        let out_spans = &mut self.emit;
        let push_row = |slot: &CursorSlot, row: i64, out: &mut Vec<(u32, u32)>| {
            if row >= 1 && row <= n {
                out.push(slot.spans[(row - 1) as usize]);
            }
        };
        match motion {
            FetchMotion::Count(0) => {
                // Re-fetch the current row, position unchanged.
                push_row(slot, slot.position, out_spans);
            }
            FetchMotion::Count(k) if k > 0 => {
                for _ in 0..k {
                    if slot.position > n {
                        break;
                    }
                    slot.position += 1;
                    push_row(slot, slot.position, out_spans);
                }
                slot.position = slot.position.min(n + 1);
            }
            FetchMotion::Count(k) => {
                for _ in 0..-k {
                    if slot.position < 1 {
                        break;
                    }
                    slot.position -= 1;
                    push_row(slot, slot.position, out_spans);
                }
                slot.position = slot.position.max(0);
            }
            FetchMotion::All => {
                while slot.position <= n {
                    slot.position += 1;
                    push_row(slot, slot.position, out_spans);
                }
                slot.position = n + 1;
            }
            FetchMotion::BackwardAll => {
                while slot.position >= 1 {
                    slot.position -= 1;
                    push_row(slot, slot.position, out_spans);
                }
                slot.position = 0;
            }
            FetchMotion::Absolute(k) => {
                let target = if k >= 0 { k } else { n + 1 + k };
                slot.position = target.clamp(0, n + 1);
                push_row(slot, slot.position, out_spans);
            }
            FetchMotion::Relative(k) => {
                slot.position = (slot.position + k).clamp(0, n + 1);
                push_row(slot, slot.position, out_spans);
            }
        }
        Ok(self.emit.len())
    }

    /// The spans the last [`Self::fetch`] selected.
    pub fn emitted(&self) -> &[(u32, u32)] {
        &self.emit
    }

    /// The stored RowDescription and row bytes of a cursor (post-`fetch`,
    /// same borrow).
    pub fn wire_parts(&self, name: &str) -> Option<(&[u8], &[u8])> {
        let at = self.find(name)?;
        Some((
            self.slots[at].description.readable(),
            self.slots[at].rows.readable(),
        ))
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
