//! The transaction's queue of AFTER trigger and foreign key events.
//!
//! PostgreSQL keeps every queued AFTER event until its statement, `SET
//! CONSTRAINTS` or commit fires it, however many rows a statement writes. The
//! queue here is a byte log of variable-length event records cut into fixed
//! pages. `deferred_trigger_bytes` of pages stay in memory per connection;
//! older pages move to a process-local spill file of
//! `trigger_event_spill_bytes`, shared by all connections. Only when both are
//! full does queuing fail, with `54000` naming both settings.
//!
//! An event is identified by its byte offset in the log, which stays stable
//! until the transaction ends or a rollback discards it. A fired event carries
//! the firing number that completed it, so rolling back to a statement or
//! savepoint mark re-arms exactly the events fired after the mark, as
//! PostgreSQL's subtransaction abort does.

use std::fs::File;
use std::os::unix::fs::FileExt;
use std::path::Path;

use crate::mem::budget::{Budget, BudgetError};
use crate::mem::fixed_vec::FixedVec;
use crate::mem::mutex::Mutex;
use crate::sql::eval::{SqlError, sqlstate};
use crate::sql::txn::{
    ConstraintIdentity, DeferredTriggerEvent, DeferredTriggerKind, DeferredTriggerTuple,
    ForeignKeyCheck,
};
use crate::sql_err;
use crate::storage::{ColumnSet, SqlName};

/// One page of the event log, in memory and in the spill file.
pub(crate) const TRIGGER_EVENT_PAGE_BYTES: usize = 64 * 1024;

const NO_PAGE: u32 = u32::MAX;
const NO_SLOT: u32 = u32::MAX;
const NO_RECORD: u32 = u32::MAX;

/// Process-local spill file for event pages. It is created empty at startup,
/// never fsynced and never published: queued events end with their
/// transaction, so a restart has nothing to recover.
pub(crate) struct TriggerEventSpill {
    file: File,
    free: FixedVec<u32>,
}

#[derive(Debug)]
pub enum TriggerEventSpillError {
    Budget(BudgetError),
    Io(std::io::Error),
    TooSmall,
}

impl std::fmt::Display for TriggerEventSpillError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Budget(error) => write!(formatter, "{error}"),
            Self::Io(error) => write!(formatter, "trigger event spill file: {error}"),
            Self::TooSmall => write!(
                formatter,
                "trigger_event_spill_bytes is smaller than one page ({TRIGGER_EVENT_PAGE_BYTES} bytes)"
            ),
        }
    }
}

impl std::error::Error for TriggerEventSpillError {}

pub(crate) type SharedTriggerEventSpill = std::sync::Arc<Mutex<TriggerEventSpill>>;

impl TriggerEventSpill {
    pub(crate) const fn pages(bytes: usize) -> usize {
        bytes / TRIGGER_EVENT_PAGE_BYTES
    }

    pub(crate) fn budget_bytes(bytes: usize) -> usize {
        Self::pages(bytes).saturating_mul(core::mem::size_of::<u32>())
    }

    pub(crate) fn open(
        budget: &mut Budget,
        path: &Path,
        bytes: usize,
    ) -> Result<Self, TriggerEventSpillError> {
        let pages = Self::pages(bytes);
        if pages == 0 {
            return Err(TriggerEventSpillError::TooSmall);
        }
        let mut free = FixedVec::new(budget, "trigger event spill pages", pages)
            .map_err(TriggerEventSpillError::Budget)?;
        if let Some(directory) = path.parent() {
            std::fs::create_dir_all(directory).map_err(TriggerEventSpillError::Io)?;
        }
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .open(path)
            .map_err(TriggerEventSpillError::Io)?;
        file.set_len((pages * TRIGGER_EVENT_PAGE_BYTES) as u64)
            .map_err(TriggerEventSpillError::Io)?;
        for slot in (0..pages as u32).rev() {
            free.push(slot).expect("sized to the page count");
        }
        Ok(Self { file, free })
    }

    fn allocate(&mut self) -> Option<u32> {
        self.free.pop()
    }

    fn release(&mut self, slot: u32) {
        self.free
            .push(slot)
            .expect("a released page was allocated from this pool");
    }

    fn write(&self, slot: u32, page: &[u8]) -> Result<(), SqlError> {
        self.file
            .write_all_at(page, u64::from(slot) * TRIGGER_EVENT_PAGE_BYTES as u64)
            .map_err(|error| {
                sql_err!(
                    sqlstate::IO_ERROR,
                    "could not write trigger event spill page: {}",
                    error
                )
            })
    }

    fn read(&self, slot: u32, page: &mut [u8]) -> Result<(), SqlError> {
        self.file
            .read_exact_at(page, u64::from(slot) * TRIGGER_EVENT_PAGE_BYTES as u64)
            .map_err(|error| {
                sql_err!(
                    sqlstate::IO_ERROR,
                    "could not read trigger event spill page: {}",
                    error
                )
            })
    }
}

#[derive(Clone, Copy)]
struct PageEntry {
    /// The spill slot holding this page's latest evicted image.
    slot: u32,
    /// The highest firing number stamped on a record starting in this page.
    max_fired: u32,
    /// The in-page offset of the first record starting in this page.
    first_record: u32,
}

#[derive(Clone, Copy)]
struct Frame {
    page: u32,
    dirty: bool,
    used: u64,
}

/// Rollback position of the queue: its length and the firing number reached.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TriggerEventMark {
    pub(crate) len: u64,
    pub(crate) fired: u32,
}

pub(crate) struct TriggerEventQueue {
    memory: Box<[u8]>,
    frames: Box<[Frame]>,
    pages: FixedVec<PageEntry>,
    len: u64,
    fired: u32,
    tick: u64,
    /// Events still waiting to fire, or `None` after a rollback until the
    /// next scan counts them again.
    waiting: Option<u64>,
    spill: Option<SharedTriggerEventSpill>,
    /// A rollback could not re-arm spilled events; the queue refuses all
    /// use until the transaction ends.
    lost: bool,
}

// Record layout: total length u32, firing number u32, kind u8, event u8,
// flags u8, check u8, trigger depth u16, effective table u16, command u32,
// then the kind's payload, the updated columns, the OLD and NEW lengths and
// the OLD and NEW row images.
const FIRED_AT: u64 = 4;
const FIXED_HEADER: usize = 20;
const MAX_HEADER: usize = FIXED_HEADER + 1 + 4 + 2 + 1 + 63 + 8 + ColumnSet::ENCODED_BYTES + 8;
const FLAG_OLD: u8 = 1;
const FLAG_NEW: u8 = 2;
const FLAG_COLUMNS: u8 = 4;
const KIND_AFTER_ROW: u8 = 0;
const KIND_CONSTRAINT: u8 = 1;
const KIND_FOREIGN_KEY: u8 = 2;

fn queue_full() -> SqlError {
    sql_err!(
        sqlstate::PROGRAM_LIMIT_EXCEEDED,
        "queued AFTER trigger events exceed deferred_trigger_bytes in memory and trigger_event_spill_bytes on disk"
    )
}

fn lost() -> SqlError {
    sql_err!(
        sqlstate::IO_ERROR,
        "queued AFTER trigger events were lost by a failed spill file read during rollback"
    )
}

fn corrupt() -> SqlError {
    sql_err!(
        sqlstate::INTERNAL_ERROR,
        "queued AFTER trigger event record is malformed"
    )
}

impl TriggerEventQueue {
    /// In-memory pages for `memory_bytes`; `spill_pages` bounds how many
    /// pages one transaction may move to the shared spill file.
    pub(crate) const fn budget_bytes(memory_bytes: usize, spill_pages: usize) -> usize {
        let frames = memory_bytes / TRIGGER_EVENT_PAGE_BYTES;
        (frames.saturating_mul(TRIGGER_EVENT_PAGE_BYTES))
            .saturating_add(frames.saturating_mul(core::mem::size_of::<Frame>()))
            .saturating_add(
                frames
                    .saturating_add(spill_pages)
                    .saturating_mul(core::mem::size_of::<PageEntry>()),
            )
    }

    pub(crate) fn new(
        budget: &mut Budget,
        memory_bytes: usize,
        spill_pages: usize,
    ) -> Result<Self, BudgetError> {
        let frames = memory_bytes / TRIGGER_EVENT_PAGE_BYTES;
        budget.draw_array(frames, TRIGGER_EVENT_PAGE_BYTES, "trigger event pages")?;
        budget.draw_array(
            frames,
            core::mem::size_of::<Frame>(),
            "trigger event page frames",
        )?;
        Ok(Self {
            memory: vec![0; frames * TRIGGER_EVENT_PAGE_BYTES].into_boxed_slice(),
            frames: vec![
                Frame {
                    page: NO_PAGE,
                    dirty: false,
                    used: 0,
                };
                frames
            ]
            .into_boxed_slice(),
            pages: FixedVec::new(
                budget,
                "trigger event page table",
                frames.saturating_add(spill_pages),
            )?,
            len: 0,
            fired: 0,
            tick: 0,
            waiting: Some(0),
            spill: None,
            lost: false,
        })
    }

    pub(crate) fn attach_spill(&mut self, spill: SharedTriggerEventSpill) {
        self.spill = Some(spill);
    }

    pub(crate) fn mark(&self) -> TriggerEventMark {
        TriggerEventMark {
            len: self.len,
            fired: self.fired,
        }
    }

    pub(crate) fn end(&self) -> u64 {
        self.len
    }

    fn frame_bytes(&mut self, frame: usize) -> &mut [u8] {
        &mut self.memory[frame * TRIGGER_EVENT_PAGE_BYTES..(frame + 1) * TRIGGER_EVENT_PAGE_BYTES]
    }

    /// The frame holding `page`, loading it (and evicting the least recently
    /// used page) when it is not resident.
    fn frame(&mut self, page: u32) -> Result<usize, SqlError> {
        self.tick += 1;
        if let Some(frame) = self.frames.iter().position(|frame| frame.page == page) {
            self.frames[frame].used = self.tick;
            return Ok(frame);
        }
        let victim = self
            .frames
            .iter()
            .enumerate()
            .min_by_key(|(_, frame)| (frame.page != NO_PAGE, frame.used))
            .map(|(index, _)| index)
            .ok_or_else(queue_full)?;
        let evicted = self.frames[victim];
        if evicted.page != NO_PAGE && evicted.dirty {
            let spill = self.spill.clone().ok_or_else(queue_full)?;
            let mut spill = spill.lock().map_err(|_| {
                sql_err!(
                    sqlstate::INTERNAL_ERROR,
                    "trigger event spill file is poisoned"
                )
            })?;
            let entry = &mut self.pages[evicted.page as usize];
            if entry.slot == NO_SLOT {
                entry.slot = spill.allocate().ok_or_else(queue_full)?;
            }
            let slot = entry.slot;
            spill.write(slot, self.frame_bytes(victim))?;
        }
        self.frames[victim] = Frame {
            page: NO_PAGE,
            dirty: false,
            used: self.tick,
        };
        let slot = self.pages[page as usize].slot;
        if slot == NO_SLOT {
            self.frame_bytes(victim).fill(0);
        } else {
            let spill = self.spill.clone().ok_or_else(corrupt)?;
            let spill = spill.lock().map_err(|_| {
                sql_err!(
                    sqlstate::INTERNAL_ERROR,
                    "trigger event spill file is poisoned"
                )
            })?;
            spill.read(slot, self.frame_bytes(victim))?;
        }
        self.frames[victim].page = page;
        Ok(victim)
    }

    fn read_at(&mut self, mut offset: u64, out: &mut [u8]) -> Result<(), SqlError> {
        if self.lost {
            return Err(lost());
        }
        let mut done = 0;
        while done < out.len() {
            let page = (offset / TRIGGER_EVENT_PAGE_BYTES as u64) as u32;
            let within = (offset % TRIGGER_EVENT_PAGE_BYTES as u64) as usize;
            let take = (out.len() - done).min(TRIGGER_EVENT_PAGE_BYTES - within);
            let frame = self.frame(page)?;
            out[done..done + take].copy_from_slice(&self.frame_bytes(frame)[within..within + take]);
            done += take;
            offset += take as u64;
        }
        Ok(())
    }

    fn write_at(&mut self, mut offset: u64, bytes: &[u8]) -> Result<(), SqlError> {
        if self.lost {
            return Err(lost());
        }
        let mut done = 0;
        while done < bytes.len() {
            let page = (offset / TRIGGER_EVENT_PAGE_BYTES as u64) as u32;
            let within = (offset % TRIGGER_EVENT_PAGE_BYTES as u64) as usize;
            let take = (bytes.len() - done).min(TRIGGER_EVENT_PAGE_BYTES - within);
            let frame = self.frame(page)?;
            self.frame_bytes(frame)[within..within + take]
                .copy_from_slice(&bytes[done..done + take]);
            self.frames[frame].dirty = true;
            done += take;
            offset += take as u64;
        }
        Ok(())
    }

    /// Appends one event and returns its identity.
    pub(crate) fn push(
        &mut self,
        event: &DeferredTriggerEvent,
        old: Option<&[u8]>,
        new: Option<&[u8]>,
    ) -> Result<u64, SqlError> {
        let mut header = [0u8; MAX_HEADER];
        let header_len = encode_header(event, old, new, &mut header)?;
        let total = header_len + old.map_or(0, <[u8]>::len) + new.map_or(0, <[u8]>::len);
        let total = u32::try_from(total).map_err(|_| queue_full())?;
        header[..4].copy_from_slice(&total.to_le_bytes());
        let start = self.len;
        let end = start.checked_add(u64::from(total)).ok_or_else(queue_full)?;
        let last_page = (end.saturating_sub(1) / TRIGGER_EVENT_PAGE_BYTES as u64) as usize;
        if last_page >= self.pages.capacity() {
            return Err(queue_full());
        }
        while self.pages.len() <= last_page {
            self.pages
                .push(PageEntry {
                    slot: NO_SLOT,
                    max_fired: 0,
                    first_record: NO_RECORD,
                })
                .map_err(|_| queue_full())?;
        }
        let result = self
            .write_at(start, &header[..header_len])
            .and_then(|()| match old {
                Some(old) => self.write_at(start + header_len as u64, old),
                None => Ok(()),
            })
            .and_then(|()| match new {
                Some(new) => self.write_at(
                    start + header_len as u64 + old.map_or(0, |old| old.len() as u64),
                    new,
                ),
                None => Ok(()),
            });
        if let Err(error) = result {
            self.truncate(start);
            return Err(error);
        }
        let page = (start / TRIGGER_EVENT_PAGE_BYTES as u64) as usize;
        if self.pages[page].first_record == NO_RECORD {
            self.pages[page].first_record = (start % TRIGGER_EVENT_PAGE_BYTES as u64) as u32;
        }
        self.len = end;
        if let Some(waiting) = &mut self.waiting {
            *waiting += 1;
        }
        Ok(start)
    }

    /// The event at `id`, the identity of the next one, and whether it fired.
    pub(crate) fn event(&mut self, id: u64) -> Result<(DeferredTriggerEvent, u64, bool), SqlError> {
        if id >= self.len {
            return Err(corrupt());
        }
        let mut header = [0u8; MAX_HEADER];
        let available = ((self.len - id) as usize).min(MAX_HEADER);
        self.read_at(id, &mut header[..available])?;
        let (event, total, fired) = decode_header(id, &header[..available])?;
        Ok((event, id + u64::from(total), fired))
    }

    pub(crate) fn is_fired(&mut self, id: u64) -> Result<bool, SqlError> {
        let mut fired = [0u8; 4];
        self.read_at(id + FIRED_AT, &mut fired)?;
        Ok(u32::from_le_bytes(fired) != 0)
    }

    /// Marks `id` fired; a rollback past this firing re-arms it.
    pub(crate) fn fire(&mut self, id: u64) -> Result<(), SqlError> {
        if self.is_fired(id)? {
            return Ok(());
        }
        let fired = self.fired.checked_add(1).ok_or_else(|| {
            sql_err!(
                sqlstate::PROGRAM_LIMIT_EXCEEDED,
                "transaction fires more than {} AFTER trigger events",
                u32::MAX - 1
            )
        })?;
        self.write_at(id + FIRED_AT, &fired.to_le_bytes())?;
        self.fired = fired;
        let page = (id / TRIGGER_EVENT_PAGE_BYTES as u64) as usize;
        self.pages[page].max_fired = fired;
        if let Some(waiting) = &mut self.waiting {
            *waiting = waiting.saturating_sub(1);
        }
        Ok(())
    }

    pub(crate) fn tuple(
        &mut self,
        tuple: DeferredTriggerTuple,
        out: &mut [u8],
    ) -> Result<(), SqlError> {
        if out.len() != tuple.length as usize {
            return Err(corrupt());
        }
        self.read_at(tuple.offset, out)
    }

    /// Whether any event has not fired.
    pub(crate) fn has_waiting(&mut self) -> Result<bool, SqlError> {
        if let Some(waiting) = self.waiting {
            return Ok(waiting != 0);
        }
        let mut waiting = 0;
        let mut id = 0;
        while id < self.len {
            if !self.is_fired(id)? {
                waiting += 1;
            }
            id = self.event(id)?.1;
        }
        self.waiting = Some(waiting);
        Ok(waiting != 0)
    }

    fn truncate(&mut self, len: u64) {
        let keep_pages = len.div_ceil(TRIGGER_EVENT_PAGE_BYTES as u64) as usize;
        while self.pages.len() > keep_pages {
            let page = self.pages.len() - 1;
            let entry = self.pages.pop().expect("page exists");
            if entry.slot != NO_SLOT
                && let Some(spill) = &self.spill
                && let Ok(mut spill) = spill.lock()
            {
                spill.release(entry.slot);
            }
            for frame in self.frames.iter_mut() {
                if frame.page == page as u32 {
                    frame.page = NO_PAGE;
                    frame.dirty = false;
                }
            }
        }
        if keep_pages > 0 {
            let last = keep_pages - 1;
            let start = last as u64 * TRIGGER_EVENT_PAGE_BYTES as u64;
            let first = self.pages[last].first_record;
            if first != NO_RECORD && start + u64::from(first) >= len {
                self.pages[last].first_record = NO_RECORD;
            }
        }
        if len < self.len {
            self.waiting = None;
        }
        self.len = len;
    }

    /// Discards events queued after `mark` and re-arms events fired after it.
    pub(crate) fn rewind(&mut self, mark: TriggerEventMark) {
        if self.rearm(mark).is_err() {
            self.lost = true;
        }
    }

    fn rearm(&mut self, mark: TriggerEventMark) -> Result<(), SqlError> {
        self.truncate(mark.len.min(self.len));
        if self.fired <= mark.fired {
            return Ok(());
        }
        for page in 0..self.pages.len() {
            if self.pages[page].max_fired <= mark.fired
                || self.pages[page].first_record == NO_RECORD
            {
                continue;
            }
            let page_start = page as u64 * TRIGGER_EVENT_PAGE_BYTES as u64;
            let page_end = page_start + TRIGGER_EVENT_PAGE_BYTES as u64;
            let mut id = page_start + u64::from(self.pages[page].first_record);
            let mut max_fired = 0;
            while id < page_end && id < self.len {
                let mut fired = [0u8; 4];
                self.read_at(id + FIRED_AT, &mut fired)?;
                let fired = u32::from_le_bytes(fired);
                if fired > mark.fired {
                    self.write_at(id + FIRED_AT, &0u32.to_le_bytes())?;
                } else {
                    max_fired = max_fired.max(fired);
                }
                id = self.event(id)?.1;
            }
            self.pages[page].max_fired = max_fired;
        }
        self.fired = mark.fired;
        self.waiting = None;
        Ok(())
    }

    /// Empties the queue, returning its spilled pages to the shared file.
    pub(crate) fn clear(&mut self) {
        self.truncate(0);
        self.fired = 0;
        self.waiting = Some(0);
        self.lost = false;
    }
}

fn put(header: &mut [u8], at: &mut usize, bytes: &[u8]) -> Result<(), SqlError> {
    header
        .get_mut(*at..*at + bytes.len())
        .ok_or_else(corrupt)?
        .copy_from_slice(bytes);
    *at += bytes.len();
    Ok(())
}

fn take<'a>(header: &'a [u8], at: &mut usize, len: usize) -> Result<&'a [u8], SqlError> {
    let bytes = header.get(*at..*at + len).ok_or_else(corrupt)?;
    *at += len;
    Ok(bytes)
}

fn take_u16(header: &[u8], at: &mut usize) -> Result<u16, SqlError> {
    Ok(u16::from_le_bytes(
        take(header, at, 2)?.try_into().expect("two bytes"),
    ))
}

fn take_u32(header: &[u8], at: &mut usize) -> Result<u32, SqlError> {
    Ok(u32::from_le_bytes(
        take(header, at, 4)?.try_into().expect("four bytes"),
    ))
}

fn encode_identity(
    identity: ConstraintIdentity,
    header: &mut [u8],
    at: &mut usize,
) -> Result<(), SqlError> {
    match identity {
        ConstraintIdentity::Table {
            table,
            name,
            generation,
        } => {
            put(header, at, &[0])?;
            put(header, at, &table.to_le_bytes())?;
            put(header, at, &generation.to_le_bytes())?;
            put(header, at, &[name.as_str().len() as u8])?;
            put(header, at, name.as_str().as_bytes())
        }
        ConstraintIdentity::Trigger { table, slot } => {
            put(header, at, &[1])?;
            put(header, at, &table.to_le_bytes())?;
            put(header, at, &slot.to_le_bytes())
        }
    }
}

fn decode_identity(header: &[u8], at: &mut usize) -> Result<ConstraintIdentity, SqlError> {
    match take(header, at, 1)?[0] {
        0 => {
            let table = take_u32(header, at)?;
            let generation = take_u16(header, at)?;
            let len = take(header, at, 1)?[0] as usize;
            let name = core::str::from_utf8(take(header, at, len)?).map_err(|_| corrupt())?;
            Ok(ConstraintIdentity::Table {
                table,
                name: SqlName::parse(name).map_err(|_| corrupt())?,
                generation,
            })
        }
        1 => Ok(ConstraintIdentity::Trigger {
            table: take_u32(header, at)?,
            slot: take_u16(header, at)?,
        }),
        _ => Err(corrupt()),
    }
}

fn encode_header(
    event: &DeferredTriggerEvent,
    old: Option<&[u8]>,
    new: Option<&[u8]>,
    header: &mut [u8; MAX_HEADER],
) -> Result<usize, SqlError> {
    let (kind, check, depth, table, command) = match event.kind {
        DeferredTriggerKind::AfterRow {
            effective_table,
            trigger_depth,
            command,
            ..
        } => (KIND_AFTER_ROW, 0, trigger_depth, effective_table, command),
        DeferredTriggerKind::Constraint {
            effective_table,
            trigger_depth,
            command,
            ..
        } => (KIND_CONSTRAINT, 0, trigger_depth, effective_table, command),
        DeferredTriggerKind::ForeignKey {
            check,
            effective_table,
            trigger_depth,
            command,
            ..
        } => (
            KIND_FOREIGN_KEY,
            check.code(),
            trigger_depth,
            effective_table,
            command,
        ),
    };
    let columns = !event.updated_columns.is_empty();
    let flags = (u8::from(old.is_some()) * FLAG_OLD)
        | (u8::from(new.is_some()) * FLAG_NEW)
        | (u8::from(columns) * FLAG_COLUMNS);
    let mut at = 8;
    put(header, &mut at, &[kind, event.event, flags, check])?;
    put(header, &mut at, &depth.to_le_bytes())?;
    put(header, &mut at, &table.to_le_bytes())?;
    put(header, &mut at, &command.to_le_bytes())?;
    match event.kind {
        DeferredTriggerKind::AfterRow { trigger_slot, .. } => {
            put(header, &mut at, &trigger_slot.to_le_bytes())?;
        }
        DeferredTriggerKind::Constraint { identity, .. } => {
            encode_identity(identity, header, &mut at)?;
        }
        DeferredTriggerKind::ForeignKey {
            identity, rowid, ..
        } => {
            encode_identity(identity, header, &mut at)?;
            put(header, &mut at, &rowid.to_le_bytes())?;
        }
    }
    if columns {
        for word in event.updated_columns.words() {
            put(header, &mut at, &word.to_le_bytes())?;
        }
    }
    for tuple in [old, new].into_iter().flatten() {
        let len = u32::try_from(tuple.len()).map_err(|_| queue_full())?;
        put(header, &mut at, &len.to_le_bytes())?;
    }
    Ok(at)
}

fn decode_header(id: u64, header: &[u8]) -> Result<(DeferredTriggerEvent, u32, bool), SqlError> {
    let mut at = 0;
    let total = take_u32(header, &mut at)?;
    let fired = take_u32(header, &mut at)? != 0;
    let fixed = take(header, &mut at, 4)?;
    let (kind, event, flags, check) = (fixed[0], fixed[1], fixed[2], fixed[3]);
    let trigger_depth = take_u16(header, &mut at)?;
    let effective_table = take_u16(header, &mut at)?;
    let command = take_u32(header, &mut at)?;
    let kind = match kind {
        KIND_AFTER_ROW => DeferredTriggerKind::AfterRow {
            trigger_slot: take_u16(header, &mut at)?,
            effective_table,
            trigger_depth,
            command,
        },
        KIND_CONSTRAINT => DeferredTriggerKind::Constraint {
            identity: decode_identity(header, &mut at)?,
            effective_table,
            trigger_depth,
            command,
        },
        KIND_FOREIGN_KEY => {
            let identity = decode_identity(header, &mut at)?;
            let rowid = u64::from_le_bytes(take(header, &mut at, 8)?.try_into().expect("eight"));
            DeferredTriggerKind::ForeignKey {
                identity,
                check: ForeignKeyCheck::from_code(check).ok_or_else(corrupt)?,
                effective_table,
                trigger_depth,
                command,
                rowid,
            }
        }
        _ => return Err(corrupt()),
    };
    let mut updated_columns = ColumnSet::EMPTY;
    if flags & FLAG_COLUMNS != 0 {
        for index in 0..ColumnSet::ENCODED_BYTES / 8 {
            let word = u64::from_le_bytes(take(header, &mut at, 8)?.try_into().expect("eight"));
            if !updated_columns.set_word(index, word) {
                return Err(corrupt());
            }
        }
    }
    let old_len = (flags & FLAG_OLD != 0)
        .then(|| take_u32(header, &mut at))
        .transpose()?;
    let new_len = (flags & FLAG_NEW != 0)
        .then(|| take_u32(header, &mut at))
        .transpose()?;
    let body = id + at as u64;
    let old = old_len.map(|length| DeferredTriggerTuple {
        offset: body,
        length,
    });
    let new = new_len.map(|length| DeferredTriggerTuple {
        offset: body + u64::from(old_len.unwrap_or(0)),
        length,
    });
    if u64::from(total)
        != at as u64 + u64::from(old_len.unwrap_or(0)) + u64::from(new_len.unwrap_or(0))
    {
        return Err(corrupt());
    }
    Ok((
        DeferredTriggerEvent {
            kind,
            event,
            updated_columns,
            old,
            new,
        },
        total,
        fired,
    ))
}

#[cfg(all(test, not(all(miri, feature = "miri-ownership"))))]
mod tests {
    use super::*;

    fn row_event(slot: u16) -> DeferredTriggerEvent {
        DeferredTriggerEvent {
            kind: DeferredTriggerKind::AfterRow {
                trigger_slot: slot,
                effective_table: 3,
                trigger_depth: 1,
                command: 7,
            },
            event: 4,
            updated_columns: ColumnSet::EMPTY,
            old: None,
            new: None,
        }
    }

    #[test]
    fn events_spill_reload_and_rewind_within_fixed_pages() {
        let path =
            std::env::temp_dir().join(format!("pos3ql-trigger-event-spill-{}", std::process::id()));
        let spill_bytes = 8 * TRIGGER_EVENT_PAGE_BYTES;
        let mut budget = Budget::new(
            TriggerEventSpill::budget_bytes(spill_bytes)
                + TriggerEventQueue::budget_bytes(TRIGGER_EVENT_PAGE_BYTES, 8),
        );
        let spill = std::sync::Arc::new(Mutex::new(
            TriggerEventSpill::open(&mut budget, &path, spill_bytes).unwrap(),
        ));
        let mut queue = TriggerEventQueue::new(&mut budget, TRIGGER_EVENT_PAGE_BYTES, 8).unwrap();
        let image = [0x5a_u8; 3000];
        let mut ids = [0u64; 100];
        // Without a spill file one page is the whole queue.
        let mut written = 0;
        let error = loop {
            match queue.push(&row_event(written as u16), None, Some(&image)) {
                Ok(id) => ids[written] = id,
                Err(error) => break error,
            }
            written += 1;
        };
        assert_eq!(error.sqlstate, sqlstate::PROGRAM_LIMIT_EXCEEDED);
        assert!(written < 30);
        queue.clear();
        queue.attach_spill(spill.clone());
        crate::mem::guard::forbid_alloc(|| {
            for (index, id) in ids.iter_mut().enumerate() {
                let mut event = row_event(index as u16);
                if index % 3 == 0 {
                    event.updated_columns = ColumnSet::from_column(index);
                }
                *id = queue
                    .push(&event, Some(&image[..index]), Some(&image))
                    .unwrap();
            }
            let mark = queue.mark();
            for id in ids.iter().step_by(2) {
                queue.fire(*id).unwrap();
            }
            for (index, id) in ids.iter().enumerate() {
                let (event, next, fired) = queue.event(*id).unwrap();
                assert_eq!(fired, index % 2 == 0);
                assert_eq!(
                    event.kind,
                    DeferredTriggerKind::AfterRow {
                        trigger_slot: index as u16,
                        effective_table: 3,
                        trigger_depth: 1,
                        command: 7,
                    }
                );
                let columns = if index % 3 == 0 {
                    ColumnSet::from_column(index)
                } else {
                    ColumnSet::EMPTY
                };
                assert_eq!(event.updated_columns, columns);
                let mut old = [0u8; 100];
                queue.tuple(event.old.unwrap(), &mut old[..index]).unwrap();
                assert!(old[..index].iter().all(|byte| *byte == 0x5a));
                assert_eq!(queue.is_fired(*id).unwrap(), index % 2 == 0);
                if index + 1 < ids.len() {
                    assert_eq!(next, ids[index + 1]);
                }
            }
            assert!(queue.has_waiting().unwrap());
            queue.rewind(mark);
            for id in &ids {
                assert!(!queue.is_fired(*id).unwrap());
            }
            queue.rewind(TriggerEventMark {
                len: ids[10],
                fired: 0,
            });
            assert_eq!(queue.end(), ids[10]);
            queue.clear();
            assert!(!queue.has_waiting().unwrap());
        });
        assert_eq!(spill.lock().unwrap().free.len(), 8);
        let _ = std::fs::remove_file(&path);
    }
}
