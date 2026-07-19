//! Write-ahead log: one preallocated journal file, TigerBeetle-style —
//! opened once at startup, so the post-freeze path never builds paths or
//! opens files. Records are CRC-32C-checksummed and strictly
//! LSN-increasing; recovery replays until the first invalid or
//! non-monotonic record, which also makes a recycled journal (after a
//! future checkpoint truncation) safe against stale tails.
//!
//! Durability: `commit` writes buffered records and issues
//! `fcntl(F_FULLFSYNC)` on macOS (plain fsync does not reach the platter
//! there). A failed durability write aborts the process: memory state is
//! already ahead of the journal, and restart-and-replay is the only
//! consistent recovery.

pub mod crc32c;

use std::fs::File;
use std::os::fd::AsRawFd;
use std::os::unix::fs::FileExt;

use crate::config::Config;
use crate::mem::budget::{Budget, BudgetError};
use crate::mem::buffer::FixedBuf;
use crate::sql::eval::SqlError;
use crate::sql::types::ColType;
use crate::sql_err;
use crate::storage::{
    CheckConstraint, ColumnMeta, FkAction, ForeignKey, OwnedDatum, SqlName, TableDef, UniqueKey,
    MAX_COLUMNS, MAX_INDEX_COLS,
};

use crc32c::{crc32c, Crc32c};

const HEADER_LEN: usize = 24;

const KIND_CREATE: u8 = 1;
const KIND_DROP: u8 = 2;
const KIND_UPSERT: u8 = 3;
const KIND_DELETE: u8 = 4;
const KIND_CREATE_VIEW: u8 = 5;
const KIND_DROP_VIEW: u8 = 6;
const KIND_CREATE_INDEX: u8 = 7;
const KIND_DROP_INDEX: u8 = 8;

/// SQLSTATE 53100 disk_full.
const JOURNAL_FULL: &str = "53100";

#[derive(Debug)]
#[expect(
    clippy::large_enum_variant,
    reason = "TableDef is a fixed inline array by design (no heap); WalOp lives briefly on the stack"
)]
pub enum WalOp<'a> {
    CreateTable(TableDef),
    DropTable(&'a str),
    Upsert {
        table: &'a str,
        rowid: u64,
        row: &'a [u8],
    },
    Delete {
        table: &'a str,
        rowid: u64,
    },
    CreateView {
        name: &'a str,
        sql: &'a str,
    },
    DropView(&'a str),
    CreateIndex {
        name: &'a str,
        table: &'a str,
        columns: [u16; MAX_INDEX_COLS],
        n_cols: usize,
        unique: bool,
    },
    DropIndex(&'a str),
}

pub struct Wal {
    file: File,
    buffer: FixedBuf,
    /// File offset where the next buffered byte lands.
    write_offset: u64,
    capacity: u64,
    last_lsn: u64,
    dirty: bool,
    /// First LSN of the batch currently buffered (for segment upload).
    batch_first_lsn: u64,
    /// Bytes appended since the last upload capture.
    batch_start_offset: u64,
}

#[derive(Debug)]
pub enum WalSetupError {
    Budget(BudgetError),
    Io(&'static str, std::io::Error),
    /// The journal on disk is larger than `wal_bytes` — refusing to
    /// truncate someone's log because a config shrank.
    ShrinkRefused { file: u64, config: u64 },
    Replay(SqlError),
}

impl std::fmt::Display for WalSetupError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Budget(e) => write!(f, "wal: {e}"),
            Self::Io(what, e) => write!(f, "wal: {what}: {e}"),
            Self::ShrinkRefused { file, config } => write!(
                f,
                "wal: journal is {file} bytes but wal_bytes is {config}; refusing to truncate"
            ),
            Self::Replay(e) => write!(f, "wal: replay failed: {}", e.message.as_str()),
        }
    }
}

impl std::error::Error for WalSetupError {}

impl From<BudgetError> for WalSetupError {
    fn from(e: BudgetError) -> Self {
        Self::Budget(e)
    }
}

impl Wal {
    /// Opens (creating and preallocating if needed) `<data_dir>/journal.wal`.
    pub fn open(config: &Config, budget: &mut Budget) -> Result<Self, WalSetupError> {
        std::fs::create_dir_all(&config.data_dir)
            .map_err(|e| WalSetupError::Io("create data_dir", e))?;
        let path = format!("{}/journal.wal", config.data_dir);
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&path)
            .map_err(|e| WalSetupError::Io("open journal", e))?;
        let len = file
            .metadata()
            .map_err(|e| WalSetupError::Io("stat journal", e))?
            .len();
        let capacity = config.wal_bytes as u64;
        if len > capacity {
            return Err(WalSetupError::ShrinkRefused {
                file: len,
                config: capacity,
            });
        }
        if len < capacity {
            file.set_len(capacity)
                .map_err(|e| WalSetupError::Io("preallocate journal", e))?;
        }
        Ok(Self {
            file,
            buffer: FixedBuf::new(budget, "wal_buffer", config.wal_buffer_bytes)?,
            write_offset: 0,
            capacity,
            last_lsn: 0,
            dirty: false,
            batch_first_lsn: 0,
            batch_start_offset: 0,
        })
    }

    pub fn last_lsn(&self) -> u64 {
        self.last_lsn
    }

    pub fn used_bytes(&self) -> u64 {
        self.write_offset + self.buffer.len() as u64
    }

    pub fn capacity_bytes(&self) -> u64 {
        self.capacity
    }

    /// Replays every valid record from the start of the journal, stopping
    /// at the first invalid or non-monotonic one (the tail). Positions the
    /// write cursor there. Records with `lsn <= floor` are scanned but not
    /// applied — they are already covered by the checkpoint the caller
    /// loaded (a crash between manifest publication and journal reset
    /// leaves such records behind). Startup only.
    pub fn replay(
        &mut self,
        floor: u64,
        mut apply: impl for<'a> FnMut(u64, WalOp<'a>) -> Result<(), SqlError>,
    ) -> Result<(), WalSetupError> {
        self.buffer.clear();
        let mut file_offset = 0u64; // next byte to read from the file
        'outer: loop {
            let space = self.buffer.writable();
            if space.is_empty() {
                // A record larger than the buffer can never be written by
                // append(), so this is corruption; stop here.
                break;
            }
            let want = space.len().min((self.capacity - file_offset) as usize);
            if want == 0 {
                break;
            }
            let n = self
                .file
                .read_at(&mut space[..want], file_offset)
                .map_err(|e| WalSetupError::Io("read journal", e))?;
            if n == 0 {
                break;
            }
            self.buffer.advance(n);
            file_offset += n as u64;

            loop {
                let data = self.buffer.readable();
                if data.len() < HEADER_LEN {
                    continue 'outer;
                }
                let stored_crc = u32::from_le_bytes(data[0..4].try_into().unwrap());
                let payload_len =
                    u32::from_le_bytes(data[4..8].try_into().unwrap()) as usize;
                let lsn = u64::from_le_bytes(data[8..16].try_into().unwrap());
                let kind = data[16];
                if !(KIND_CREATE..=KIND_DROP_INDEX).contains(&kind)
                    || payload_len > self.buffer.capacity() - HEADER_LEN
                    || lsn <= self.last_lsn
                {
                    break 'outer;
                }
                let total = HEADER_LEN + payload_len;
                if data.len() < total {
                    continue 'outer;
                }
                if crc32c(&data[4..total]) != stored_crc {
                    break 'outer;
                }
                let payload = &data[HEADER_LEN..total];
                let operation = match decode_op(kind, payload) {
                    Some(operation) => operation,
                    None => break 'outer,
                };
                if lsn > floor {
                    apply(lsn, operation).map_err(WalSetupError::Replay)?;
                }
                self.last_lsn = lsn;
                self.write_offset += total as u64;
                self.buffer.consume(total);
            }
        }
        self.buffer.clear();
        Ok(())
    }

    /// Byte position for [`Self::truncate_to_mark`]: everything appended
    /// after the mark can be dropped, which is how an aborted transaction
    /// guarantees none of its records ever reach the journal file.
    pub fn mark(&self) -> usize {
        self.buffer.mark()
    }

    pub fn truncate_to_mark(&mut self, mark: usize) {
        self.buffer.truncate_to(mark);
        self.dirty = !self.buffer.is_empty();
    }

    /// Appends one record to the in-memory batch. Never writes to the file:
    /// a batch either fits `wal_buffer_bytes` entirely or the transaction
    /// fails — so the journal only ever contains whole transactions.
    pub fn append(&mut self, lsn: u64, operation: &WalOp) -> Result<(), SqlError> {
        let payload_len = encoded_payload_len(operation);
        let total = HEADER_LEN + payload_len;
        if self.buffer.capacity() - self.buffer.len() < total {
            return Err(sql_err!(
                "54000",
                "transaction exceeds wal_buffer_bytes ({}); raise it or commit in smaller batches",
                self.buffer.capacity()
            ));
        }
        if self.write_offset + self.buffer.len() as u64 + total as u64 > self.capacity {
            return Err(sql_err!(
                JOURNAL_FULL,
                "WAL journal is full; run CHECKPOINT (or raise wal_bytes)"
            ));
        }
        debug_assert!(lsn > self.last_lsn, "LSNs must be strictly increasing");
        if self.batch_first_lsn == 0 {
            self.batch_first_lsn = lsn;
            // The file offset where this batch's first byte will land once the
            // buffer flushes — not the in-memory buffer mark. `read_range`
            // during upload reads from the file, so a buffer-relative offset
            // would re-read (and re-upload) the journal from its start.
            self.batch_start_offset = self.write_offset + self.buffer.len() as u64;
        }

        let mark = self.buffer.mark();
        let mut ok = self.buffer.append(&[0u8; 4]); // crc, patched below
        ok &= self.buffer.append(&(payload_len as u32).to_le_bytes());
        ok &= self.buffer.append(&lsn.to_le_bytes());
        ok &= self.buffer.append(&[op_kind(operation), 0, 0, 0, 0, 0, 0, 0]);
        ok &= append_payload(&mut self.buffer, operation);
        assert!(ok, "record size was checked against buffer capacity");

        let filled = self.buffer.filled_mut();
        let crc = {
            let mut c = Crc32c::new();
            c.update(&filled[mark + 4..mark + total]);
            c.finish()
        };
        filled[mark..mark + 4].copy_from_slice(&crc.to_le_bytes());
        self.last_lsn = lsn;
        self.dirty = true;
        Ok(())
    }

    /// The batch just committed: (first LSN, file byte range). Valid only
    /// immediately after commit(), before the next append.
    pub fn last_committed_batch(&self) -> Option<(u64, u64, u64)> {
        if self.batch_first_lsn == 0 {
            return None;
        }
        Some((self.batch_first_lsn, self.batch_start_offset, self.write_offset))
    }

    /// Bytes of committed-but-not-yet-uploaded WAL accumulated in the current
    /// upload batch (the marker is cleared once its bytes are uploaded). Zero
    /// when nothing awaits upload.
    pub fn pending_batch_bytes(&self) -> u64 {
        if self.batch_first_lsn == 0 {
            return 0;
        }
        self.write_offset.saturating_sub(self.batch_start_offset)
    }

    /// Reads `len` bytes at file `offset` into `out` (for segment upload).
    pub fn read_range(&self, offset: u64, out: &mut [u8]) -> std::io::Result<()> {
        use std::os::unix::fs::FileExt;
        self.file.read_exact_at(out, offset)
    }

    /// Makes everything appended so far durable. Aborts the process on I/O
    /// failure: the in-memory state is already ahead of the journal, and
    /// restart-with-replay is the only consistent way forward.
    pub fn commit(&mut self) {
        if !self.dirty {
            return;
        }
        self.flush_buffer();
        if !fsync_durable(self.file.as_raw_fd()) {
            die("pos3ql: WAL fsync failed; aborting for consistency\n");
        }
        self.dirty = false;
    }

    /// After a checkpoint made everything up to the current LSN durable in
    /// object storage, the journal restarts from the beginning. Stale bytes
    /// beyond the new tail are defused by the monotonic-LSN replay rule.
    pub fn reset_after_checkpoint(&mut self) {
        self.buffer.clear();
        self.write_offset = 0;
        self.dirty = false;
        self.batch_first_lsn = 0;
        self.batch_start_offset = 0;
    }

    /// Clears the current batch marker after its bytes were captured for
    /// upload, so the next transaction starts a fresh segment.
    pub fn clear_batch_marker(&mut self) {
        self.batch_first_lsn = 0;
    }

    fn flush_buffer(&mut self) {
        if self.buffer.is_empty() {
            return;
        }
        let data = self.buffer.readable();
        if self.file.write_all_at(data, self.write_offset).is_err() {
            die("pos3ql: WAL write failed; aborting for consistency\n");
        }
        self.write_offset += data.len() as u64;
        let n = data.len();
        self.buffer.consume(n);
    }
}

/// Durable sync: F_FULLFSYNC on macOS (plain fsync does not reach the
/// platter there), fdatasync on Linux, fsync elsewhere.
fn fsync_durable(fd: std::os::fd::RawFd) -> bool {
    #[cfg(target_os = "macos")]
    let rc = unsafe { libc::fcntl(fd, libc::F_FULLFSYNC, 0) };
    #[cfg(target_os = "linux")]
    let rc = unsafe { libc::fdatasync(fd) };
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    let rc = unsafe { libc::fsync(fd) };
    rc == 0
}

/// Post-freeze fatal path: raw write + abort (no allocating panic).
fn die(msg: &str) -> ! {
    unsafe {
        libc::write(2, msg.as_ptr().cast(), msg.len());
    }
    std::process::abort();
}

fn op_kind(operation: &WalOp) -> u8 {
    match operation {
        WalOp::CreateTable(_) => KIND_CREATE,
        WalOp::DropTable(_) => KIND_DROP,
        WalOp::Upsert { .. } => KIND_UPSERT,
        WalOp::Delete { .. } => KIND_DELETE,
        WalOp::CreateView { .. } => KIND_CREATE_VIEW,
        WalOp::DropView(_) => KIND_DROP_VIEW,
        WalOp::CreateIndex { .. } => KIND_CREATE_INDEX,
        WalOp::DropIndex(_) => KIND_DROP_INDEX,
    }
}

fn encoded_payload_len(operation: &WalOp) -> usize {
    match operation {
        WalOp::CreateTable(def) => {
            let mut n = 1 + def.name.as_str().len() + 2;
            for c in def.columns() {
                n += 1 + c.name.as_str().len() + 2 + 4 + encoded_default_len(&c.default_value);
            }
            // uniques
            n += 1;
            for uk in def.uniques() {
                n += 1 + uk.name.as_str().len() + 2 + uk.n_cols * 2;
            }
            // checks
            n += 1;
            for check in def.checks() {
                n += 1 + check.name.as_str().len() + 2 + check.expression.as_str().len();
            }
            // foreign keys
            n += 1;
            for fk in def.fkeys() {
                n += 1 + fk.name.as_str().len()
                    + 1 + fk.n_cols * 2
                    + 1 + fk.parent.as_str().len()
                    + 1 + fk.n_parent_cols * 2
                    + 2;
            }
            n
        }
        WalOp::DropTable(name) => 1 + name.len(),
        WalOp::Upsert { table, row, .. } => 1 + table.len() + 8 + 4 + row.len(),
        WalOp::Delete { table, .. } => 1 + table.len() + 8,
        WalOp::CreateView { name, sql } => 1 + name.len() + 2 + sql.len(),
        WalOp::DropView(name) => 1 + name.len(),
        WalOp::CreateIndex { name, table, n_cols, .. } => {
            1 + name.len() + 1 + table.len() + 1 + 1 + n_cols * 2
        }
        WalOp::DropIndex(name) => 1 + name.len(),
    }
}

fn append_payload(buffer: &mut FixedBuf, operation: &WalOp) -> bool {
    let name_bytes = |buffer: &mut FixedBuf, s: &str| -> bool {
        buffer.append(&[s.len() as u8]) && buffer.append(s.as_bytes())
    };
    match operation {
        WalOp::CreateTable(def) => {
            let mut ok = name_bytes(buffer, def.name.as_str());
            ok &= buffer.append(&(def.n_columns as u16).to_le_bytes());
            for c in def.columns() {
                ok &= name_bytes(buffer, c.name.as_str());
                let flags = u8::from(c.not_null)
                    | (u8::from(c.unique) << 1)
                    | (u8::from(c.primary) << 2)
                    | (u8::from(c.auto_increment) << 3);
                ok &= buffer.append(&[type_code(c.ctype), flags]);
                ok &= buffer.append(&c.type_mod.to_le_bytes());
                ok &= append_default(buffer, &c.default_value);
            }
            // Multi-column UNIQUE/PRIMARY KEY constraints.
            ok &= buffer.append(&[def.n_uniques as u8]);
            for uk in def.uniques() {
                ok &= name_bytes(buffer, uk.name.as_str());
                ok &= buffer.append(&[u8::from(uk.is_primary), uk.n_cols as u8]);
                for &c in uk.columns() {
                    ok &= buffer.append(&c.to_le_bytes());
                }
            }
            // CHECK constraints.
            ok &= buffer.append(&[def.n_checks as u8]);
            for check in def.checks() {
                ok &= name_bytes(buffer, check.name.as_str());
                let e = check.expression.as_str();
                ok &= buffer.append(&(e.len() as u16).to_le_bytes());
                ok &= buffer.append(e.as_bytes());
            }
            // FOREIGN KEY constraints.
            ok &= buffer.append(&[def.n_fkeys as u8]);
            for fk in def.fkeys() {
                ok &= name_bytes(buffer, fk.name.as_str());
                ok &= buffer.append(&[fk.n_cols as u8]);
                for &c in fk.columns() {
                    ok &= buffer.append(&c.to_le_bytes());
                }
                ok &= name_bytes(buffer, fk.parent.as_str());
                ok &= buffer.append(&[fk.n_parent_cols as u8]);
                for &c in fk.parent_cols() {
                    ok &= buffer.append(&c.to_le_bytes());
                }
                ok &= buffer.append(&[fk.on_delete.code(), fk.on_update.code()]);
            }
            ok
        }
        WalOp::DropTable(name) => name_bytes(buffer, name),
        WalOp::Upsert { table, rowid, row } => {
            name_bytes(buffer, table)
                && buffer.append(&rowid.to_le_bytes())
                && buffer.append(&(row.len() as u32).to_le_bytes())
                && buffer.append(row)
        }
        WalOp::Delete { table, rowid } => {
            name_bytes(buffer, table) && buffer.append(&rowid.to_le_bytes())
        }
        WalOp::CreateView { name, sql } => {
            name_bytes(buffer, name)
                && buffer.append(&(sql.len() as u16).to_le_bytes())
                && buffer.append(sql.as_bytes())
        }
        WalOp::DropView(name) => name_bytes(buffer, name),
        WalOp::CreateIndex { name, table, columns, n_cols, unique } => {
            let mut ok = name_bytes(buffer, name)
                && name_bytes(buffer, table)
                && buffer.append(&[u8::from(*unique), *n_cols as u8]);
            for c in &columns[..*n_cols] {
                ok &= buffer.append(&c.to_le_bytes());
            }
            ok
        }
        WalOp::DropIndex(name) => name_bytes(buffer, name),
    }
}

/// Decodes an uploaded-segment record starting at the kind byte. The
/// on-disk record header is `crc(4) len(4) lsn(8) kind(1) pad(7)`; callers
/// pass the slice from the kind byte onward, so the payload begins 8 bytes
/// in (kind + 7 pad), matching the local journal layout.
pub fn decode_record(record: &[u8]) -> Option<WalOp<'_>> {
    if record.len() < 8 {
        return None;
    }
    decode_op(record[0], &record[8..])
}

fn decode_op(kind: u8, payload: &[u8]) -> Option<WalOp<'_>> {
    let mut at = 0usize;
    let take_name = |at: &mut usize| -> Option<&str> {
        let len = *payload.get(*at)? as usize;
        *at += 1;
        let raw = payload.get(*at..*at + len)?;
        *at += len;
        core::str::from_utf8(raw).ok()
    };
    match kind {
        KIND_CREATE => {
            let name = take_name(&mut at)?;
            let n_cols =
                u16::from_le_bytes(payload.get(at..at + 2)?.try_into().unwrap()) as usize;
            at += 2;
            if n_cols > MAX_COLUMNS {
                return None;
            }
            let mut def = TableDef {
                name: SqlName::parse(name).ok()?,
                columns: [ColumnMeta {
                    name: SqlName::parse("").ok()?,
                    ctype: ColType::Bool,
                    type_mod: -1,
                    not_null: false,
                    unique: false,
                    primary: false,
                    auto_increment: false,
                    default_value: None,
                }; MAX_COLUMNS],
                n_columns: n_cols,
                ..TableDef::empty()
            };
            for i in 0..n_cols {
                let col_name = take_name(&mut at)?;
                let meta = payload.get(at..at + 2)?;
                at += 2;
                let type_mod = i32::from_le_bytes(payload.get(at..at + 4)?.try_into().unwrap());
                at += 4;
                let default_value = decode_default(payload, &mut at)?;
                def.columns[i] = ColumnMeta {
                    name: SqlName::parse(col_name).ok()?,
                    ctype: code_type(meta[0])?,
                    type_mod,
                    not_null: meta[1] & 1 != 0,
                    unique: meta[1] & 2 != 0,
                    primary: meta[1] & 4 != 0,
                    auto_increment: meta[1] & 8 != 0,
                    default_value,
                };
            }
            // Multi-column UNIQUE/PRIMARY KEY constraints.
            let n_uniques = *payload.get(at)? as usize;
            at += 1;
            if n_uniques > crate::storage::MAX_UNIQUES {
                return None;
            }
            def.n_uniques = n_uniques;
            for u in 0..n_uniques {
                let uname = take_name(&mut at)?;
                let meta = payload.get(at..at + 2)?;
                at += 2;
                let n = meta[1] as usize;
                if n > MAX_INDEX_COLS {
                    return None;
                }
                let mut uk = UniqueKey::EMPTY;
                uk.name = SqlName::parse(uname).ok()?;
                uk.is_primary = meta[0] != 0;
                uk.n_cols = n;
                for c in uk.columns.iter_mut().take(n) {
                    *c = u16::from_le_bytes(payload.get(at..at + 2)?.try_into().unwrap());
                    at += 2;
                }
                def.uniques[u] = uk;
            }
            // CHECK constraints.
            let n_checks = *payload.get(at)? as usize;
            at += 1;
            if n_checks > crate::storage::MAX_CHECKS {
                return None;
            }
            def.n_checks = n_checks;
            for k in 0..n_checks {
                let constraint_name = take_name(&mut at)?;
                let elen =
                    u16::from_le_bytes(payload.get(at..at + 2)?.try_into().unwrap()) as usize;
                at += 2;
                let raw = payload.get(at..at + elen)?;
                at += elen;
                let text = core::str::from_utf8(raw).ok()?;
                let mut check = CheckConstraint::EMPTY;
                check.name = SqlName::parse(constraint_name).ok()?;
                core::fmt::Write::write_str(&mut check.expression, text).ok()?;
                if check.expression.is_truncated() {
                    return None;
                }
                def.checks[k] = check;
            }
            // FOREIGN KEY constraints.
            let n_fkeys = *payload.get(at)? as usize;
            at += 1;
            if n_fkeys > crate::storage::MAX_FKEYS {
                return None;
            }
            def.n_fkeys = n_fkeys;
            for f in 0..n_fkeys {
                let fname = take_name(&mut at)?;
                let nc = *payload.get(at)? as usize;
                at += 1;
                if nc > MAX_INDEX_COLS {
                    return None;
                }
                let mut fk = ForeignKey::EMPTY;
                fk.name = SqlName::parse(fname).ok()?;
                fk.n_cols = nc;
                for c in fk.columns.iter_mut().take(nc) {
                    *c = u16::from_le_bytes(payload.get(at..at + 2)?.try_into().unwrap());
                    at += 2;
                }
                let parent_name = take_name(&mut at)?;
                fk.parent = SqlName::parse(parent_name).ok()?;
                let np = *payload.get(at)? as usize;
                at += 1;
                if np > MAX_INDEX_COLS {
                    return None;
                }
                fk.n_parent_cols = np;
                for c in fk.parent_cols.iter_mut().take(np) {
                    *c = u16::from_le_bytes(payload.get(at..at + 2)?.try_into().unwrap());
                    at += 2;
                }
                let acts = payload.get(at..at + 2)?;
                at += 2;
                fk.on_delete = FkAction::from_code(acts[0])?;
                fk.on_update = FkAction::from_code(acts[1])?;
                def.fkeys[f] = fk;
            }
            (at == payload.len()).then_some(WalOp::CreateTable(def))
        }
        KIND_DROP => {
            let name = take_name(&mut at)?;
            (at == payload.len()).then_some(WalOp::DropTable(name))
        }
        KIND_UPSERT => {
            let table = take_name(&mut at)?;
            let rowid = u64::from_le_bytes(payload.get(at..at + 8)?.try_into().unwrap());
            at += 8;
            let row_len =
                u32::from_le_bytes(payload.get(at..at + 4)?.try_into().unwrap()) as usize;
            at += 4;
            let row = payload.get(at..at + row_len)?;
            at += row_len;
            (at == payload.len()).then_some(WalOp::Upsert { table, rowid, row })
        }
        KIND_DELETE => {
            let table = take_name(&mut at)?;
            let rowid = u64::from_le_bytes(payload.get(at..at + 8)?.try_into().unwrap());
            at += 8;
            (at == payload.len()).then_some(WalOp::Delete { table, rowid })
        }
        KIND_CREATE_VIEW => {
            let name = take_name(&mut at)?;
            let sql_len =
                u16::from_le_bytes(payload.get(at..at + 2)?.try_into().unwrap()) as usize;
            at += 2;
            let raw = payload.get(at..at + sql_len)?;
            at += sql_len;
            let sql = core::str::from_utf8(raw).ok()?;
            (at == payload.len()).then_some(WalOp::CreateView { name, sql })
        }
        KIND_DROP_VIEW => {
            let name = take_name(&mut at)?;
            (at == payload.len()).then_some(WalOp::DropView(name))
        }
        KIND_CREATE_INDEX => {
            let name = take_name(&mut at)?;
            let table = take_name(&mut at)?;
            let unique = *payload.get(at)? != 0;
            at += 1;
            let n_cols = *payload.get(at)? as usize;
            at += 1;
            if n_cols > MAX_INDEX_COLS {
                return None;
            }
            let mut columns = [0u16; MAX_INDEX_COLS];
            for c in columns.iter_mut().take(n_cols) {
                *c = u16::from_le_bytes(payload.get(at..at + 2)?.try_into().unwrap());
                at += 2;
            }
            (at == payload.len()).then_some(WalOp::CreateIndex {
                name,
                table,
                columns,
                n_cols,
                unique,
            })
        }
        KIND_DROP_INDEX => {
            let name = take_name(&mut at)?;
            (at == payload.len()).then_some(WalOp::DropIndex(name))
        }
        _ => None,
    }
}

pub fn encoded_default_len(d: &Option<OwnedDatum>) -> usize {
    1 + match d {
        None | Some(OwnedDatum::Null) => 0,
        Some(OwnedDatum::Bool(_)) => 1,
        Some(OwnedDatum::Int4(_)) => 4,
        Some(OwnedDatum::Int8(_)) | Some(OwnedDatum::Float8(_)) => 8,
        Some(OwnedDatum::Text { len, .. }) => 1 + *len as usize,
        Some(OwnedDatum::Numeric { nbytes, .. }) => 6 + *nbytes as usize,
    }
}

pub fn append_default(buffer: &mut FixedBuf, d: &Option<OwnedDatum>) -> bool {
    let mut scratch = [0u8; MAX_DEFAULT_ENCODED];
    let n = encode_default_bytes(d, &mut scratch);
    buffer.append(&scratch[..n])
}

/// Largest encoded default: tag + len byte + 48 text bytes.
pub const MAX_DEFAULT_ENCODED: usize = 7 + crate::storage::MAX_DEFAULT_TEXT;

/// Stack encoding of a column default; returns the byte count.
pub fn encode_default_bytes(d: &Option<OwnedDatum>, out: &mut [u8]) -> usize {
    match d {
        None => {
            out[0] = 0;
            1
        }
        Some(OwnedDatum::Null) => {
            out[0] = 1;
            1
        }
        Some(OwnedDatum::Bool(b)) => {
            out[0] = 2;
            out[1] = u8::from(*b);
            2
        }
        Some(OwnedDatum::Int4(v)) => {
            out[0] = 3;
            out[1..5].copy_from_slice(&v.to_le_bytes());
            5
        }
        Some(OwnedDatum::Int8(v)) => {
            out[0] = 4;
            out[1..9].copy_from_slice(&v.to_le_bytes());
            9
        }
        Some(OwnedDatum::Float8(v)) => {
            out[0] = 5;
            out[1..9].copy_from_slice(&v.to_le_bytes());
            9
        }
        Some(OwnedDatum::Text { len, bytes }) => {
            out[0] = 6;
            out[1] = *len;
            out[2..2 + *len as usize].copy_from_slice(&bytes[..*len as usize]);
            2 + *len as usize
        }
        Some(OwnedDatum::Numeric { sign, weight, dscale, nbytes, digits }) => {
            out[0] = 7;
            out[1] = *sign;
            out[2..4].copy_from_slice(&weight.to_le_bytes());
            out[4..6].copy_from_slice(&dscale.to_le_bytes());
            out[6] = *nbytes;
            out[7..7 + *nbytes as usize].copy_from_slice(&digits[..*nbytes as usize]);
            7 + *nbytes as usize
        }
    }
}

/// Also used by the manifest codec.
pub fn decode_default(payload: &[u8], at: &mut usize) -> Option<Option<OwnedDatum>> {
    let tag = *payload.get(*at)?;
    *at += 1;
    Some(match tag {
        0 => None,
        1 => Some(OwnedDatum::Null),
        2 => {
            let b = *payload.get(*at)?;
            *at += 1;
            Some(OwnedDatum::Bool(b != 0))
        }
        3 => {
            let b = payload.get(*at..*at + 4)?;
            *at += 4;
            Some(OwnedDatum::Int4(i32::from_le_bytes(b.try_into().unwrap())))
        }
        4 => {
            let b = payload.get(*at..*at + 8)?;
            *at += 8;
            Some(OwnedDatum::Int8(i64::from_le_bytes(b.try_into().unwrap())))
        }
        5 => {
            let b = payload.get(*at..*at + 8)?;
            *at += 8;
            Some(OwnedDatum::Float8(f64::from_le_bytes(b.try_into().unwrap())))
        }
        6 => {
            let len = *payload.get(*at)? as usize;
            *at += 1;
            if len > crate::storage::MAX_DEFAULT_TEXT {
                return None;
            }
            let raw = payload.get(*at..*at + len)?;
            *at += len;
            core::str::from_utf8(raw).ok()?;
            let mut bytes = [0u8; crate::storage::MAX_DEFAULT_TEXT];
            bytes[..len].copy_from_slice(raw);
            Some(OwnedDatum::Text { len: len as u8, bytes })
        }
        7 => {
            let sign = *payload.get(*at)?;
            let weight = i16::from_le_bytes(payload.get(*at + 1..*at + 3)?.try_into().unwrap());
            let dscale = u16::from_le_bytes(payload.get(*at + 3..*at + 5)?.try_into().unwrap());
            let nbytes = *payload.get(*at + 5)? as usize;
            *at += 6;
            if nbytes > crate::storage::MAX_DEFAULT_TEXT {
                return None;
            }
            let raw = payload.get(*at..*at + nbytes)?;
            *at += nbytes;
            let mut digits = [0u8; crate::storage::MAX_DEFAULT_TEXT];
            digits[..nbytes].copy_from_slice(raw);
            Some(OwnedDatum::Numeric { sign, weight, dscale, nbytes: nbytes as u8, digits })
        }
        _ => return None,
    })
}

fn type_code(t: ColType) -> u8 {
    match t {
        ColType::Bool => 1,
        ColType::Int2 => 12,
        ColType::Float4 => 13,
        ColType::Varchar => 14,
        ColType::Bpchar => 15,
        ColType::Int4 => 2,
        ColType::Int8 => 3,
        ColType::Float8 => 4,
        ColType::Text => 5,
        ColType::Date => 6,
        ColType::Timestamp => 7,
        ColType::Timestamptz => 8,
        ColType::Time => 16,
        ColType::Interval => 17,
        ColType::Json => 18,
        ColType::Jsonb => 19,
        ColType::Array(e) => 32 + e.code(),
        ColType::Uuid => 9,
        ColType::Bytea => 10,
        ColType::Numeric => 11,
        ColType::Range(k) => 20 + k.code(),
    }
}

fn code_type(code: u8) -> Option<ColType> {
    Some(match code {
        1 => ColType::Bool,
        12 => ColType::Int2,
        13 => ColType::Float4,
        14 => ColType::Varchar,
        15 => ColType::Bpchar,
        2 => ColType::Int4,
        3 => ColType::Int8,
        4 => ColType::Float8,
        5 => ColType::Text,
        6 => ColType::Date,
        7 => ColType::Timestamp,
        8 => ColType::Timestamptz,
        16 => ColType::Time,
        17 => ColType::Interval,
        18 => ColType::Json,
        19 => ColType::Jsonb,
        c if (20..26).contains(&c) => ColType::Range(crate::sql::types::RangeKind::from_code(c - 20)),
        c if c >= 32 => ColType::Array(crate::sql::types::ArrElem::from_code(c - 32)?),
        9 => ColType::Uuid,
        10 => ColType::Bytea,
        11 => ColType::Numeric,
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_config(dir: &str) -> Config {
        let mut c = Config::default_dev();
        c.data_dir = dir.to_string();
        c.wal_bytes = 1 << 16;
        c.wal_buffer_bytes = 1 << 12;
        c
    }

    fn temp_dir(name: &str) -> String {
        let dir = std::env::temp_dir().join(format!(
            "pos3ql-wal-{}-{}",
            std::process::id(),
            name
        ));
        let _ = std::fs::remove_dir_all(&dir);
        dir.to_str().unwrap().to_string()
    }

    fn sample_def() -> TableDef {
        let mut def = TableDef {
            name: SqlName::parse("t").unwrap(),
            columns: [ColumnMeta {
                name: SqlName::parse("").unwrap(),
                ctype: ColType::Bool,
                type_mod: -1,
                not_null: false,
                unique: false,
                primary: false,
                auto_increment: false,
                default_value: None,
            }; MAX_COLUMNS],
            n_columns: 2,
            ..TableDef::empty()
        };
        def.columns[0] = ColumnMeta {
            name: SqlName::parse("id").unwrap(),
            ctype: ColType::Int4,
            type_mod: -1,
            not_null: true,
            unique: true,
            primary: true,
            auto_increment: false,
            default_value: None,
        };
        def.columns[1] = ColumnMeta {
            name: SqlName::parse("v").unwrap(),
            ctype: ColType::Text,
            type_mod: -1,
            not_null: false,
            unique: false,
            primary: false,
            auto_increment: false,
            default_value: Some(OwnedDatum::Int4(7)),
        };
        // A multi-column UNIQUE, a CHECK, and a FOREIGN KEY, so the WAL
        // round-trip covers every constraint kind.
        let mut uk = UniqueKey::EMPTY;
        uk.name = SqlName::parse("t_id_v_key").unwrap();
        uk.columns[0] = 0;
        uk.columns[1] = 1;
        uk.n_cols = 2;
        def.uniques[0] = uk;
        def.n_uniques = 1;
        let mut check = CheckConstraint::EMPTY;
        check.name = SqlName::parse("t_check").unwrap();
        core::fmt::Write::write_str(&mut check.expression, "id > 0").unwrap();
        def.checks[0] = check;
        def.n_checks = 1;
        let mut fk = ForeignKey::EMPTY;
        fk.name = SqlName::parse("t_id_fkey").unwrap();
        fk.columns[0] = 0;
        fk.n_cols = 1;
        fk.parent = SqlName::parse("parent").unwrap();
        fk.parent_cols[0] = 3;
        fk.n_parent_cols = 1;
        fk.on_delete = FkAction::Restrict;
        def.fkeys[0] = fk;
        def.n_fkeys = 1;
        def
    }

    fn collect_replay(wal: &mut Wal) -> Vec<String> {
        collect_replay_from(wal, 0)
    }

    fn collect_replay_from(wal: &mut Wal, floor: u64) -> Vec<String> {
        let mut seen = Vec::new();
        wal.replay(floor, |lsn, operation| {
            seen.push(format!("{lsn}:{operation:?}"));
            Ok(())
        })
        .unwrap();
        seen
    }

    #[test]
    fn roundtrip_all_ops() {
        let dir = temp_dir("roundtrip");
        let config = test_config(&dir);
        let mut budget = Budget::new(1 << 20);
        {
            let mut wal = Wal::open(&config, &mut budget).unwrap();
            wal.append(1, &WalOp::CreateTable(sample_def())).unwrap();
            wal.append(
                2,
                &WalOp::Upsert {
                    table: "t",
                    rowid: 1,
                    row: b"ROWBYTES",
                },
            )
            .unwrap();
            wal.append(3, &WalOp::Delete { table: "t", rowid: 1 }).unwrap();
            wal.append(4, &WalOp::DropTable("t")).unwrap();
            wal.commit();
        }
        let mut budget2 = Budget::new(1 << 20);
        let mut wal = Wal::open(&config, &mut budget2).unwrap();
        let seen = collect_replay(&mut wal);
        assert_eq!(seen.len(), 4);
        assert!(seen[0].starts_with("1:CreateTable"));
        // Constraints survive the encode/replay round-trip.
        assert!(seen[0].contains("t_id_v_key"), "unique key: {}", seen[0]);
        assert!(seen[0].contains("t_check"), "check: {}", seen[0]);
        assert!(seen[0].contains("t_id_fkey") && seen[0].contains("parent"), "fkey: {}", seen[0]);
        assert!(seen[1].contains("rowid: 1"));
        assert!(seen[3].starts_with("4:DropTable"));
        assert_eq!(wal.last_lsn(), 4);
        // Appending continues after the replayed tail.
        wal.append(5, &WalOp::DropTable("u")).unwrap();
        wal.commit();
    }

    #[test]
    fn corrupt_record_truncates_tail() {
        let dir = temp_dir("corrupt");
        let config = test_config(&dir);
        let mut budget = Budget::new(1 << 20);
        {
            let mut wal = Wal::open(&config, &mut budget).unwrap();
            for lsn in 1..=3 {
                wal.append(lsn, &WalOp::Delete { table: "t", rowid: lsn })
                    .unwrap();
            }
            wal.commit();
        }
        // Flip one byte in the second record's payload.
        let path = format!("{dir}/journal.wal");
        let mut bytes = std::fs::read(&path).unwrap();
        let record_len = HEADER_LEN + encoded_payload_len(&WalOp::Delete { table: "t", rowid: 1 });
        bytes[record_len + HEADER_LEN] ^= 0xff;
        std::fs::write(&path, &bytes).unwrap();

        let mut budget2 = Budget::new(1 << 20);
        let mut wal = Wal::open(&config, &mut budget2).unwrap();
        let seen = collect_replay(&mut wal);
        assert_eq!(seen.len(), 1, "only the record before the corruption survives");
    }

    #[test]
    fn replay_floor_skips_checkpointed_records() {
        let dir = temp_dir("floor");
        let config = test_config(&dir);
        let mut budget = Budget::new(1 << 20);
        {
            let mut wal = Wal::open(&config, &mut budget).unwrap();
            for lsn in 1..=5 {
                wal.append(lsn, &WalOp::Delete { table: "t", rowid: lsn }).unwrap();
            }
            wal.commit();
        }
        let mut budget2 = Budget::new(1 << 20);
        let mut wal = Wal::open(&config, &mut budget2).unwrap();
        let seen = collect_replay_from(&mut wal, 3);
        assert_eq!(seen.len(), 2, "only records above the floor apply");
        assert!(seen[0].starts_with("4:"));
        assert_eq!(wal.last_lsn(), 5, "scan still tracks the true tail");
    }

    #[test]
    fn reset_after_checkpoint_defuses_stale_tail() {
        let dir = temp_dir("reset");
        let config = test_config(&dir);
        let mut budget = Budget::new(1 << 20);
        {
            let mut wal = Wal::open(&config, &mut budget).unwrap();
            for lsn in 1..=10 {
                wal.append(lsn, &WalOp::Delete { table: "t", rowid: lsn }).unwrap();
            }
            wal.commit();
            // Checkpoint at lsn 10; journal restarts with two tail records.
            wal.reset_after_checkpoint();
            wal.append(11, &WalOp::Delete { table: "t", rowid: 11 }).unwrap();
            wal.append(12, &WalOp::Delete { table: "t", rowid: 12 }).unwrap();
            wal.commit();
        }
        let mut budget2 = Budget::new(1 << 20);
        let mut wal = Wal::open(&config, &mut budget2).unwrap();
        // The checkpoint says floor = 10; stale records 3..10 still sit in
        // the file beyond the new tail but must not replay.
        let seen = collect_replay_from(&mut wal, 10);
        assert_eq!(seen.len(), 2);
        assert!(seen[0].starts_with("11:"));
        assert!(seen[1].starts_with("12:"));
    }

    #[test]
    fn journal_full_is_a_clean_error() {
        let dir = temp_dir("full");
        let mut config = test_config(&dir);
        config.wal_bytes = 256;
        let mut budget = Budget::new(1 << 20);
        let mut wal = Wal::open(&config, &mut budget).unwrap();
        let mut lsn = 0;
        let err = loop {
            lsn += 1;
            match wal.append(
                lsn,
                &WalOp::Upsert {
                    table: "t",
                    rowid: lsn,
                    row: &[0u8; 32],
                },
            ) {
                Ok(()) => {}
                Err(e) => break e,
            }
        };
        assert_eq!(err.sqlstate, "53100");
        wal.commit();
    }

    #[test]
    fn oversized_record_is_rejected() {
        let dir = temp_dir("oversized");
        let mut config = test_config(&dir);
        config.wal_buffer_bytes = 128;
        let mut budget = Budget::new(1 << 20);
        let mut wal = Wal::open(&config, &mut budget).unwrap();
        let big = [0u8; 256];
        let err = wal
            .append(1, &WalOp::Upsert { table: "t", rowid: 1, row: &big })
            .unwrap_err();
        assert_eq!(err.sqlstate, "54000");
    }

    #[test]
    fn append_does_not_allocate() {
        let dir = temp_dir("noalloc");
        let config = test_config(&dir);
        let mut budget = Budget::new(1 << 20);
        let mut wal = Wal::open(&config, &mut budget).unwrap();
        crate::mem::guard::forbid_alloc(|| {
            for lsn in 1..=16 {
                wal.append(lsn, &WalOp::Delete { table: "t", rowid: lsn })
                    .unwrap();
            }
        });
        wal.commit();
    }
}
