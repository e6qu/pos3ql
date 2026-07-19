//! Checkpointing: the durable home of the database is the bucket.
//!
//! A checkpoint snapshots every live table into an SST object
//! (`sst/<lsn>/<slot>.sst`), then publishes a `manifest` object naming them
//! via compare-and-swap (`If-Match` on the previous ETag, `If-None-Match: *`
//! for the first). After the manifest lands, superseded SSTs are deleted,
//! the WAL restarts, and the row heap is compacted. A node with an empty
//! disk cold-starts by loading the manifest, rehydrating SSTs, and
//! replaying whatever WAL tail is newer than the manifest's LSN.
//!
//! CAS on the manifest means a second writer pointed at the same bucket
//! fails loudly instead of corrupting anything.

use std::io::Write as IoWrite;

use crate::config::Config;
use crate::mem::budget::{Budget, BudgetError};
use crate::mem::buf::FixedBuf;
use crate::mem::fixed_vec::FixedVec;
use crate::s3::sha256::{HexDigest, Sha256};
use crate::s3::{Precondition, S3Client, S3Error};
use crate::sql::eval::{sqlstate, SqlError};
use crate::sql::types::ColType;
use crate::sql_err;
use crate::stack_format;
use crate::storage::{ColumnMeta, OwnedDatum, RowLoc, SqlName, Storage, TableDef, MAX_COLUMNS};
use crate::util::StackStr;
use crate::wal::crc32c::Crc32c;

pub const MANIFEST_KEY: &str = "manifest";
const MANIFEST_HEADER: &str = "pos3ql-manifest-v2";
const MANIFEST_BUF_BYTES: usize = 256 * 1024;
const SST_MAGIC: u64 = 0x3154_5353_4c51_3350; // "P3QLSST1" little-endian
const SST_FOOTER_LEN: usize = 20; // count u64 | crc u32 | magic u64
const SST_ENTRY_HEADER: usize = 12; // rowid u64 | len u32

/// io_error — object storage trouble surfaced to a statement.
const SQLSTATE_IO: &str = "58030";
/// serialization_failure — manifest CAS lost to another writer.
const SQLSTATE_CAS: &str = "40001";

/// A prior checkpoint's SST reference for one table slot.
#[derive(Clone, Copy)]
struct PrevSst {
    key: StackStr<64>,
    count: u64,
    bytes: u64,
    crc: u32,
}

pub struct Checkpointer {
    client: S3Client,
    manifest_buf: FixedBuf,
    manifest_etag: Option<StackStr<80>>,
    manifest_lsn: u64,
    /// Per-slot SST from the last published manifest; clean tables reuse
    /// these keys (delta checkpoints). Capacity is reserved at startup so
    /// the post-freeze checkpoint path never allocates.
    prev_ssts: Vec<Option<PrevSst>>,
    /// Keys referenced by the manifest just published (GC keep-set).
    referenced: Vec<StackStr<64>>,
    /// Pre-reserved scratch built during a checkpoint, then swapped into the
    /// fields above; keeps the post-freeze path allocation-free.
    prev_scratch: Vec<Option<PrevSst>>,
    ref_scratch: Vec<StackStr<64>>,
    /// Pre-reserved scratch for GC / WAL-segment sweeps.
    doomed_scratch: Vec<StackStr<64>>,
}

/// Upper bounds reserved at startup so checkpoint-time bookkeeping never
/// touches the allocator. A sweep that would exceed these logs and defers
/// the remainder to the next checkpoint.
const MAX_CKPT_TABLES: usize = 1024;
const MAX_SWEEP_KEYS: usize = 4096;

impl Checkpointer {
    pub fn budget_bytes(config: &Config) -> usize {
        S3Client::budget_bytes(config) + MANIFEST_BUF_BYTES
    }

    /// Fails when S3 is enabled but credentials are missing — explicitly,
    /// at startup.
    pub fn new(config: &Config, budget: &mut Budget) -> Result<Self, CheckpointSetupError> {
        let mut config = config.clone();
        if config.s3_access_key.is_empty() {
            config.s3_access_key = std::env::var("AWS_ACCESS_KEY_ID").map_err(|_| {
                CheckpointSetupError::Credentials("s3_access_key / AWS_ACCESS_KEY_ID")
            })?;
        }
        if config.s3_secret_key.is_empty() {
            config.s3_secret_key = std::env::var("AWS_SECRET_ACCESS_KEY").map_err(|_| {
                CheckpointSetupError::Credentials("s3_secret_key / AWS_SECRET_ACCESS_KEY")
            })?;
        }
        Ok(Self {
            client: S3Client::new(&config, budget)
                .map_err(|e| CheckpointSetupError::S3(e.to_string()))?,
            manifest_buf: FixedBuf::new(budget, "manifest_buf", MANIFEST_BUF_BYTES)
                .map_err(CheckpointSetupError::Budget)?,
            manifest_etag: None,
            manifest_lsn: 0,
            prev_ssts: Vec::with_capacity(MAX_CKPT_TABLES),
            referenced: Vec::with_capacity(MAX_CKPT_TABLES),
            prev_scratch: Vec::with_capacity(MAX_CKPT_TABLES),
            ref_scratch: Vec::with_capacity(MAX_CKPT_TABLES),
            doomed_scratch: Vec::with_capacity(MAX_SWEEP_KEYS),
        })
    }

    pub fn manifest_lsn(&self) -> u64 {
        self.manifest_lsn
    }

    /// Uploads a committed WAL batch as a segment keyed by its first LSN,
    /// so a lost-disk cold start can replay everything past the manifest.
    /// Called with the raw journal bytes of one commit.
    pub fn upload_wal_segment(&mut self, first_lsn: u64, bytes: &[u8]) -> Result<(), SqlError> {
        let key = stack_format!(48, "wal/{:020}.seg", first_lsn);
        self.client
            .put(key.as_str(), bytes, Precondition::None)
            .map_err(s3_to_sql)?;
        Ok(())
    }

    /// Downloads and replays WAL segments with a first-LSN strictly greater
    /// than `floor`, in ascending order, feeding each record to `apply`.
    /// Startup only (allocates while listing/parsing).
    pub fn replay_wal_segments(
        &mut self,
        floor: u64,
        mut apply: impl FnMut(u64, &[u8]) -> Result<(), SqlError>,
    ) -> Result<u64, CheckpointSetupError> {
        let mut keys: Vec<String> = Vec::new();
        self.client
            .list("wal/", |k| keys.push(k.to_string()))
            .map_err(|e| CheckpointSetupError::S3(format!("list wal: {e}")))?;
        keys.sort();
        let mut last_lsn = floor;
        for key in &keys {
            // Key is wal/<20-digit first lsn>.seg
            let Some(digits) = key.strip_prefix("wal/").and_then(|k| k.strip_suffix(".seg"))
            else {
                continue;
            };
            let Ok(_first_lsn) = digits.parse::<u64>() else {
                continue;
            };
            self.client
                .get(key, None)
                .map_err(|e| CheckpointSetupError::S3(format!("get wal segment: {e}")))?;
            let body = self.client.body_bytes();
            // Records are the same framed format as the local journal.
            let n = replay_segment_bytes(body, floor, &mut apply)
                .map_err(CheckpointSetupError::Replay)?;
            if n > last_lsn {
                last_lsn = n;
            }
        }
        Ok(last_lsn)
    }

    /// Deletes uploaded WAL segments whose records are entirely covered by
    /// the current manifest LSN. Called after a checkpoint.
    pub fn prune_wal_segments(&mut self, up_to_lsn: u64) -> Result<(), SqlError> {
        // Two passes because list borrows the client: collect keys into
        // pre-reserved scratch (no allocation post-freeze — this runs inside a
        // checkpoint). Keep the highest-keyed doomed segment so one straddling
        // the checkpoint boundary is never lost.
        self.doomed_scratch.clear();
        let doomed = &mut self.doomed_scratch;
        let mut overflow = false;
        let mut max_key = StackStr::<64>::new();
        self.client
            .list("wal/", |k| {
                let is_doomed = k
                    .strip_prefix("wal/")
                    .and_then(|x| x.strip_suffix(".seg"))
                    .and_then(|d| d.parse::<u64>().ok())
                    .is_some_and(|first| first <= up_to_lsn);
                if is_doomed {
                    if k > max_key.as_str() {
                        max_key = crate::stack_format!(64, "{}", k);
                    }
                    if doomed.len() < MAX_SWEEP_KEYS {
                        doomed.push(crate::stack_format!(64, "{}", k));
                    } else {
                        overflow = true;
                    }
                }
            })
            .map_err(s3_to_sql)?;
        for i in 0..self.doomed_scratch.len() {
            let key = self.doomed_scratch[i];
            if key.as_str() == max_key.as_str() {
                continue;
            }
            self.client.delete(key.as_str()).map_err(s3_to_sql)?;
        }
        if overflow {
            eprintln!("pos3ql: wal segments exceed one sweep; continuing next checkpoint");
        }
        Ok(())
    }

    /// Cold start: loads the manifest (if any) and rehydrates every SST
    /// into storage. Returns the manifest LSN — the WAL replay floor.
    /// Startup only (allocates freely while parsing).
    pub fn load_into(&mut self, storage: &mut Storage) -> Result<u64, CheckpointSetupError> {
        match self.client.get(MANIFEST_KEY, None) {
            Ok(r) => {
                self.manifest_etag = Some(r.etag);
            }
            Err(e) if e.is_not_found() => return Ok(0),
            Err(e) => return Err(CheckpointSetupError::S3(format!("load manifest: {e}"))),
        }
        let text = core::str::from_utf8(self.client.body_bytes())
            .map_err(|_| CheckpointSetupError::Corrupt("manifest is not UTF-8"))?
            .to_string();

        let mut lines = text.lines();
        if lines.next() != Some(MANIFEST_HEADER) {
            return Err(CheckpointSetupError::Corrupt("bad manifest header"));
        }
        let mut lsn = 0u64;
        let mut next_rowid = 1u64;
        // manifest table index → live slot index
        let mut slot_of: Vec<Option<usize>> = Vec::new();
        let mut pending_def: Option<(usize, TableDef, usize)> = None; // (mindex, def, cols_seen)
        let mut ssts: Vec<(String, usize, u64, u64, u32)> = Vec::new();
        let mut saw_end = false;

        let finish_pending = |storage: &mut Storage,
                              slot_of: &mut Vec<Option<usize>>,
                              pending: Option<(usize, TableDef, usize)>|
         -> Result<(), CheckpointSetupError> {
            if let Some((mindex, def, seen)) = pending {
                if seen != def.n_columns {
                    return Err(CheckpointSetupError::Corrupt("manifest column count mismatch"));
                }
                let slot = storage
                    .create_table(def)
                    .map_err(|e| CheckpointSetupError::S3(format!(
                        "manifest table rejected: {}",
                        e.message.as_str()
                    )))?;
                if slot_of.len() <= mindex {
                    slot_of.resize(mindex + 1, None);
                }
                slot_of[mindex] = Some(slot);
            }
            Ok(())
        };

        for line in lines {
            let mut words = line.split(' ');
            match words.next() {
                Some("lsn") => {
                    lsn = parse_field(words.next(), "lsn")?;
                }
                Some("next_rowid") => {
                    next_rowid = parse_field(words.next(), "next_rowid")?;
                }
                Some("table") => {
                    finish_pending(storage, &mut slot_of, pending_def.take())?;
                    let mindex: usize = parse_field(words.next(), "table index")?;
                    let n_cols: usize = parse_field(words.next(), "table columns")?;
                    if n_cols > MAX_COLUMNS {
                        return Err(CheckpointSetupError::Corrupt("too many columns"));
                    }
                    let name = rest_of(line, 3)?;
                    let def = TableDef {
                        name: sql_name(name)?,
                        columns: [empty_column(); MAX_COLUMNS],
                        n_columns: n_cols,
                        ..TableDef::empty()
                    };
                    pending_def = Some((mindex, def, 0));
                }
                Some("col") => {
                    let Some((_, def, seen)) = pending_def.as_mut() else {
                        return Err(CheckpointSetupError::Corrupt("col outside table"));
                    };
                    let type_code: u8 = parse_field(words.next(), "col type")?;
                    let not_null: u8 = parse_field(words.next(), "col notnull")?;
                    let type_mod: i32 = parse_field(words.next(), "col typmod")?;
                    let default_hex = words
                        .next()
                        .ok_or(CheckpointSetupError::Corrupt("col default missing"))?;
                    let name = rest_of(line, 5)?;
                    if *seen >= def.n_columns {
                        return Err(CheckpointSetupError::Corrupt("too many col lines"));
                    }
                    def.columns[*seen] = ColumnMeta {
                        name: sql_name(name)?,
                        ctype: col_type_from_code(type_code)?,
                        type_mod,
                        not_null: not_null & 1 != 0,
                        unique: not_null & 2 != 0,
                        primary: not_null & 4 != 0,
                        auto_increment: not_null & 8 != 0,
                        default_value: default_from_hex(default_hex)?,
                    };
                    *seen += 1;
                }
                Some("ukey") => {
                    let Some((_, def, _)) = pending_def.as_mut() else {
                        return Err(CheckpointSetupError::Corrupt("ukey outside table"));
                    };
                    if def.n_uniques >= crate::storage::MAX_UNIQUES {
                        return Err(CheckpointSetupError::Corrupt("too many ukey lines"));
                    }
                    let is_primary: u8 = parse_field(words.next(), "ukey primary")?;
                    let n_cols: usize = parse_field(words.next(), "ukey ncols")?;
                    if n_cols == 0 || n_cols > crate::storage::MAX_INDEX_COLS {
                        return Err(CheckpointSetupError::Corrupt("bad ukey ncols"));
                    }
                    let mut uk = crate::storage::UniqueKey::EMPTY;
                    uk.is_primary = is_primary != 0;
                    uk.n_cols = n_cols;
                    for c in uk.cols.iter_mut().take(n_cols) {
                        *c = parse_field(words.next(), "ukey col")?;
                    }
                    let hname = words
                        .next()
                        .ok_or(CheckpointSetupError::Corrupt("ukey name missing"))?;
                    uk.name = sql_name(&decode_hex_name(hname)?)?;
                    let i = def.n_uniques;
                    def.uniques[i] = uk;
                    def.n_uniques += 1;
                }
                Some("chk") => {
                    let Some((_, def, _)) = pending_def.as_mut() else {
                        return Err(CheckpointSetupError::Corrupt("chk outside table"));
                    };
                    if def.n_checks >= crate::storage::MAX_CHECKS {
                        return Err(CheckpointSetupError::Corrupt("too many chk lines"));
                    }
                    let hname = words
                        .next()
                        .ok_or(CheckpointSetupError::Corrupt("chk name missing"))?;
                    let hexpr = words
                        .next()
                        .ok_or(CheckpointSetupError::Corrupt("chk expr missing"))?;
                    let mut check = crate::storage::CheckConstraint::EMPTY;
                    check.name = sql_name(&decode_hex_name(hname)?)?;
                    let expr = decode_hex_name(hexpr)?;
                    use core::fmt::Write;
                    let _ = write!(check.expr, "{expr}");
                    if check.expr.is_truncated() {
                        return Err(CheckpointSetupError::Corrupt("chk predicate too long"));
                    }
                    let i = def.n_checks;
                    def.checks[i] = check;
                    def.n_checks += 1;
                }
                Some("fkey") => {
                    let Some((_, def, _)) = pending_def.as_mut() else {
                        return Err(CheckpointSetupError::Corrupt("fkey outside table"));
                    };
                    if def.n_fkeys >= crate::storage::MAX_FKEYS {
                        return Err(CheckpointSetupError::Corrupt("too many fkey lines"));
                    }
                    let n_cols: usize = parse_field(words.next(), "fkey ncols")?;
                    if n_cols == 0 || n_cols > crate::storage::MAX_INDEX_COLS {
                        return Err(CheckpointSetupError::Corrupt("bad fkey ncols"));
                    }
                    let mut fk = crate::storage::ForeignKey::EMPTY;
                    fk.n_cols = n_cols;
                    for c in fk.cols.iter_mut().take(n_cols) {
                        *c = parse_field(words.next(), "fkey col")?;
                    }
                    let n_parent: usize = parse_field(words.next(), "fkey nparent")?;
                    if n_parent == 0 || n_parent > crate::storage::MAX_INDEX_COLS {
                        return Err(CheckpointSetupError::Corrupt("bad fkey nparent"));
                    }
                    fk.n_parent_cols = n_parent;
                    for c in fk.parent_cols.iter_mut().take(n_parent) {
                        *c = parse_field(words.next(), "fkey pcol")?;
                    }
                    let od: u8 = parse_field(words.next(), "fkey on_delete")?;
                    let ou: u8 = parse_field(words.next(), "fkey on_update")?;
                    fk.on_delete = crate::storage::FkAction::from_code(od)
                        .ok_or(CheckpointSetupError::Corrupt("bad fkey on_delete"))?;
                    fk.on_update = crate::storage::FkAction::from_code(ou)
                        .ok_or(CheckpointSetupError::Corrupt("bad fkey on_update"))?;
                    let hname = words
                        .next()
                        .ok_or(CheckpointSetupError::Corrupt("fkey name missing"))?;
                    let hparent = words
                        .next()
                        .ok_or(CheckpointSetupError::Corrupt("fkey parent missing"))?;
                    fk.name = sql_name(&decode_hex_name(hname)?)?;
                    fk.parent = sql_name(&decode_hex_name(hparent)?)?;
                    let i = def.n_fkeys;
                    def.fkeys[i] = fk;
                    def.n_fkeys += 1;
                }
                Some("sst") => {
                    finish_pending(storage, &mut slot_of, pending_def.take())?;
                    let key = words
                        .next()
                        .ok_or(CheckpointSetupError::Corrupt("sst key missing"))?
                        .to_string();
                    let mindex: usize = parse_field(words.next(), "sst table")?;
                    let count: u64 = parse_field(words.next(), "sst count")?;
                    let bytes: u64 = parse_field(words.next(), "sst bytes")?;
                    let crc: u32 = parse_field(words.next(), "sst crc")?;
                    ssts.push((key, mindex, count, bytes, crc));
                }
                Some("view") => {
                    finish_pending(storage, &mut slot_of, pending_def.take())?;
                    let hex = words
                        .next()
                        .ok_or(CheckpointSetupError::Corrupt("view sql missing"))?;
                    if hex.len() % 2 != 0 || hex.len() / 2 > crate::storage::VIEW_SQL_MAX {
                        return Err(CheckpointSetupError::Corrupt("bad view sql"));
                    }
                    let mut bytes = Vec::with_capacity(hex.len() / 2);
                    for i in 0..hex.len() / 2 {
                        bytes.push(
                            u8::from_str_radix(&hex[i * 2..i * 2 + 2], 16)
                                .map_err(|_| CheckpointSetupError::Corrupt("bad view sql hex"))?,
                        );
                    }
                    let sql = String::from_utf8(bytes)
                        .map_err(|_| CheckpointSetupError::Corrupt("view sql not UTF-8"))?;
                    let name = rest_of(line, 2)?;
                    let mut buf = StackStr::<{ crate::storage::VIEW_SQL_MAX }>::new();
                    use core::fmt::Write;
                    let _ = write!(buf, "{sql}");
                    storage
                        .create_view(sql_name(name)?, buf, true)
                        .map_err(|e| {
                            CheckpointSetupError::S3(format!(
                                "manifest view rejected: {}",
                                e.message.as_str()
                            ))
                        })?;
                }
                Some("idx") => {
                    finish_pending(storage, &mut slot_of, pending_def.take())?;
                    let unique: u8 = parse_field(words.next(), "idx unique")?;
                    let n_cols: usize = parse_field(words.next(), "idx ncols")?;
                    if n_cols == 0 || n_cols > crate::storage::MAX_INDEX_COLS {
                        return Err(CheckpointSetupError::Corrupt("bad index ncols"));
                    }
                    let mut cols = [0u16; crate::storage::MAX_INDEX_COLS];
                    for c in cols.iter_mut().take(n_cols) {
                        *c = parse_field(words.next(), "idx col")?;
                    }
                    let hname = words
                        .next()
                        .ok_or(CheckpointSetupError::Corrupt("idx name missing"))?;
                    let htable = words
                        .next()
                        .ok_or(CheckpointSetupError::Corrupt("idx table missing"))?;
                    let name = decode_hex_name(hname)?;
                    let table = decode_hex_name(htable)?;
                    storage
                        .create_index(crate::storage::IndexDef {
                            name: sql_name(&name)?,
                            table: sql_name(&table)?,
                            cols,
                            n_cols,
                            unique: unique != 0,
                            live: true,
                        })
                        .map_err(|e| {
                            CheckpointSetupError::S3(format!(
                                "manifest index rejected: {}",
                                e.message.as_str()
                            ))
                        })?;
                }
                Some("end") => {
                    finish_pending(storage, &mut slot_of, pending_def.take())?;
                    saw_end = true;
                }
                Some("") | None => {}
                Some(other) => {
                    return Err(CheckpointSetupError::S3(format!(
                        "unknown manifest line '{other}'"
                    )));
                }
            }
        }
        if !saw_end {
            return Err(CheckpointSetupError::Corrupt("manifest truncated (no end)"));
        }

        for (key, mindex, count, bytes, crc) in &ssts {
            let slot = slot_of
                .get(*mindex)
                .copied()
                .flatten()
                .ok_or(CheckpointSetupError::Corrupt("sst references unknown table"))?;
            self.rehydrate_sst(storage, key, slot, *count, *bytes, *crc)?;
            // Remember the SST so a later delta checkpoint can carry it.
            if self.prev_ssts.len() <= slot {
                self.prev_ssts.resize(slot + 1, None);
            }
            self.prev_ssts[slot] = Some(PrevSst {
                key: crate::stack_format!(64, "{}", key),
                count: *count,
                bytes: *bytes,
                crc: *crc,
            });
            self.referenced.push(crate::stack_format!(64, "{}", key));
        }

        storage.set_lsn(lsn);
        if next_rowid > 0 {
            storage.observe_rowid(next_rowid - 1);
        }
        self.manifest_lsn = lsn;
        Ok(lsn)
    }

    fn rehydrate_sst(
        &mut self,
        storage: &mut Storage,
        key: &str,
        slot: usize,
        expect_count: u64,
        total_bytes: u64,
        expect_crc: u32,
    ) -> Result<(), CheckpointSetupError> {
        let corrupt = |what: &'static str| CheckpointSetupError::Corrupt(what);
        if total_bytes < SST_FOOTER_LEN as u64 {
            return Err(corrupt("sst smaller than its footer"));
        }
        let entries_end = total_bytes - SST_FOOTER_LEN as u64;

        // Footer first.
        self.client
            .get(key, Some((entries_end, total_bytes - 1)))
            .map_err(|e| CheckpointSetupError::S3(format!("sst footer: {e}")))?;
        let f = self.client.body_bytes();
        if f.len() != SST_FOOTER_LEN {
            return Err(corrupt("sst footer short"));
        }
        let count = u64::from_le_bytes(f[0..8].try_into().unwrap());
        let crc_stored = u32::from_le_bytes(f[8..12].try_into().unwrap());
        let magic = u64::from_le_bytes(f[12..20].try_into().unwrap());
        if magic != SST_MAGIC || count != expect_count || crc_stored != expect_crc {
            return Err(corrupt("sst footer mismatch with manifest"));
        }

        let mut crc = Crc32c::new();
        let mut offset = 0u64;
        let mut seen = 0u64;
        while offset < entries_end {
            let to = (offset + self.client.response_capacity() as u64 - 1).min(entries_end - 1);
            self.client
                .get(key, Some((offset, to)))
                .map_err(|e| CheckpointSetupError::S3(format!("sst read: {e}")))?;
            // Parse complete entries; partially fetched ones re-fetch from
            // their start on the next round.
            let mut consumed = 0usize;
            loop {
                let data = &self.client.body_bytes()[consumed..];
                if data.len() < SST_ENTRY_HEADER {
                    break;
                }
                let rowid = u64::from_le_bytes(data[0..8].try_into().unwrap());
                let len = u32::from_le_bytes(data[8..12].try_into().unwrap()) as usize;
                if data.len() < SST_ENTRY_HEADER + len {
                    break;
                }
                let row = &data[SST_ENTRY_HEADER..SST_ENTRY_HEADER + len];
                crc.update(&data[..SST_ENTRY_HEADER + len]);
                let (loc, slice) = storage
                    .heap
                    .append(len)
                    .map_err(|e| CheckpointSetupError::S3(format!(
                        "rehydrate: {}",
                        e.message.as_str()
                    )))?;
                slice.copy_from_slice(row);
                storage.observe_rowid(rowid);
                storage
                    .table_mut(slot)
                    .rows
                    .insert(rowid, crate::storage::RowState::committed_only(loc))
                    .map_err(|_| corrupt("sst rows exceed table_rows"))?;
                seen += 1;
                consumed += SST_ENTRY_HEADER + len;
            }
            if consumed == 0 {
                return Err(corrupt("sst entry larger than the response buffer"));
            }
            offset += consumed as u64;
        }
        if seen != count || crc.finish() != crc_stored {
            return Err(corrupt("sst content does not match its footer"));
        }
        Ok(())
    }

    /// Uploads a full snapshot and publishes it. The caller resets the WAL
    /// and compacts the heap afterwards. No-op when nothing changed.
    pub fn checkpoint(
        &mut self,
        storage: &Storage,
        sort_scratch: &mut FixedVec<(u64, RowLoc)>,
    ) -> Result<bool, SqlError> {
        let lsn = storage.lsn();
        if lsn == self.manifest_lsn && self.manifest_etag.is_some() {
            return Ok(false);
        }

        // Manifest is assembled as SSTs upload. Delta bookkeeping collects
        // the new per-slot references and GC keep-set into pre-reserved
        // scratch so this post-freeze path never allocates.
        self.prev_scratch.clear();
        self.ref_scratch.clear();
        self.manifest_buf.clear();
        write_manifest(&mut self.manifest_buf, MANIFEST_HEADER)?;
        write_manifest(&mut self.manifest_buf, format_args!("lsn {lsn}"))?;
        write_manifest(
            &mut self.manifest_buf,
            format_args!("next_rowid {}", storage.peek_next_rowid()),
        )?;

        for slot in 0..storage.table_count() {
            let table = storage.table(slot);
            if !table.live {
                continue;
            }
            // Table + columns into the manifest.
            write_manifest(
                &mut self.manifest_buf,
                format_args!(
                    "table {slot} {} {}",
                    table.def.n_columns,
                    table.def.name.as_str()
                ),
            )?;
            for c in table.def.columns() {
                let default_hex = default_to_hex(&c.default_value);
                let flags = u8::from(c.not_null)
                    | (u8::from(c.unique) << 1)
                    | (u8::from(c.primary) << 2)
                    | (u8::from(c.auto_increment) << 3);
                write_manifest(
                    &mut self.manifest_buf,
                    format_args!(
                        "col {} {} {} {} {}",
                        col_type_code(c.ctype),
                        flags,
                        c.type_mod,
                        default_hex.as_str(),
                        c.name.as_str()
                    ),
                )?;
            }
            // Constraint lines (hex-encoded names/text tolerate spaces):
            // `ukey <is_primary> <ncols> <c0..cN> <hex-name>`
            for uk in table.def.uniques() {
                use core::fmt::Write;
                let mut cols = StackStr::<64>::new();
                for c in uk.cols() {
                    let _ = write!(cols, "{c} ");
                }
                let mut hname = StackStr::<130>::new();
                for b in uk.name.as_str().as_bytes() {
                    let _ = write!(hname, "{b:02x}");
                }
                write_manifest(
                    &mut self.manifest_buf,
                    format_args!(
                        "ukey {} {} {}{}",
                        u8::from(uk.is_primary),
                        uk.n_cols,
                        cols.as_str(),
                        hname.as_str()
                    ),
                )?;
            }
            // `chk <hex-name> <hex-predicate>`
            for check in table.def.checks() {
                use core::fmt::Write;
                let mut hname = StackStr::<130>::new();
                for b in check.name.as_str().as_bytes() {
                    let _ = write!(hname, "{b:02x}");
                }
                let mut hexpr = StackStr::<{ 2 * crate::storage::CHECK_SQL_MAX }>::new();
                for b in check.expr.as_str().as_bytes() {
                    let _ = write!(hexpr, "{b:02x}");
                }
                write_manifest(
                    &mut self.manifest_buf,
                    format_args!("chk {} {}", hname.as_str(), hexpr.as_str()),
                )?;
            }
            // `fkey <ncols> <c..> <nparent> <p..> <on_delete> <on_update> <hex-name> <hex-parent>`
            for fk in table.def.fkeys() {
                use core::fmt::Write;
                let mut cols = StackStr::<64>::new();
                for c in fk.cols() {
                    let _ = write!(cols, "{c} ");
                }
                let mut pcols = StackStr::<64>::new();
                for c in fk.parent_cols() {
                    let _ = write!(pcols, "{c} ");
                }
                let mut hname = StackStr::<130>::new();
                for b in fk.name.as_str().as_bytes() {
                    let _ = write!(hname, "{b:02x}");
                }
                let mut hparent = StackStr::<130>::new();
                for b in fk.parent.as_str().as_bytes() {
                    let _ = write!(hparent, "{b:02x}");
                }
                write_manifest(
                    &mut self.manifest_buf,
                    format_args!(
                        "fkey {} {}{} {}{} {} {} {}",
                        fk.n_cols,
                        cols.as_str(),
                        fk.n_parent_cols,
                        pcols.as_str(),
                        fk.on_delete.code(),
                        fk.on_update.code(),
                        hname.as_str(),
                        hparent.as_str()
                    ),
                )?;
            }

            // Sort rows by rowid; snapshots contain committed images only.
            sort_scratch.clear();
            for (&rowid, state) in table.rows.iter() {
                let Some(loc) = state.committed else {
                    continue;
                };
                sort_scratch.push((rowid, loc)).map_err(|e| {
                    sql_err!(sqlstate::PROGRAM_LIMIT_EXCEEDED, "checkpoint scratch: {}", e)
                })?;
            }
            sort_scratch.as_mut_slice().sort_unstable_by_key(|(rowid, _)| *rowid);

            // Pass 1: length, SHA-256 (for SigV4), CRC (for the footer).
            let mut sha = Sha256::new();
            let mut crc = Crc32c::new();
            let mut body_len = 0u64;
            for &(rowid, loc) in sort_scratch.iter() {
                let row = storage.heap.get(loc);
                let mut header = [0u8; SST_ENTRY_HEADER];
                header[0..8].copy_from_slice(&rowid.to_le_bytes());
                header[8..12].copy_from_slice(&(row.len() as u32).to_le_bytes());
                sha.update(&header);
                sha.update(row);
                crc.update(&header);
                crc.update(row);
                body_len += (SST_ENTRY_HEADER + row.len()) as u64;
            }
            let crc = crc.finish();
            let count = sort_scratch.len() as u64;
            let mut footer = [0u8; SST_FOOTER_LEN];
            footer[0..8].copy_from_slice(&count.to_le_bytes());
            footer[8..12].copy_from_slice(&crc.to_le_bytes());
            footer[12..20].copy_from_slice(&SST_MAGIC.to_le_bytes());
            sha.update(&footer);
            let total_len = body_len + SST_FOOTER_LEN as u64;
            let sha_hex = HexDigest::of(&sha.finish());

            // Delta: a clean table whose prior SST still describes the same
            // rows is carried forward by reference instead of re-uploaded.
            let prev = self.prev_ssts.get(slot).copied().flatten();
            let reuse = !storage.table(slot).dirty
                && prev.is_some_and(|p| p.count == count && p.crc == crc);
            let (key, ref_count, ref_bytes, ref_crc) = if reuse {
                let p = prev.expect("reuse implies prev");
                (p.key, p.count, p.bytes, p.crc)
            } else {
                let key = stack_format!(64, "sst/{:020}/{:04}.sst", lsn, slot);
                let heap = &storage.heap;
                let rows = sort_scratch.as_slice();
                self.client
                    .put_streamed(
                        key.as_str(),
                        total_len,
                        sha_hex.as_str(),
                        Precondition::None,
                        |stream| {
                            let mut w = ChunkedWriter::new(stream);
                            for &(rowid, loc) in rows {
                                let row = heap.get(loc);
                                let mut header = [0u8; SST_ENTRY_HEADER];
                                header[0..8].copy_from_slice(&rowid.to_le_bytes());
                                header[8..12]
                                    .copy_from_slice(&(row.len() as u32).to_le_bytes());
                                w.write_all(&header)?;
                                w.write_all(row)?;
                            }
                            w.write_all(&footer)?;
                            w.flush()
                        },
                    )
                    .map_err(s3_to_sql)?;
                (key, count, total_len, crc)
            };

            if self.prev_scratch.len() <= slot && self.prev_scratch.len() < MAX_CKPT_TABLES {
                self.prev_scratch.resize(slot + 1, None);
            }
            if slot < self.prev_scratch.len() {
                self.prev_scratch[slot] = Some(PrevSst {
                    key,
                    count: ref_count,
                    bytes: ref_bytes,
                    crc: ref_crc,
                });
            }
            if self.ref_scratch.len() < MAX_SWEEP_KEYS {
                self.ref_scratch.push(key);
            }
            write_manifest(
                &mut self.manifest_buf,
                format_args!(
                    "sst {} {slot} {ref_count} {ref_bytes} {ref_crc}",
                    key.as_str()
                ),
            )?;
        }
        // Views: `view <hex-of-SELECT-text> <name>` (name is rest-of-line, so
        // it may contain spaces; the SELECT is hex so it survives the format).
        for (name, sql) in storage.live_views() {
            let mut hex = StackStr::<{ 2 * crate::storage::VIEW_SQL_MAX }>::new();
            use core::fmt::Write;
            for b in sql.as_bytes() {
                let _ = write!(hex, "{b:02x}");
            }
            write_manifest(
                &mut self.manifest_buf,
                format_args!("view {} {name}", hex.as_str()),
            )?;
        }
        // Indexes: `idx <unique> <ncols> <c0..cN> <hex-name> <hex-table>`.
        for idx in storage.live_indexes() {
            use core::fmt::Write;
            let mut cols = StackStr::<128>::new();
            for c in &idx.cols[..idx.n_cols] {
                let _ = write!(cols, "{c} ");
            }
            let mut hname = StackStr::<130>::new();
            for b in idx.name.as_str().as_bytes() {
                let _ = write!(hname, "{b:02x}");
            }
            let mut htable = StackStr::<130>::new();
            for b in idx.table.as_str().as_bytes() {
                let _ = write!(htable, "{b:02x}");
            }
            write_manifest(
                &mut self.manifest_buf,
                format_args!(
                    "idx {} {} {}{} {}",
                    u8::from(idx.unique),
                    idx.n_cols,
                    cols.as_str(),
                    hname.as_str(),
                    htable.as_str()
                ),
            )?;
        }
        write_manifest(&mut self.manifest_buf, "end")?;

        // Publish via CAS.
        let precondition = match &self.manifest_etag {
            Some(etag) => Precondition::IfMatch(etag.as_str()),
            None => Precondition::IfNoneMatchAny,
        };
        let etag = self
            .client
            .put(MANIFEST_KEY, self.manifest_buf.readable(), precondition)
            .map_err(|e| {
                if e.is_precondition_failed() {
                    sql_err!(
                        SQLSTATE_CAS,
                        "manifest compare-and-swap failed: another writer owns this bucket"
                    )
                } else {
                    s3_to_sql(e)
                }
            })?;
        self.manifest_etag = Some(etag);
        self.manifest_lsn = lsn;
        std::mem::swap(&mut self.prev_ssts, &mut self.prev_scratch);
        std::mem::swap(&mut self.referenced, &mut self.ref_scratch);

        // GC: delete any SST under sst/ not referenced by the new manifest.
        self.collect_garbage()?;
        Ok(true)
    }

    fn collect_garbage(&mut self) -> Result<(), SqlError> {
        // Two passes because list borrows the client: collect keys first
        // into pre-reserved scratch (no allocation post-freeze).
        self.doomed_scratch.clear();
        let referenced = &self.referenced;
        let doomed = &mut self.doomed_scratch;
        let mut overflow = false;
        self.client
            .list("sst/", |key| {
                if !referenced.iter().any(|r| r.as_str() == key) {
                    if doomed.len() < MAX_SWEEP_KEYS {
                        doomed.push(crate::stack_format!(64, "{}", key));
                    } else {
                        overflow = true;
                    }
                }
            })
            .map_err(s3_to_sql)?;
        for i in 0..self.doomed_scratch.len() {
            let key = self.doomed_scratch[i];
            self.client.delete(key.as_str()).map_err(s3_to_sql)?;
        }
        if overflow {
            eprintln!("pos3ql: sst garbage exceeds one sweep; continuing next checkpoint");
        }
        Ok(())
    }
}

/// Fixed-stack buffered writer so streaming an SST does not issue one
/// syscall per row.
struct ChunkedWriter<'a> {
    stream: &'a mut std::net::TcpStream,
    buf: [u8; 16384],
    len: usize,
}

impl<'a> ChunkedWriter<'a> {
    fn new(stream: &'a mut std::net::TcpStream) -> Self {
        Self {
            stream,
            buf: [0; 16384],
            len: 0,
        }
    }

    fn write_all(&mut self, mut data: &[u8]) -> std::io::Result<()> {
        while !data.is_empty() {
            if self.len == self.buf.len() {
                self.flush_buf()?;
            }
            let take = data.len().min(self.buf.len() - self.len);
            self.buf[self.len..self.len + take].copy_from_slice(&data[..take]);
            self.len += take;
            data = &data[take..];
        }
        Ok(())
    }

    fn flush_buf(&mut self) -> std::io::Result<()> {
        self.stream.write_all(&self.buf[..self.len])?;
        self.len = 0;
        Ok(())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.flush_buf()
    }
}

fn write_manifest(buf: &mut FixedBuf, line: impl core::fmt::Display) -> Result<(), SqlError> {
    use core::fmt::Write;
    writeln!(buf, "{line}").map_err(|_| {
        sql_err!(
            sqlstate::PROGRAM_LIMIT_EXCEEDED,
            "manifest exceeds its fixed buffer"
        )
    })
}

fn s3_to_sql(e: S3Error) -> SqlError {
    sql_err!(SQLSTATE_IO, "{}", e)
}

/// Parses framed WAL records from an uploaded segment (same layout as the
/// local journal: crc u32 | len u32 | lsn u64 | payload) and applies each
/// with lsn > floor. Returns the highest LSN seen.
fn replay_segment_bytes(
    bytes: &[u8],
    floor: u64,
    apply: &mut impl FnMut(u64, &[u8]) -> Result<(), SqlError>,
) -> Result<u64, SqlError> {
    const HEADER_LEN: usize = 24;
    let mut at = 0usize;
    let mut last = floor;
    while at + HEADER_LEN <= bytes.len() {
        let stored_crc = u32::from_le_bytes(bytes[at..at + 4].try_into().unwrap());
        let payload_len = u32::from_le_bytes(bytes[at + 4..at + 8].try_into().unwrap()) as usize;
        let lsn = u64::from_le_bytes(bytes[at + 8..at + 16].try_into().unwrap());
        let total = HEADER_LEN + payload_len;
        if at + total > bytes.len() {
            break;
        }
        if crate::wal::crc32c::crc32c(&bytes[at + 4..at + total]) != stored_crc {
            break;
        }
        if lsn > floor {
            // Hand over from the kind byte (offset 16) to end of record;
            // decode_record skips the kind + 7 pad bytes.
            apply(lsn, &bytes[at + 16..at + total])?;
            if lsn > last {
                last = lsn;
            }
        }
        at += total;
    }
    Ok(last)
}

#[derive(Debug)]
pub enum CheckpointSetupError {
    Budget(BudgetError),
    Credentials(&'static str),
    S3(String),
    Corrupt(&'static str),
    Replay(SqlError),
}

impl std::fmt::Display for CheckpointSetupError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Budget(e) => write!(f, "checkpoint: {e}"),
            Self::Credentials(what) =>

                write!(f, "s3 is enabled but no credentials were provided ({what})"),
            Self::S3(what) => write!(f, "checkpoint: {what}"),
            Self::Corrupt(what) => write!(f, "checkpoint: corrupt bucket state: {what}"),
            Self::Replay(e) => write!(f, "checkpoint: wal replay failed: {}", e.message.as_str()),
        }
    }
}

impl std::error::Error for CheckpointSetupError {}

fn parse_field<T: core::str::FromStr>(
    word: Option<&str>,
    what: &'static str,
) -> Result<T, CheckpointSetupError> {
    word.and_then(|w| w.parse().ok())
        .ok_or(CheckpointSetupError::Corrupt(what))
}

/// The name is everything after the first `skip` space-separated fields.
fn rest_of(line: &str, skip: usize) -> Result<&str, CheckpointSetupError> {
    let mut at = 0;
    let mut seen = 0;
    for (i, b) in line.bytes().enumerate() {
        if b == b' ' {
            seen += 1;
            if seen == skip {
                at = i + 1;
                break;
            }
        }
    }
    if seen < skip {
        return Err(CheckpointSetupError::Corrupt("truncated manifest line"));
    }
    Ok(&line[at..])
}

/// Decodes a hex-encoded identifier from the manifest (startup only, so the
/// allocation is fine).
fn decode_hex_name(hex: &str) -> Result<String, CheckpointSetupError> {
    if !hex.len().is_multiple_of(2) {
        return Err(CheckpointSetupError::Corrupt("odd-length hex name"));
    }
    let mut bytes = Vec::with_capacity(hex.len() / 2);
    for i in 0..hex.len() / 2 {
        bytes.push(
            u8::from_str_radix(&hex[i * 2..i * 2 + 2], 16)
                .map_err(|_| CheckpointSetupError::Corrupt("bad hex name"))?,
        );
    }
    String::from_utf8(bytes).map_err(|_| CheckpointSetupError::Corrupt("hex name not UTF-8"))
}

fn sql_name(s: &str) -> Result<SqlName, CheckpointSetupError> {
    SqlName::parse(s).map_err(|_| CheckpointSetupError::Corrupt("name too long in manifest"))
}

fn empty_column() -> ColumnMeta {
    ColumnMeta {
        name: SqlName::parse("").expect("empty fits"),
        ctype: ColType::Bool,
        type_mod: -1,
        not_null: false,
        unique: false,
        primary: false,
        auto_increment: false,
        default_value: None,
    }
}

/// Column defaults travel in the manifest as hex of the WAL default
/// encoding ("-" for none-with-no-bytes readability).
fn default_to_hex(d: &Option<OwnedDatum>) -> StackStr<128> {
    let mut scratch = [0u8; crate::wal::MAX_DEFAULT_ENCODED];
    let n = crate::wal::encode_default_bytes(d, &mut scratch);
    let mut out = StackStr::<128>::new();
    use core::fmt::Write;
    for b in &scratch[..n] {
        let _ = write!(out, "{b:02x}");
    }
    out
}

fn default_from_hex(hex: &str) -> Result<Option<OwnedDatum>, CheckpointSetupError> {
    let corrupt = || CheckpointSetupError::Corrupt("bad default encoding");
    if !hex.len().is_multiple_of(2) || hex.len() > 256 {
        return Err(corrupt());
    }
    let mut bytes = [0u8; 128];
    let n = hex.len() / 2;
    for i in 0..n {
        bytes[i] =
            u8::from_str_radix(&hex[i * 2..i * 2 + 2], 16).map_err(|_| corrupt())?;
    }
    let mut at = 0usize;
    let d = crate::wal::decode_default(&bytes[..n], &mut at).ok_or_else(corrupt)?;
    if at != n {
        return Err(corrupt());
    }
    Ok(d)
}

fn col_type_code(t: ColType) -> u8 {
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

fn col_type_from_code(code: u8) -> Result<ColType, CheckpointSetupError> {
    Ok(match code {
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
        c if c >= 32 => crate::sql::types::ArrElem::from_code(c - 32).map(ColType::Array).ok_or(CheckpointSetupError::Corrupt("bad array element code"))?,
        9 => ColType::Uuid,
        10 => ColType::Bytea,
        11 => ColType::Numeric,
        _ => return Err(CheckpointSetupError::Corrupt("unknown column type code")),
    })
}
